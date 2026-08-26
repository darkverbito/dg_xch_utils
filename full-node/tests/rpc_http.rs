// RPC parity — the chia HTTP envelope and the 8555 TLS posture.
//
// Envelope: every response is `{"<named_key>": ..., "success": true}` and every application
// error is an HTTP-200 `{"success": false, "error": ...}` (chia/rpc/util.py:74-97). The
// envelope tests drive `NodeRpcHandler` directly; the shape assertions deserialize through
// dg_xch_clients' REAL chia response wrappers (the exact structs a chia-shaped Rust client
// parses).
//
// TLS: `build_rpc_tls_context` serves the chia 8555 posture (chia server.py:54-71) — server
// cert generated from the CA chain, client certificate REQUIRED and verified against that CA.
// The end-to-end tests run the real `RpcServer` accept loop and connect with the real
// `FullnodeClient` (chia-tooling shape: client certs + https + envelope parse); the negative
// tests prove a no-cert client and a wrong-CA client are refused at the handshake.

mod common;

use bytes::Bytes;
use dg_xch_clients::ClientSSLConfig;
use dg_xch_clients::api::responses::full_node_responses::{
    BlockRecordResp, CoinRecordAryResp, FullBlockResp, TXResp,
};
use dg_xch_clients::rpc::full_node::{FullnodeAPI, FullnodeClient};
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::tx_status::TXStatus;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::constants::{CHIA_CA_CRT, CHIA_CA_KEY};
use dg_xch_core::ssl::generate_ca_signed_cert_data;
use dg_xch_node::Mempool;
use dg_xch_servers::rpc::{RequestType, RpcHandler, RpcRequest, RpcServer, RpcServerConfig};
use dg_xch_stores::SqliteStore;
use full_node::{NodeRpc, NodeRpcHandler, build_rpc_tls_context};
use http::HeaderMap;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, StatusCode};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

async fn seeded_rpc() -> (Arc<NodeRpc<SqliteStore>>, Arc<Mutex<Mempool>>) {
    let store = Arc::new(common::open_store().await);
    common::seed_peak(&store).await;
    let mempool = Arc::new(Mutex::new(Mempool::new(&MAINNET)));
    let node = NodeRpc::new(
        store,
        mempool.clone(),
        MAINNET,
        Arc::new(AtomicBool::new(true)),
        Arc::new(Mutex::new(Vec::new())),
    );
    (Arc::new(node), mempool)
}

// Drive the handler exactly as the RpcServer service does: a sized request in, a JSON body out.
async fn call(
    handler: &NodeRpcHandler<SqliteStore>,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    call_raw(handler, path, body.to_string().into_bytes()).await
}

async fn call_raw(
    handler: &NodeRpcHandler<SqliteStore>,
    path: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .body(Full::new(Bytes::from(body)))
        .expect("request");
    let rpc_request = RpcRequest {
        request_type: RequestType::Sized(request),
        response_headers: HeaderMap::new(),
    };
    let response = Response::builder()
        .body(Full::new(Bytes::new()))
        .expect("response");
    let addr: SocketAddr = "127.0.0.1:9999".parse().expect("addr");
    let out = handler
        .handle(rpc_request, response, &addr)
        .await
        .expect("handler never errors at the transport level");
    let status = out.status();
    let bytes = out.into_body().collect().await.expect("body").to_bytes();
    let value = serde_json::from_slice(&bytes).expect("json body");
    (status, value)
}

// ---- envelope shape ---------------------------------------------------------------------------

// Success envelope: `{"blockchain_state": {...}, "success": true}` with chia's full state shape
// (peak as a full block record, sync sub-object, mempool gauges, average_block_time).
#[tokio::test]
async fn envelope_blockchain_state_named_key_and_success() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let (status, v) = call(&handler, "/get_blockchain_state", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["success"], Value::from(true));
    let state = &v["blockchain_state"];
    assert_eq!(
        state["peak"]["height"].as_u64(),
        Some(u64::from(common::PEAK_HEIGHT))
    );
    assert_eq!(state["sync"]["synced"], Value::from(true));
    assert!(state["mempool_size"].is_u64());
    assert!(state["block_max_cost"].is_u64());
    assert!(
        state
            .as_object()
            .expect("obj")
            .contains_key("average_block_time"),
        "chia's average_block_time key is present"
    );
    assert!(state.as_object().expect("obj").contains_key("node_id"));
}

