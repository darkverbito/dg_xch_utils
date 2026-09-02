//! Red demonstration for docs/security-review-2026-09.md (finding 5). Fails until the fix
//! lands; run with `cargo test -p dg_xch_clients --test red_pool_tls -- --ignored`. When the
//! fix lands, remove the ignore and keep the test as the regression gate.

use dg_xch_clients::api::pool::{DefaultPoolClient, PoolClient};
use dg_xch_core::ssl::{
    generate_ca_signed_cert_data, load_certs_from_bytes, load_private_key_from_bytes,
    make_ca_cert_data,
};
use std::io::{Read, Write};
use std::sync::Arc;

// Finding 5: pool metadata feeds a state the wallet SIGNS; the transport must authenticate the
// pool. A server presenting a certificate no public authority vouches for must be refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "red: finding 5 in docs/security-review-2026-09.md — the pool client accepts any TLS certificate"]
async fn the_pool_client_rejects_an_unverified_certificate() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ca_cert, ca_key) = make_ca_cert_data().expect("ca");
    let (server_cert, server_key) = generate_ca_signed_cert_data(&ca_cert, &ca_key).expect("srv");
    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            load_certs_from_bytes(&server_cert).expect("certs"),
            load_private_key_from_bytes(&server_key).expect("key"),
        )
        .expect("server config");

    let body = concat!(
        "{\"name\":\"pool\",\"logo_url\":\"\",\"minimum_difficulty\":1,",
        "\"relative_lock_height\":100,\"protocol_version\":1,\"fee\":\"0.01\",",
        "\"description\":\"\",",
        "\"target_puzzle_hash\":\"0x",
        "1111111111111111111111111111111111111111111111111111111111111111",
        "\",\"authentication_token_timeout\":5}"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let config = Arc::new(server_config);
        // Serve a handful of connections: reqwest may probe more than once.
        for _ in 0..4 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let Ok(mut conn) = rustls::ServerConnection::new(config.clone()) else {
                return;
            };
            let mut ok = true;
            while conn.is_handshaking() {
                if conn.complete_io(&mut stream).is_err() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            let mut buf = [0u8; 4096];
            let _ = conn.reader().read(&mut buf);
            let _ = conn.complete_io(&mut stream);
            let _ = conn.reader().read(&mut buf);
            let _ = conn.writer().write_all(response.as_bytes());
            while conn.wants_write() {
                if conn.complete_io(&mut stream).is_err() {
                    break;
                }
            }
            conn.send_close_notify();
            let _ = conn.complete_io(&mut stream);
        }
    });

    let client = DefaultPoolClient::new();
    let outcome = client
        .get_pool_info(&format!("https://127.0.0.1:{port}"))
        .await;
    drop(server);
    assert!(
        outcome.is_err(),
        "pool_info was accepted over a connection whose certificate no authority vouches for; \
         the signed pool state downstream inherits whatever this endpoint claims"
    );
}
