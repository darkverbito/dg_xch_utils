// In-process capstone. Boot the daemon against a fixture-seeded loopback peer, sync a range,
// serve a RequestBlock(s) to a peer, answer get_blockchain_state over the real TLS RPC server, and deliver a
// wallet CoinStateUpdate — the whole node wired together, minus the live-mainnet run (the production deploy).

mod common;

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use full_node::{Backend, Config, Node};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::RwLock;

static DBN: AtomicU64 = AtomicU64::new(0);

fn config(listen: SocketAddr, rpc: SocketAddr) -> Config {
    let n = DBN.fetch_add(1, Ordering::Relaxed);
    let db = std::env::temp_dir().join(format!(
        "full_node_daemon_{}_{n}.sqlite",
        std::process::id()
    ));
    Config {
        listen,
        rpc,
        introducer: None,
        manual_peers: Vec::new(),
        advertise: None,
        backend: Backend::Sqlite(db),
        network_id: "mainnet".to_string(),
        metrics: None,
        capture_dir: None,
        genesis_sync: false,
        sync_from: 0,
        uncompact: false,
        prefetch_memory_mb: None,
        prefetch_max_inflight: None,
        trusted_peers: Vec::new(),
        trusted_cidrs: Vec::new(),
    }
}

fn free_addr() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    l.local_addr().expect("addr")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_boots_syncs_serves_and_answers_rpc_and_wallet() {
    // ---- a loopback peer serving real mainnet block 5000000 ----
    let block = common::full_block();
    let peer_api = Arc::new(common::MapApi {
        blocks: RwLock::new(HashMap::from([(common::PEAK_HEIGHT, block.clone())])),
    });
    let (peer_port, peer_run) = common::spawn_serving_node(peer_api).await;

    // ---- boot the daemon (empty store) ----
    let listen = free_addr();
    let rpc_addr = free_addr();
    let node = Arc::new(Node::boot(config(listen, rpc_addr)).await.expect("boot"));

    // A wallet subscribes to a puzzle hash present in block 5000000's additions, before the sync.
    let adds = common::additions();
    let watched = adds
        .iter()
        .find(|c| !c.coinbase)
        .map(|c| c.coin.puzzle_hash)
        .expect("a non-coinbase addition");
    let wallet_peer = Bytes32::from([0x5a; 32]);
    let mut wallet_rx = node
        .wallet
        .register_for_ph_updates(wallet_peer, None, &[watched])
        .await
        .expect("register")
        .0
        .expect("receiver");

    // ---- boot -> sync: follow the peer's tip, confirming block 5000000 into our store ----
    let source = common::dial_source("127.0.0.1", peer_port).await;
    let peak = node
        .sync_follow(&source, common::PEAK_HEIGHT, common::PEAK_HEIGHT)
        .await
        .expect("sync");
    assert_eq!(
        peak,
        Some((block.header_hash().unwrap(), common::PEAK_HEIGHT)),
        "daemon synced to the correct peak"
    );

    // ---- wallet CoinStateUpdate delivered for the subscribed puzzle hash ----
    let update = tokio::time::timeout(Duration::from_secs(2), wallet_rx.recv())
        .await
        .expect("wallet update in time")
        .expect("update present");
    assert_eq!(update.height, common::PEAK_HEIGHT);
    assert!(
        update
            .items
            .iter()
            .any(|cs| cs.coin.puzzle_hash == watched
                && cs.created_height == Some(common::PEAK_HEIGHT)),
        "subscribed puzzle hash received its created coin"
    );

    // ---- serve a RequestBlock(s) to a peer: our own peer server hands block 5000000 to a dialer ----
    let (serve_run, inbound_peers) = node.spawn_peer_server().expect("peer server");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let puller = common::dial_source("127.0.0.1", listen.port()).await;
    let served = puller
        .fetch_range(common::PEAK_HEIGHT, common::PEAK_HEIGHT)
        .await
        .expect("peer served the block");
    assert_eq!(served.len(), 1);
    // Live-socket confirmation the inbound session is gauged: the connected dialer occupies
    // exactly one entry in the server's inbound map — the collection the leak fix bounds and the new
    // `fullnode_inbound_connections` gauge exposes. (Deterministic teardown-on-close is proven by
    // dg_xch_servers::websocket::peer_map_tests, which do not depend on drop timing.)
    assert_eq!(
        inbound_peers.read().await.len(),
        1,
        "the connected dialer occupies exactly one inbound session"
    );
    assert_eq!(
        served[0].header_hash().unwrap(),
        block.header_hash().unwrap()
    );

    // ---- answer get_blockchain_state over the real TLS RPC server ----
    let rpc_run = node.spawn_rpc_server().expect("rpc server");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let envelope = rpc_get_blockchain_state(rpc_addr).await;
    assert_eq!(
        envelope["success"].as_bool(),
        Some(true),
        "chia envelope: success stamped"
    );
    let state = &envelope["blockchain_state"];
    assert_eq!(
        state["peak"]["height"].as_u64(),
        Some(u64::from(common::PEAK_HEIGHT)),
        "RPC reports the synced peak"
    );
    // The fixture peak is HISTORICAL (its tx-block timestamp is years old), and synced carries
    // chia's semantic — `FullNode.synced` (chia full_node.py:930-948): current iff the last tx
    // block's timestamp is within 7 minutes. Chia's own get_blockchain_state reports false for a
    // stale-tip chain; so do we. (The synced=true case with a fresh-timestamped peak is covered by
    // announce_pull.rs::synced_flag_requires_a_current_chain_tip.)
    assert_eq!(state["sync"]["synced"].as_bool(), Some(false));

    // ---- drain ----
    serve_run.store(false, Ordering::Relaxed);
    rpc_run.store(false, Ordering::Relaxed);
    peer_run.store(false, Ordering::Relaxed);
}

// A Chia-cert-model TLS client (server cert unvalidated, CLIENT CERT PRESENTED — the daemon's
// RPC posture requires a certificate chained to the CA) that issues a raw
// HTTP/1.1 GET and returns the parsed JSON body.
async fn rpc_get_blockchain_state(addr: SocketAddr) -> serde_json::Value {
    use dg_xch_core::constants::{CHIA_CA_CRT, CHIA_CA_KEY};
    use dg_xch_core::ssl::{
        generate_ca_signed_cert_data, load_certs_from_bytes, load_private_key_from_bytes,
    };
    use tokio_rustls::TlsConnector;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (crt, key_bytes) =
        generate_ca_signed_cert_data(CHIA_CA_CRT.as_bytes(), CHIA_CA_KEY.as_bytes())
            .expect("client cert");
    let certs = load_certs_from_bytes(&crt).expect("certs");
    let key = load_private_key_from_bytes(&key_bytes).expect("key");
    let verifier = Arc::new(dg_xch_core::protocols::shared::NoCertificateVerification);
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .expect("client auth");
    let connector = TlsConnector::from(Arc::new(cfg));
    let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("server name");
    let mut tls = connector.connect(server_name, tcp).await.expect("tls");

    let req = "GET /get_blockchain_state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    tls.write_all(req.as_bytes()).await.expect("write");

    // Read to close (Connection: close); tolerate an abrupt EOF without a TLS close_notify.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tls.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let body = text.split_once("\r\n\r\n").map_or("", |x| x.1);
    serde_json::from_str(body.trim()).expect("json body")
}