// Error envelope: an application error is HTTP-200 `{"success": false, "error": ...}` with the
// traceback/structuredError keys chia emits (chia/rpc/util.py:86-97) — never an HTTP 4xx/5xx.
#[tokio::test]
async fn envelope_errors_are_http_200_success_false() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let bogus = Bytes32::from([0x42u8; 32]);
    let (status, v) = call(&handler, "/get_block", json!({"header_hash": bogus})).await;
    assert_eq!(status, StatusCode::OK, "app errors are HTTP 200 in chia");
    assert_eq!(v["success"], Value::from(false));
    assert!(
        v["error"]
            .as_str()
            .expect("error string")
            .contains("not found"),
        "the chia error message survives: {v}"
    );
    let obj = v.as_object().expect("obj");
    assert!(obj.contains_key("traceback"));
    assert!(obj.contains_key("structuredError"));

    // A malformed / missing-parameter body is the same envelope.
    let (status, v) = call(&handler, "/get_block", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["success"], Value::from(false));
}

// Unknown endpoints are HTTP 404 (chia's aiohttp router behavior).
#[tokio::test]
async fn unknown_endpoint_is_404() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let (status, v) = call(&handler, "/get_nonexistent", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(v["success"], Value::from(false));
}

// An oversize body is refused with 413 (chia's aiohttp client_max_size = 1 MiB).
#[tokio::test]
async fn oversize_body_is_413() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let body = vec![b'{'; full_node::rpc::MAX_RPC_BODY_BYTES + 1];
    let (status, _v) = call_raw(&handler, "/get_blockchain_state", body).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

// The chia-client-shaped assertion: the coin_records envelope parses through dg_xch_clients'
// real response wrapper (`response["coin_records"]` + `response["success"]`).
#[tokio::test]
async fn envelope_coin_records_parse_as_chia_client_shape() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let name = common::additions()[0].coin.name();
    let (status, v) = call(
        &handler,
        "/get_coin_records_by_names",
        json!({"names": [name]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: CoinRecordAryResp = serde_json::from_value(v).expect("chia client shape");
    assert!(parsed.success);
    assert_eq!(parsed.coin_records.len(), 1);
    assert_eq!(parsed.coin_records[0].coin.name(), name);
}

// get_block / get_block_record envelopes parse through the chia client wrappers.
#[tokio::test]
async fn envelope_block_and_record_parse_as_chia_client_shape() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let hh = common::peak_record().header_hash;
    let (_s, v) = call(&handler, "/get_block", json!({"header_hash": hh})).await;
    let parsed: FullBlockResp = serde_json::from_value(v).expect("block shape");
    assert!(parsed.success);
    assert_eq!(parsed.block.header_hash().expect("hh"), hh);

    let (_s, v) = call(
        &handler,
        "/get_block_record_by_height",
        json!({"height": common::PEAK_HEIGHT}),
    )
    .await;
    let parsed: BlockRecordResp = serde_json::from_value(v).expect("record shape");
    assert!(parsed.success);
    assert_eq!(parsed.block_record.header_hash, hh);
}

// push_tx answers chia's `{"status": "SUCCESS"}`, idempotent on a duplicate; the status parses
// through the chia client's TXResp.
#[tokio::test]
async fn envelope_push_tx_status_success_and_idempotent() {
    let (rpc, mempool) = seeded_rpc().await;
    mempool.lock().await.set_peak(common::PEAK_HEIGHT, 0);
    let coin = common::seed_easy_coin(rpc.store(), 1_000).await;
    let bundle = common::easy_bundle(&coin, 1);
    let handler = NodeRpcHandler::new(rpc);
    let body = json!({"spend_bundle": bundle});
    let (status, v) = call(&handler, "/push_tx", body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: TXResp = serde_json::from_value(v).expect("tx shape");
    assert!(parsed.success);
    assert_eq!(parsed.status, TXStatus::SUCCESS);
    // Duplicate: SUCCESS again (chia full_node_rpc_api.py:846-855), still one resident item.
    let (_s, v) = call(&handler, "/push_tx", body).await;
    let parsed: TXResp = serde_json::from_value(v).expect("tx shape");
    assert_eq!(parsed.status, TXStatus::SUCCESS);
    assert_eq!(mempool.lock().await.len(), 1);
}

// The utility endpoints: healthz, get_routes, get_version, get_network_info,
// get_aggsig_additional_data (plain hex, chia's `.hex()`), get_connections (empty sans live).
#[tokio::test]
async fn envelope_utility_endpoints() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);

    let (status, v) = call(&handler, "/healthz", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v, json!({"success": true}));

    let (_s, v) = call(&handler, "/get_routes", json!({})).await;
    assert_eq!(v["success"], Value::from(true));
    let routes: Vec<String> = serde_json::from_value(v["routes"].clone()).expect("routes list");
    assert!(routes.contains(&"/get_blockchain_state".to_string()));
    assert!(routes.contains(&"/push_tx".to_string()));

    let (_s, v) = call(&handler, "/get_version", json!({})).await;
    assert_eq!(v["version"].as_str(), Some(env!("CARGO_PKG_VERSION")));

    let (_s, v) = call(&handler, "/get_network_info", json!({})).await;
    assert_eq!(v["network_name"].as_str(), Some("mainnet"));
    assert_eq!(v["network_prefix"].as_str(), Some("xch"));
    let genesis = v["genesis_challenge"].as_str().expect("genesis");
    assert!(!genesis.starts_with("0x"), "chia serves plain hex here");

    let (_s, v) = call(&handler, "/get_aggsig_additional_data", json!({})).await;
    let data = v["additional_data"].as_str().expect("additional data");
    assert!(!data.starts_with("0x"), "chia's .hex() has no 0x prefix");
    assert_eq!(
        format!("0x{data}"),
        MAINNET.agg_sig_me_additional_data.to_string()
    );

    let (_s, v) = call(&handler, "/get_connections", json!({})).await;
    assert_eq!(v["success"], Value::from(true));
    assert!(v["connections"].as_array().expect("array").is_empty());
}

