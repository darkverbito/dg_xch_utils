mod common;

// Inbound `error` protocol frames (chia ede354c58, code 255) and unknown message types over the
// real WS loopback.
//
// chia's posture, mirrored here:
//   - an `error` frame is DECODED + LOGGED and the connection carries on — no ban, no
//     disconnect (ws_connection.py `_api_call`: `Error.from_bytes` → `log.warning` → return);
//   - a message type chia does not define disconnects the peer with a short ban
//     (chia b1b68072a: PROTOCOL_ERROR close + INTERNAL_PROTOCOL_ERROR_BAN_SECONDS).

use common::{MemApi, connect, spawn_full_node};
use dg_xch_clients::websocket::oneshot;
use dg_xch_core::protocols::full_node::{RequestPeers, RespondPeers};
use dg_xch_core::protocols::shared::ErrorMessage;
use dg_xch_core::protocols::{ChiaMessage, ProtocolMessageTypes};
use dg_xch_serialize::ChiaProtocolVersion;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

fn blind_api() -> Arc<MemApi> {
    Arc::new(MemApi {
        blocks: HashMap::new(),
        gossip: Vec::new(),
        respond_peers_seen: Arc::new(RwLock::new(Vec::new())),
    })
}

async fn request_peers_round_trips(client: &dg_xch_clients::websocket::full_node::FullnodeClient) {
    let version = ChiaProtocolVersion::default();
    let _: RespondPeers = oneshot(
        client.client.connection.clone(),
        ChiaMessage::new(
            ProtocolMessageTypes::RequestPeers,
            version,
            &RequestPeers {},
            Some(21),
        )
        .unwrap(),
        Some(ProtocolMessageTypes::RespondPeers),
        version,
        Some(21),
        Some(15000),
    )
    .await
    .expect("the connection still serves after the frame");
}

// A conforming CNI peer's `error` report (a real chia message type) must be tolerated: logged,
// never treated as a protocol violation — the connection keeps serving. This is the guard that
// keeps the unknown-type disconnect from over-reaching onto chia's own code 255.
#[tokio::test]
async fn error_frame_is_tolerated_and_the_connection_keeps_serving() {
    let server = spawn_full_node(blind_api()).await;
    let client = connect(server.port).await;
    let version = ChiaProtocolVersion::default();

    let err_frame = ChiaMessage::new(
        ProtocolMessageTypes::Error,
        version,
        &ErrorMessage {
            code: -13,
            message: "INVALID_FEE_TOO_CLOSE_TO_ZERO".to_string(),
            data: None,
        },
        None,
    )
    .unwrap();
    client
        .client
        .connection
        .write()
        .await
        .send(err_frame.into())
        .await
        .expect("error frame sent");

    request_peers_round_trips(&client).await;
    assert_eq!(server.peers.read().await.len(), 1, "peer not evicted");
    assert!(
        server.bans.is_empty(),
        "no ban for a legitimate error frame"
    );
}

// A message type chia 2.7.1 does NOT define must disconnect the peer with a short host ban —
// chia b1b68072a (`_read_one_message`: unknown `ProtocolMessageTypes(type)` → ERROR log →
// `close(INTERNAL_PROTOCOL_ERROR_BAN_SECONDS, WSCloseCode.PROTOCOL_ERROR,
// Err.INVALID_PROTOCOL_MESSAGE)`). Before this landed we logged "No Matches" and kept serving.
#[tokio::test]
async fn unknown_message_type_disconnects_and_bans() {
    let server = spawn_full_node(blind_api()).await;
    let client = connect(server.port).await;

    // Hand-framed message: type 254 (unassigned in chia), no id, empty length-prefixed body
    // (chia Message: uint8 type, Optional[uint16] id, bytes data).
    let raw: Vec<u8> = vec![254u8, 0u8, 0, 0, 0, 0];
    client
        .client
        .connection
        .write()
        .await
        .send(Message::Binary(raw.into()))
        .await
        .expect("frame sent");

    // The read loop must evict + close promptly.
    let evicted = common::wait_until(
        || {
            let peers = server.peers.clone();
            async move { peers.read().await.is_empty() }
        },
        Duration::from_secs(10),
    )
    .await;
    assert!(
        evicted,
        "the peer must be evicted for an unknown message type"
    );
    assert!(
        server
            .bans
            .is_banned(&"127.0.0.1".parse::<std::net::IpAddr>().unwrap()),
        "the host gets chia's short INTERNAL_PROTOCOL_ERROR ban"
    );

    // And the ban is enforced at the accept path: an immediate reconnect is refused.
    let reconnect = dg_xch_clients::websocket::full_node::FullnodeClient::new(
        Arc::new(dg_xch_clients::websocket::WsClientConfig {
            host: "127.0.0.1".to_string(),
            port: server.port,
            network_id: "mainnet".to_string(),
            ssl_info: None,
            software_version: None,
            protocol_version: ChiaProtocolVersion::default(),
            additional_headers: None,
            rate_limited: false,
        }),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
        5,
    )
    .await;
    assert!(
        reconnect.is_err(),
        "a banned host's reconnect must be refused within the ban window"
    );
}
