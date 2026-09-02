//! Red demonstrations for docs/security-review-2026-09.md (findings 1, 2, and 3). Each test
//! asserts the SAFE behavior and fails until its finding's fix lands; run with
//! `cargo test -p dg_xch_p2p --test red_security_review -- --ignored`. When a fix lands,
//! remove the ignore and keep the test as the regression gate.

mod common;

use common::{empty_api, fast_settings, spawn_full_node, wait_until};
use dg_xch_p2p::Supervisor;
use std::time::Duration;

// Finding 1: inbound sessions must draw from the configured peer budget. Twelve concurrent
// inbound peers against a small cap must leave at most the cap admitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "red: finding 1 in docs/security-review-2026-09.md — inbound admission bypasses the peer cap"]
async fn inbound_sessions_respect_the_peer_cap() {
    const CAP: usize = 8;
    let server = spawn_full_node(empty_api()).await;
    let mut sups = Vec::new();
    for _ in 0..12 {
        let mut sup = Supervisor::new(fast_settings());
        sup.start_manual("127.0.0.1", server.port);
        sups.push(sup);
    }
    assert!(
        wait_until(
            || async { server.peers.read().await.len() >= CAP },
            Duration::from_secs(20)
        )
        .await,
        "the server accepted connections"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let admitted = server.peers.read().await.len();
    for sup in &mut sups {
        sup.stop().await;
    }
    assert!(
        admitted <= CAP,
        "{admitted} inbound sessions live against a cap of {CAP} — inbound admission is unbounded"
    );
}

// Finding 2: a socket that completes TCP and withholds its TLS bytes must not stop the
// listener from accepting the next peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "red: finding 2 in docs/security-review-2026-09.md — an inline TLS handshake serializes the accept loop"]
async fn a_stalled_handshake_does_not_block_new_accepts() {
    let server = spawn_full_node(empty_api()).await;
    // Hold a raw TCP connection open without ever starting the TLS handshake.
    let stalled = std::net::TcpStream::connect(("127.0.0.1", server.port)).expect("tcp connects");
    stalled.set_nodelay(true).ok();

    let mut sup = Supervisor::new(fast_settings());
    sup.start_manual("127.0.0.1", server.port);
    let reg = sup.registry.clone();
    let connected = wait_until(
        || async { reg.outbound_count().await == 1 },
        Duration::from_secs(15),
    )
    .await;
    sup.stop().await;
    drop(stalled);
    assert!(
        connected,
        "a well-behaved peer could not connect while one stalled handshake held the accept loop"
    );
}

// Finding 3: presenting a certificate is only an identity when the client proves possession of
// its private key. A handshake whose CertificateVerify is signed by a DIFFERENT key must fail.
#[test]
#[ignore = "red: finding 3 in docs/security-review-2026-09.md — the verifier accepts a certificate without proof of possession"]
fn a_client_without_the_certificates_key_fails_the_handshake() {
    use dg_xch_core::ssl::{
        AllowAny, generate_ca_signed_cert_data, load_certs_from_bytes, load_private_key_from_bytes,
        make_ca_cert_data,
    };
    use rustls::SignatureScheme;
    use rustls::client::ResolvesClientCert;
    use rustls::sign::CertifiedKey;
    use std::io::{Read, Write};
    use std::sync::Arc;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let (ca_cert, ca_key) = make_ca_cert_data().expect("ca");
    let (server_cert, server_key) = generate_ca_signed_cert_data(&ca_cert, &ca_key).expect("srv");
    let (victim_cert, _victim_key) = generate_ca_signed_cert_data(&ca_cert, &ca_key).expect("vic");
    let (_attacker_cert, attacker_key) =
        generate_ca_signed_cert_data(&ca_cert, &ca_key).expect("atk");

    // The attacker presents the victim's certificate but can only sign with its own key.
    #[derive(Debug)]
    struct MismatchedIdentity(Arc<CertifiedKey>);
    impl ResolvesClientCert for MismatchedIdentity {
        fn resolve(
            &self,
            _root_hint_subjects: &[&[u8]],
            _sigschemes: &[SignatureScheme],
        ) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
        fn has_certs(&self) -> bool {
            true
        }
    }

    let server_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(AllowAny::new())
        .with_single_cert(
            load_certs_from_bytes(&server_cert).expect("server certs"),
            load_private_key_from_bytes(&server_key).expect("server key"),
        )
        .expect("server config");
    let signing_key = rustls::crypto::ring::sign::any_supported_type(
        &load_private_key_from_bytes(&attacker_key).expect("attacker key"),
    )
    .expect("signing key");
    let certified = Arc::new(CertifiedKey::new(
        load_certs_from_bytes(&victim_cert).expect("victim certs"),
        signing_key,
    ));
    let client_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(
            dg_xch_core::protocols::shared::NoCertificateVerification,
        ))
        .with_client_cert_resolver(Arc::new(MismatchedIdentity(certified)));

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut conn = rustls::ServerConnection::new(Arc::new(server_config)).expect("server conn");
        // Drive the handshake to completion or error.
        while conn.is_handshaking() {
            if conn.complete_io(&mut stream).is_err() {
                return Err(());
            }
        }
        // Read one byte so the client's Finished flight is fully processed.
        let mut buf = [0u8; 1];
        let mut plain = conn.reader();
        let _ = plain.read(&mut buf);
        Ok(())
    });

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut conn = rustls::ClientConnection::new(
        Arc::new(client_config),
        rustls::pki_types::ServerName::try_from("chia.net").expect("name"),
    )
    .expect("client conn");
    let mut client_ok = true;
    while conn.is_handshaking() {
        if conn.complete_io(&mut stream).is_err() {
            client_ok = false;
            break;
        }
    }
    if client_ok {
        let _ = conn.writer().write_all(b"x");
        let _ = conn.complete_io(&mut stream);
    }
    let server_ok = matches!(server.join(), Ok(Ok(())));
    assert!(
        !(client_ok && server_ok),
        "a handshake signed with the wrong private key completed — certificate identity is \
         accepted without proof of possession"
    );
}