// The empty body of a GET-style probe reads as `{}` for all-optional endpoints and errors
// cleanly (not panics) for required-parameter endpoints.
#[tokio::test]
async fn empty_body_handling() {
    let (rpc, _mp) = seeded_rpc().await;
    let handler = NodeRpcHandler::new(rpc);
    let (status, v) = call_raw(&handler, "/get_connections", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["success"], Value::from(true));
    let (status, v) = call_raw(&handler, "/get_block", Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["success"], Value::from(false));
}

// ---- TLS posture over the real accept loop ----------------------------------------------------

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

fn write_client_certs(tag: &str) -> ClientSSLConfig {
    let (crt, key) = generate_ca_signed_cert_data(CHIA_CA_CRT.as_bytes(), CHIA_CA_KEY.as_bytes())
        .expect("client cert");
    let dir = std::env::temp_dir();
    let crt_path = dir.join(format!("rpc_http_{}_{tag}.crt", std::process::id()));
    let key_path = dir.join(format!("rpc_http_{}_{tag}.key", std::process::id()));
    std::fs::write(&crt_path, &crt).expect("write crt");
    std::fs::write(&key_path, &key).expect("write key");
    ClientSSLConfig {
        ssl_crt_path: crt_path.to_string_lossy().to_string(),
        ssl_key_path: key_path.to_string_lossy().to_string(),
        ssl_ca_crt_path: String::new(),
    }
}

