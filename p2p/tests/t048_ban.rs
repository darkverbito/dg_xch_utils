mod common;

// Timed p2p ban list (chia `ChiaServer.banned_peers`): a recurring residual — our close
// sites tore the connection down but the peer could immediately
// reconnect, because we had no ban list. These tests exercise the wire path end-to-end over loopback
// TLS: a violation closes AND bans the host, a reconnect from the same host is refused at the accept
// path for the ban window, and clearing the ban restores access. The unit-level registry behaviour
// (durations, expiry, bounded/prune, host-keying) is proven in
// `dg_xch_core::protocols::ban::tests`.
//
// NOTE: loopback (127.0.0.1) IS bannable here — we deliberately do NOT replicate chia's
// localhost/trusted/exempt ban exemptions (a separate feature we have not implemented), which is what lets
// the loopback client observe the refusal.

use common::{
    connect, contiguous_api, spawn_full_node_rate_limited, try_connect, wait_until,
};
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::protocols::{ChiaMessage, ProtocolMessageTypes};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

// Send a raw framed message with no correlation id (unsolicited shape) straight down the socket,
// bypassing the request machinery so the read loop sees exactly what we send.
async fn raw_send(client: &dg_xch_clients::websocket::full_node::FullnodeClient, msg: ChiaMessage) {
    client
        .client
        .connection
        .write()
        .await
        .send(msg.into())
        .await
        .expect("raw send");
}

fn sized_msg(msg_type: ProtocolMessageTypes, len: usize) -> ChiaMessage {
    ChiaMessage {
        msg_type,
        id: None,
        data: UnsizedBytes::new(vec![0u8; len]),
    }
}

// Drive a rate-limit violation and wait for the server to evict + ban the peer.
async fn trip_rate_limit(server: &common::RunningServer) {
    let client = connect(server.port).await;
    assert!(
        wait_until(
            || async { server.peers.read().await.len() == 1 },
            Duration::from_secs(5),
        )
        .await,
        "server registers the inbound peer"
    );
    // Six request_proof_of_weight: the 6th trips the 5/min cap (chia rate_limit_numbers.py:95).
    for _ in 0..6 {
        raw_send(&client, sized_msg(ProtocolMessageTypes::RequestProofOfWeight, 10)).await;
    }
    assert!(
        wait_until(
            || async { server.peers.read().await.is_empty() },
            Duration::from_secs(5),
        )
        .await,
        "the flooding peer is closed + evicted"
    );
}

// Headline red→green: a rate-limit violation not only closes the peer, it BANS the host — a reconnect
// from the same host within the window is refused at the accept path. On the pre-ban-list code the
// second connect succeeds (the residual this test closes); with the ban list it fails.
#[tokio::test]
async fn banned_host_cannot_reconnect_within_the_window() {
    let server = spawn_full_node_rate_limited(contiguous_api(100, 1)).await;
    trip_rate_limit(&server).await;

    // The host is now banned (chia RATE_LIMITER_BAN_SECONDS = 300s).
    assert!(
        wait_until(
            || async { server.bans.is_banned(&LOOPBACK) },
            Duration::from_secs(5),
        )
        .await,
        "the violating host is entered into the timed ban list"
    );

    // A fresh dial from the same host is refused at the accept path (HTTP 403 → handshake fails).
    let refused = try_connect(server.port).await;
    assert!(
        refused.is_err(),
        "a reconnect from a banned host must be refused (was allowed on the pre-ban-list code)"
    );

    server
        .run
        .store(false, std::sync::atomic::Ordering::Relaxed);
}

// After the ban lifts (here simulated deterministically by clearing the registry, standing in for the
// 300s expiry the unit test proves), the same host connects fine again — the accept path consults the
// live registry, it is not a permanent block.
#[tokio::test]
async fn host_reconnects_after_the_ban_is_lifted() {
    let server = spawn_full_node_rate_limited(contiguous_api(100, 1)).await;
    trip_rate_limit(&server).await;
    assert!(
        wait_until(
            || async { server.bans.is_banned(&LOOPBACK) },
            Duration::from_secs(5),
        )
        .await,
        "host banned"
    );
    assert!(try_connect(server.port).await.is_err(), "refused while banned");

    // Ban lifts (expiry / operator reset).
    server.bans.clear();
    assert!(!server.bans.is_banned(&LOOPBACK), "ban cleared");

    // The same host is admitted again.
    let ok = try_connect(server.port).await;
    assert!(
        ok.is_ok(),
        "after the ban lifts the host connects normally: {:?}",
        ok.err()
    );

    server
        .run
        .store(false, std::sync::atomic::Ordering::Relaxed);
}

// The close_peer ban path: an unsolicited block reply closes AND bans the sender
// (chia full_node_api.py respond_block → close(RATE_LIMITER_BAN_SECONDS)). Proves the handler-side
// close site records the ban, not just the read-loop rate limiter.
#[tokio::test]
async fn unsolicited_block_reply_bans_the_sender() {
    let server = spawn_full_node_rate_limited(contiguous_api(100, 1)).await;
    let client = connect(server.port).await;
    assert!(
        wait_until(
            || async { server.peers.read().await.len() == 1 },
            Duration::from_secs(5),
        )
        .await,
        "peer registers"
    );

    // One unsolicited RespondBlock (id=None → not consumed by the correlation fast path → reaches the
    // close arm). Small body so the inbound size limiter passes it through to dispatch.
    raw_send(&client, sized_msg(ProtocolMessageTypes::RespondBlock, 16)).await;

    assert!(
        wait_until(
            || async { server.peers.read().await.is_empty() && server.bans.is_banned(&LOOPBACK) },
            Duration::from_secs(5),
        )
        .await,
        "the unsolicited-reply sender is closed + evicted + banned"
    );
    assert!(
        try_connect(server.port).await.is_err(),
        "the banned sender cannot reconnect within the window"
    );

    server
        .run
        .store(false, std::sync::atomic::Ordering::Relaxed);
}

// The ban keys on the REMOTE host, not on our own node identity (cert hash). After a violation the
// registry holds the peer's remote IP (127.0.0.1) — a wire-level confirmation of the host-keying the
// unit test asserts in isolation.
#[tokio::test]
async fn ban_keys_on_remote_host() {
    let server = spawn_full_node_rate_limited(contiguous_api(100, 1)).await;
    trip_rate_limit(&server).await;

    assert!(
        wait_until(
            || async { server.bans.is_banned(&LOOPBACK) },
            Duration::from_secs(5),
        )
        .await,
        "the ban is keyed on the peer's remote host (127.0.0.1), the ban key chia uses"
    );
    // A different host is not swept up by one peer's violation.
    assert!(
        !server.bans.is_banned(&IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1))),
        "an unrelated host is unaffected"
    );

    server
        .run
        .store(false, std::sync::atomic::Ordering::Relaxed);
}
