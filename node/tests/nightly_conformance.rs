// NIGHTLY CONFORMANCE SENTINEL. The per-commit suites validate spot heights (t050's two goldens)
// and structural properties; the drift they cannot see is a validation change that keeps the
// goldens green while breaking elsewhere in a real span. The sentinel replays the FULL committed
// mainnet corpus through the headers-first pipeline and full single-header PoW validation at
// EVERY height, pinned to the weight-proof-attested epoch schedule — so any consensus-code drift
// against the real chain surfaces nightly, not at the next live sync.
//
// Run:  cargo test -p dg_xch_node --test nightly_conformance -- --ignored
// Pair with the six-phase weight-proof leg (release-gated, same cadence):
//       cargo test -p dg_xch_node --release --test t053_fast_sync -- --ignored

mod common;

use dg_xch_core::blockchain::weight_proof::{RecentChainData, WeightProof};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;
use std::sync::Arc;

// The slice's flat epoch parameters (no epoch turn inside: 9,054,336 is a sub-epoch, not an
// epoch, multiple) — the same values t050 pins; the attested schedule must agree (asserted).
const SSI: u64 = 574_619_648;
const DIFF: u64 = 2608;
// The weight-proof reference required_iters goldens t050 pins — the sentinel must reproduce them
// through the schedule-driven path.
const GOLDEN: &[(u32, u64)] = &[(9_054_611, 2_155_695), (9_054_612, 7_701_062)];
// Headers whose validation ancestry precedes the slice cannot fully validate from the slice
// alone; everything at or past this offset MUST validate. Empirically the deepest lookback for
// this slice is under one sub-slot of blocks.
const ANCESTRY_WARMUP: usize = 64;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "nightly conformance sentinel: full-span mainnet header replay (~minutes); run with --ignored"]
async fn full_mainnet_slice_replays_and_every_header_validates() {
    // The full committed corpus: 285 real mainnet headers crossing the 9,054,336 sub-epoch
    // boundary, and the real 14 MB weight proof whose summary chain attests the epoch schedule.
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054336_9054620.bin");
    let chain = RecentChainData::from_bytes_full(bytes, ChiaProtocolVersion::default())
        .expect("corpus slice decodes with exact-fit framing")
        .recent_chain_data;
    assert!(chain.len() > 280, "the full slice is present");
    let wp_bytes =
        include_bytes!("../../weight-proof/tests/fixtures/weight_proof_mainnet_9054698.bin");
    let wp = WeightProof::from_bytes(&mut Cursor::new(&wp_bytes[..]), ChiaProtocolVersion::default())
        .expect("weight proof decodes");
    let summaries = dg_xch_weight_proof::sub_epoch_summaries_of(&wp, &MAINNET)
        .expect("phase-2 summary chain reconstructs and anchors");

    // Corpus sanity (fixture-rot tripwire): contiguous heights, strictly increasing weight.
    for pair in chain.windows(2) {
        assert_eq!(
            pair[1].height(),
            pair[0].height() + 1,
            "corpus heights contiguous"
        );
        assert!(
            pair[1].reward_chain_block.weight > pair[0].reward_chain_block.weight,
            "corpus weight strictly increases"
        );
    }

    // The attested schedule must agree with the slice's known flat epoch parameters.
    let schedule = dg_xch_node::EpochSchedule::from_summaries(
        &summaries,
        MAINNET.sub_epoch_blocks,
        SSI,
        DIFF,
    );
    let first = chain.first().unwrap().height();
    let last = chain.last().unwrap().height();
    assert_eq!(schedule.at(first), (SSI, DIFF));
    assert_eq!(schedule.at(last), (SSI, DIFF));

    // Headers-first replay of the whole slice (the boundary summary attachment included).
    let store = common::new_store().await;
    let engine = Engine::new(Arc::new(store), NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, SyncConfig::default());
    let stored = chaser
        .sync_headers(&chain, &schedule, &summaries)
        .await
        .expect("headers-first replay of the full slice");
    assert_eq!(stored, chain.len(), "every header stored as a candidate");

    // FULL single-header PoW validation at EVERY height past the ancestry warm-up, driven by the
    // attested per-height schedule — the whole-span conformance no per-commit test runs.
    let mut validated = 0usize;
    let mut golden_hits = 0usize;
    for (idx, header) in chain.iter().enumerate() {
        let height = header.height();
        let (ssi, difficulty) = schedule.at(height);
        match chaser.validate_stored_header(header, ssi, difficulty) {
            Ok(required_iters) => {
                validated += 1;
                if let Some(&(_, expected)) = GOLDEN.iter().find(|&&(h, _)| h == height) {
                    assert_eq!(
                        required_iters, expected,
                        "height {height}: required_iters drifted from the weight-proof reference"
                    );
                    golden_hits += 1;
                }
            }
            Err(e) => {
                assert!(
                    idx < ANCESTRY_WARMUP,
                    "height {height} (index {idx}) failed full validation past the ancestry \
                     warm-up: {e}"
                );
            }
        }
    }
    assert!(
        validated >= chain.len() - ANCESTRY_WARMUP,
        "the full span validated ({validated}/{})",
        chain.len()
    );
    assert_eq!(golden_hits, GOLDEN.len(), "both reference goldens reproduced");
    println!(
        "[sentinel] validated {validated}/{} headers ({first}..={last}), goldens {golden_hits}/{}",
        chain.len(),
        GOLDEN.len()
    );
}