async fn spawn_tls_server() -> (
    u16,
    Arc<AtomicBool>,
    Arc<Mutex<Mempool>>,
    Arc<NodeRpc<SqliteStore>>,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (rpc, mempool) = seeded_rpc().await;
    let tls = build_rpc_tls_context().expect("tls context");
    let port = free_port();
    let handler = Arc::new(NodeRpcHandler::new(rpc.clone()));
    let server = RpcServer::new_with_server_config(
        &RpcServerConfig {
            host: "127.0.0.1".to_string(),
            port,
            ssl_info: None,
        },
        tls.server_config,
        handler,
    )
    .expect("server");
    let run = Arc::new(AtomicBool::new(true));
    let run_c = run.clone();
    tokio::spawn(async move {
        let _ = server.run(run_c).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    (port, run, mempool, rpc)
}

// End-to-end with the REAL chia-shaped Rust RPC client: client cert signed by our CA is
// accepted, https + envelope parse round-trips four representative endpoints.
#[tokio::test]
async fn tls_e2e_chia_client_four_endpoints() {
    let (port, run, mempool, rpc) = spawn_tls_server().await;
    mempool.lock().await.set_peak(common::PEAK_HEIGHT, 0);
    let coin = common::seed_easy_coin(rpc.store(), 1_000).await;

    let ssl = write_client_certs("e2e");
    let client = FullnodeClient::new("127.0.0.1", port, 15, Some(ssl), &None).expect("client");

    // 1. get_blockchain_state
    let state = client.get_blockchain_state().await.expect("state");
    assert_eq!(
        state.peak.as_ref().map(|p| p.height),
        Some(common::PEAK_HEIGHT)
    );
    assert!(state.sync.synced);

    // 2. get_block
    let hh = common::peak_record().header_hash;
    let block = client.get_block(&hh).await.expect("block");
    assert_eq!(block.header_hash().expect("hh"), hh);

    // 3. get_coin_records_by_names (spent default false — the seeded additions are unspent)
    let name = common::additions()[0].coin.name();
    let records = client
        .get_coin_records_by_names(&[name], None, None, None)
        .await
        .expect("records");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].coin.name(), name);

    // 4. push_tx (typed status through the envelope)
    let status = client
        .push_tx(&common::easy_bundle(&coin, 1))
        .await
        .expect("push");
    assert_eq!(status, TXStatus::SUCCESS);
    assert_eq!(mempool.lock().await.len(), 1);

    // The error envelope surfaces as a typed client error, not a decode failure.
    let err = client
        .get_block(&Bytes32::from([0x24u8; 32]))
        .await
        .expect_err("unknown block errors");
    assert!(
        err.error.unwrap_or_default().contains("not found"),
        "server error string reaches the client"
    );

    run.store(false, Ordering::Relaxed);
}

// chia posture negative: a client with NO certificate is refused at the handshake
// (chia server.py:70 `verify_mode = ssl.CERT_REQUIRED`).
#[tokio::test]
async fn tls_no_client_cert_is_refused() {
    let (port, run, _mp, _rpc) = spawn_tls_server().await;
    let verifier = Arc::new(dg_xch_core::protocols::shared::NoCertificateVerification);
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let refused = raw_request_fails(port, Arc::new(cfg)).await;
    assert!(
        refused,
        "a certificate-less client must not get an HTTP response"
    );
    run.store(false, Ordering::Relaxed);
}

// chia posture negative: a client certificate from the WRONG CA (here: signed by a mere leaf,
// so it cannot chain to the server's root) is refused at the handshake.
#[tokio::test]
async fn tls_wrong_ca_client_cert_is_refused() {
    use dg_xch_core::ssl::{load_certs_from_bytes, load_private_key_from_bytes};
    let (port, run, _mp, _rpc) = spawn_tls_server().await;
    // A cert signed by a LEAF of the real CA — presentable, but it does not chain to the CA root.
    let (leaf_crt, leaf_key) =
        generate_ca_signed_cert_data(CHIA_CA_CRT.as_bytes(), CHIA_CA_KEY.as_bytes()).expect("leaf");
    let (wrong_crt, wrong_key) =
        generate_ca_signed_cert_data(&leaf_crt, &leaf_key).expect("wrong-ca cert");
    let certs = load_certs_from_bytes(&wrong_crt).expect("certs");
    let key = load_private_key_from_bytes(&wrong_key).expect("key");
    let verifier = Arc::new(dg_xch_core::protocols::shared::NoCertificateVerification);
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .expect("client auth");
    let refused = raw_request_fails(port, Arc::new(cfg)).await;
    assert!(refused, "a wrong-CA client must not get an HTTP response");
    run.store(false, Ordering::Relaxed);
}

// Positive control for the raw path: the SAME raw client WITH a CA-chained cert gets an HTTP
// response — so the negatives above fail for the cert reason, not a plumbing one.
#[tokio::test]
async fn tls_raw_client_with_valid_cert_succeeds() {
    use dg_xch_core::ssl::{load_certs_from_bytes, load_private_key_from_bytes};
    let (port, run, _mp, _rpc) = spawn_tls_server().await;
    let (crt, key_bytes) =
        generate_ca_signed_cert_data(CHIA_CA_CRT.as_bytes(), CHIA_CA_KEY.as_bytes()).expect("cert");
    let certs = load_certs_from_bytes(&crt).expect("certs");
    let key = load_private_key_from_bytes(&key_bytes).expect("key");
    let verifier = Arc::new(dg_xch_core::protocols::shared::NoCertificateVerification);
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .expect("client auth");
    let refused = raw_request_fails(port, Arc::new(cfg)).await;
    assert!(!refused, "a CA-chained client cert is accepted");
    run.store(false, Ordering::Relaxed);
}

