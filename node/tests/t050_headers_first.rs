mod common;

use std::io::Cursor;

use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::weight_proof::RecentChainData;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use dg_xch_stores::BlockStore;

// The reference required_iters the weight-proof recent-block validator produces for these heights (same
// golden the header-validation test pins). Full validation off the sync-populated cache must reproduce these.
const GOLDEN: &[(u32, u64)] = &[(9_054_611, 2_155_695), (9_054_612, 7_701_062)];
// The epoch parameters for the slice (no sub-epoch boundary inside it) — same constants the header-validation
// and difficulty-retarget mainnet tests pin.
const SSI: u64 = 574_619_648;
const DIFF: u64 = 2608;

fn load_chain() -> Vec<HeaderBlock> {
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054524_9054620.bin");
    RecentChainData::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("recent chain slice deserializes")
        .recent_chain_data
}

// Headers-first candidate chain. A real mainnet header range is validated (PoW) and stored as the
// candidate chain — decoupled from body download. Full validation triggers off the tip cache the sync
// populates (closing the header-validation test's caveat), the records land without bodies, and no block is confirmed yet.
#[tokio::test]
async fn headers_first_candidate_chain_validates_and_stores_without_bodies() {
    let chain = load_chain();
    assert!(chain.len() > 80, "real mainnet slice present");

    let store = common::new_store().await;
    let engine = Engine::new(store, NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());

    let stored = chaser
        .sync_headers(
            &chain,
            &dg_xch_node::EpochSchedule::from_summaries(&[], MAINNET.sub_epoch_blocks, SSI, DIFF),
            &[],
        )
        .await
        .expect("sync headers");
    assert_eq!(
        stored,
        chain.len(),
        "every candidate header record is stored"
    );

    // Full PoW/VDF validation fires off the ancestry the header sync populated — no hand-built ancestor map
    // (closing the header-validation test's caveat) — and reproduces the weight-proof reference required_iters for the golden heights.
    let by_height: std::collections::HashMap<u32, &HeaderBlock> =
        chain.iter().map(|b| (b.height(), b)).collect();
    for &(height, expected) in GOLDEN {
        let header = by_height.get(&height).expect("golden height in slice");
        let got = chaser
            .validate_stored_header(header, SSI, DIFF)
            .unwrap_or_else(|e| panic!("full validation off the synced cache at {height}: {e}"));
        assert_eq!(
            got, expected,
            "chaser reproduced the weight-proof required_iters at {height} off the synced tip cache"
        );
    }

    // Every header in the slice is stored as a candidate record but has no body yet: get_unassociated (the
    // reservation-window feed) reports them, and none is confirmed (candidate chain, not the main chain).
    let store = chaser.engine().store();
    let pending = store
        .get_unassociated(chain.len() + 10)
        .await
        .expect("unassociated");
    assert_eq!(
        pending.len(),
        chain.len(),
        "all candidate heights need bodies — headers-first stored records, not bodies"
    );
    assert!(
        store.get_peak().await.expect("peak").is_none(),
        "headers-first confirms nothing — the candidate chain is separate from the confirmed chain"
    );
}

// The sub-epoch-summary attachment (the ses machinery): a boundary header declares its
// included summary's hash (authenticated by the challenge-chain VDF chain); headers-first must attach the
// weight-proof-attested summary OBJECT to that record —
// because the epoch machinery (can_finish_sub_and_full_epoch, the
// get_next_* pass-throughs, make_sub_epoch_summary's prev-ses walk) reads records, not headers. A boundary
// record stored WITHOUT its summary makes every later new-slot block in the sub-epoch spuriously
// "finishable" (INVALID_SUB_EPOCH_SUMMARY rejections). This slice starts ON the mainnet sub-epoch boundary
// at 9,054,336, and the summaries come from the committed real weight proof (phase-2 reconstruction only).
#[tokio::test]
async fn headers_first_attaches_the_weight_proof_summary_at_the_boundary() {
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054336_9054620.bin");
    let chain =
        RecentChainData::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
            .expect("boundary slice deserializes")
            .recent_chain_data;
    let wp_bytes =
        include_bytes!("../../weight-proof/tests/fixtures/weight_proof_mainnet_9054698.bin");
    let wp = dg_xch_core::blockchain::weight_proof::WeightProof::from_bytes(
        &mut Cursor::new(&wp_bytes[..]),
        ChiaProtocolVersion::default(),
    )
    .expect("real mainnet weight proof deserializes");
    let summaries = dg_xch_weight_proof::sub_epoch_summaries_of(&wp, &MAINNET)
        .expect("summary chain reconstructs");

    let declaring: Vec<&HeaderBlock> = chain
        .iter()
        .filter(|h| {
            h.finished_sub_slots
                .iter()
                .any(|s| s.challenge_chain.subepoch_summary_hash.is_some())
        })
        .collect();
    assert!(
        !declaring.is_empty(),
        "precondition: the boundary slice contains a summary-declaring header"
    );

    let store = common::new_store().await;
    let engine = Engine::new(store, NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());
    chaser
        .sync_headers(
            &chain,
            &dg_xch_node::EpochSchedule::from_summaries(
                &summaries,
                MAINNET.sub_epoch_blocks,
                SSI,
                DIFF,
            ),
            &summaries,
        )
        .await
        .expect("sync headers across the sub-epoch boundary");

    for header in &declaring {
        let declared = header
            .finished_sub_slots
            .iter()
            .find_map(|s| s.challenge_chain.subepoch_summary_hash)
            .unwrap();
        let record = chaser
            .engine()
            .store()
            .get_block_record(&header.header_hash().unwrap())
            .await
            .expect("get_block_record")
            .expect("boundary candidate record stored");
        let ses = record
            .sub_epoch_summary_included
            .expect("the boundary record carries the attested sub-epoch summary");
        assert_eq!(
            ses.hash().unwrap(),
            declared,
            "the attached summary hashes to the header's declared (VDF-authenticated) value"
        );
    }

    // A non-declaring header's record carries no summary — presence is the epoch machinery's signal.
    let plain = chain
        .iter()
        .find(|h| {
            h.finished_sub_slots
                .iter()
                .all(|s| s.challenge_chain.subepoch_summary_hash.is_none())
        })
        .expect("a non-declaring header in the slice");
    let plain_record = chaser
        .engine()
        .store()
        .get_block_record(&plain.header_hash().unwrap())
        .await
        .expect("get_block_record")
        .expect("record stored");
    assert!(
        plain_record.sub_epoch_summary_included.is_none(),
        "non-boundary records carry no summary"
    );
}
