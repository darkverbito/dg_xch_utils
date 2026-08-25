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