// Issue one GET /healthz over TLS with the given client config; true = no HTTP response came
// back (handshake or IO refused), false = a well-formed HTTP response arrived.
async fn raw_request_fails(port: u16, cfg: Arc<rustls::ClientConfig>) -> bool {
    let connector = tokio_rustls::TlsConnector::from(cfg);
    let Ok(tcp) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else {
        return true;
    };
    let server_name = rustls::pki_types::ServerName::try_from("localhost").expect("name");
    let Ok(mut tls) = connector.connect(server_name, tcp).await else {
        return true;
    };
    let req = "GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if tls.write_all(req.as_bytes()).await.is_err() {
        return true;
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tls.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
    !String::from_utf8_lossy(&buf).starts_with("HTTP/1.1 200")
}

// ===== PR #52 SECURITY RED TEST — RPC authentication bypass via the world-public Chia CA =========
//
// FINDING (pr-52, F1): in the DEFAULT configuration (no `PRIVATE_CA_CRT` / `PRIVATE_CA_KEY` env
// vars set), `build_rpc_tls_context` (full-node/src/rpc.rs:1353-1363) roots the RPC client-cert
// verifier at the embedded, world-PUBLIC `CHIA_CA_CRT`, whose matching private key `CHIA_CA_KEY`
// is committed in this repo (core/src/constants.rs:163-190) and published in chia-blockchain. The
// production `--rpc` default binds `0.0.0.0:8555` (full-node/src/main.rs:24) and the RPC server is
// started unconditionally (full-node/src/daemon.rs:4387 -> spawn_rpc_server). Because the CA
// private key is public, ANY network-reachable attacker can mint a client cert that chains to it
// (precisely what the "legitimate" `write_client_certs` helper above does) and satisfy the mTLS
// client-auth — so the whole RPC surface (`/push_tx`, `/get_all_mempool_items`, `/get_connections`,
// coin & block queries) is effectively UNAUTHENTICATED despite presenting client-cert mTLS.
//
// This test states the SECURE property: a client presenting a cert that merely chains to the
// world-public Chia CA (an unauthenticated stranger, byte-indistinguishable from the current
// "chia client") MUST be refused by the default RPC listener. It FAILS on #51 HEAD — the stranger
// is accepted, exactly as the sibling `tls_e2e_chia_client_four_endpoints` round-trips real
// endpoints with the very same public-CA cert. Turning it green is a TLS trust-model decision
// (require an explicit private CA for RPC client-auth, refuse to start client-auth against the
// public CA, and/or bind the RPC to loopback by default) — routed to Grant, not fixed here.
// CWE-321 (hard-coded cryptographic key) + CWE-295 (improper certificate validation).
#[tokio::test]
async fn tls_public_chia_ca_client_is_an_auth_bypass() {
    use dg_xch_core::ssl::{load_certs_from_bytes, load_private_key_from_bytes};
    let (port, run, _mp, _rpc) = spawn_tls_server().await;
    // The attacker's entire capability: sign a client cert with the PUBLIC, in-repo Chia CA key.
    // No secret is required — `CHIA_CA_KEY` ships in this source tree and in chia-blockchain.
    let (crt, key_bytes) =
        generate_ca_signed_cert_data(CHIA_CA_CRT.as_bytes(), CHIA_CA_KEY.as_bytes())
            .expect("attacker cert signed by the world-public Chia CA");
    let certs = load_certs_from_bytes(&crt).expect("certs");
    let key = load_private_key_from_bytes(&key_bytes).expect("key");
    // Client-side server verification is intentionally disabled: this test exercises the SERVER's
    // client-cert verification, not the client's trust in the server.
    let verifier = Arc::new(dg_xch_core::protocols::shared::NoCertificateVerification);
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(certs, key)
        .expect("client auth");
    let refused = raw_request_fails(port, Arc::new(cfg)).await;
    assert!(
        refused,
        "SECURITY (CWE-321/CWE-295): an unauthenticated client whose cert merely chains to the \
         WORLD-PUBLIC Chia CA must not reach the RPC — trusting a CA whose private key is public \
         is no authentication at all. This assertion FAILS on #51 HEAD, proving the bypass."
    );
    run.store(false, Ordering::Relaxed);
}
