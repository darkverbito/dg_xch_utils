mod common;

// Decode-time list limits: the CPU half.
//
// A handler-declared per-field list limit is applied DURING decode, so a request whose list
// claims far more items than the handler will ever use is truncated while parsing — the
// remaining fixed-size elements are skipped in O(1) with a seek, never materialized. Parsing
// every element first is a CPU DoS: 1.2M coin_ids costs seconds on a small node. The four wired
// handlers and their limits:
//   register_for_ph_updates   -> puzzle_hashes capped at max_subscriptions(peer)
//   register_for_coin_updates -> coin_ids      capped at max_subscriptions(peer)
//   request_puzzle_state      -> puzzle_hashes capped at MAX_PUZZLE_HASH_BATCH_SIZE
//   request_coin_state        -> coin_ids      capped at max_subscribe_response_items(peer)
//
// These tests drive the two register arms over the real WS loopback: the store-blind default api
// echoes the parsed list back, so the echoed length IS the length the decode materialized. The
// request_puzzle_state / request_coin_state mechanics are pinned at the type level in
// `core/tests/list_limited_decode.rs`; their store-blind defaults reject, so the parsed list is
// not observable over the loopback.

use common::{MemApi, connect, spawn_full_node};
use dg_xch_clients::websocket::oneshot;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::protocols::wallet::{
    RegisterForCoinUpdates, RegisterForPhUpdates, RespondToCoinUpdates, RespondToPhUpdates,
};
use dg_xch_core::protocols::{ChiaMessage, ProtocolMessageTypes};
use dg_xch_serialize::ChiaProtocolVersion;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// The untrusted `max_subscriptions` default the store-blind handler layer must enforce at
// decode time.
const UNTRUSTED_MAX_SUBSCRIPTIONS: usize = 200_000;

fn ids(n: usize) -> Vec<Bytes32> {
    // Distinct leading bytes so a truncation keeps the FIRST `cap` items: the head of the list
    // is parsed and the tail skipped, so order must be preserved.
    (0..n)
        .map(|i| {
            let mut b = [0u8; 32];
            b[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            Bytes32::from(b)
        })
        .collect()
}

fn blind_api() -> Arc<MemApi> {
    Arc::new(MemApi {
        blocks: HashMap::new(),
        gossip: Vec::new(),
        respond_peers_seen: Arc::new(RwLock::new(Vec::new())),
    })
}

// An over-cap RegisterForPhUpdates must be truncated to max_subscriptions DURING decode: the
// echoed puzzle_hashes carry exactly the first `cap` items, proving the tail was skipped, not
// parsed: parsing stops early and seeks past the remaining fixed-size elements.
#[tokio::test]
async fn register_ph_updates_decode_truncates_at_max_subscriptions() {
    let server = spawn_full_node(blind_api()).await;
    let client = connect(server.port).await;
    let version = ChiaProtocolVersion::default();

    let sent = ids(UNTRUSTED_MAX_SUBSCRIPTIONS + 17);
    let resp: RespondToPhUpdates = oneshot(
        client.client.connection.clone(),
        ChiaMessage::new(
            ProtocolMessageTypes::RegisterInterestInPuzzleHash,
            version,
            &RegisterForPhUpdates {
                puzzle_hashes: sent.clone(),
                min_height: 3,
            },
            Some(11),
        )
        .unwrap(),
        Some(ProtocolMessageTypes::RespondToPhUpdate),
        version,
        Some(11),
        Some(30000),
    )
    .await
    .expect("served RespondToPhUpdates");

    assert_eq!(
        resp.puzzle_hashes.len(),
        UNTRUSTED_MAX_SUBSCRIPTIONS,
        "an over-cap puzzle-hash list must be truncated to max_subscriptions at decode time"
    );
    assert_eq!(
        resp.puzzle_hashes[..],
        sent[..UNTRUSTED_MAX_SUBSCRIPTIONS],
        "truncation keeps the FIRST cap items in order (head parsed, tail skipped)"
    );
    assert_eq!(
        resp.min_height, 3,
        "the fields AFTER the truncated list still decode correctly"
    );
}

// Same bound on the coin-id register arm: coin_ids capped at max_subscriptions.
#[tokio::test]
async fn register_coin_updates_decode_truncates_at_max_subscriptions() {
    let server = spawn_full_node(blind_api()).await;
    let client = connect(server.port).await;
    let version = ChiaProtocolVersion::default();

    let sent = ids(UNTRUSTED_MAX_SUBSCRIPTIONS + 5);
    let resp: RespondToCoinUpdates = oneshot(
        client.client.connection.clone(),
        ChiaMessage::new(
            ProtocolMessageTypes::RegisterInterestInCoin,
            version,
            &RegisterForCoinUpdates {
                coin_ids: sent.clone(),
                min_height: 9,
            },
            Some(12),
        )
        .unwrap(),
        Some(ProtocolMessageTypes::RespondToCoinUpdate),
        version,
        Some(12),
        Some(30000),
    )
    .await
    .expect("served RespondToCoinUpdates");

    assert_eq!(
        resp.coin_ids.len(),
        UNTRUSTED_MAX_SUBSCRIPTIONS,
        "an over-cap coin-id list must be truncated to max_subscriptions at decode time"
    );
    assert_eq!(resp.coin_ids[..], sent[..UNTRUSTED_MAX_SUBSCRIPTIONS]);
    assert_eq!(resp.min_height, 9);
}

// An under-cap registration is untouched — the limit only truncates, it never rejects or alters
// a conforming request: `items_to_parse = min(list_size, max_items)`.
#[tokio::test]
async fn under_cap_registration_is_unchanged() {
    let server = spawn_full_node(blind_api()).await;
    let client = connect(server.port).await;
    let version = ChiaProtocolVersion::default();

    let sent = ids(64);
    let resp: RespondToPhUpdates = oneshot(
        client.client.connection.clone(),
        ChiaMessage::new(
            ProtocolMessageTypes::RegisterInterestInPuzzleHash,
            version,
            &RegisterForPhUpdates {
                puzzle_hashes: sent.clone(),
                min_height: 0,
            },
            Some(13),
        )
        .unwrap(),
        Some(ProtocolMessageTypes::RespondToPhUpdate),
        version,
        Some(13),
        Some(30000),
    )
    .await
    .expect("served RespondToPhUpdates");

    assert_eq!(
        resp.puzzle_hashes, sent,
        "under-cap lists decode byte-identically"
    );
}
