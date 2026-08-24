// Staging persistence commit granularity — the catch-up dead-time defect.
//
// `Engine::stage_block_pre` committed the archive writes (record + cold body + status) ONE
// TRANSACTION PER BLOCK (engine.rs `persist_archive` + `store.commit`), unconditionally — while the
// confirm side is already phase-aware (`confirm_staged_batch`: one batch per window in catch-up,
// per-block only near the tip; t160_per_block_confirm.rs pins both). On the SQLite backend every
// staging commit serializes on the single writer connection with an fsync (~100 ms on the iSCSI
// band): 32 sequential round-trips per window of pure dead time between the parallel body
// precompute and the confirm — the measured ~12 s/window catch-up crawl, while the validation work
// itself is ~0.7 s/window.
//
// The fix mirrors the confirm side's model at the staging seam: in the CATCH-UP band
// (`!store.near_tip()`) the window loop (`follow_blocks_reporting`) owns ONE staging transaction
// spanning every `stage_block_pre` of the window; near the tip staging keeps its per-block commit
// (per-block durability is correct AT tip). Validation logic is untouched — only where the COMMIT
// lands.
//
// Written RED against the per-block staging: a fresh catch-up window of N blocks paid N staging
// commits + 1 confirm commit; the batched form pays exactly 2 (one staging, one confirm). The
// commit counts are read from the store's own phase-labelled commit histograms
// (`StoreTelemetry::commit_catch_up` / `commit_near_tip` — every COMMIT on the writer connection
// is recorded there at the fsync-bearing statement, stores/src/sqlite/block.rs `commit`), so the
// assertion counts real writer round-trips, not code-path guesses.

mod common;

use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_stores::BlockStore;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const BASE_WEIGHT: u128 = 1_000_000;
const START: u32 = 100;
const N: u32 = 8;

// A linked chain of re-stamped real mainnet bodies (the torn_peak.rs builder shape): full body
// validation runs real under follow; assume_valid covers the header signatures.
fn build_chain(base: &FullBlock, start: u32, end: u32, prev0: Bytes32) -> Vec<FullBlock> {
    let mut prev = prev0;
    let mut out = Vec::new();
    for h in start..=end {
        let mut b = base.clone();
        b.reward_chain_block.height = h;
        b.reward_chain_block.weight = BASE_WEIGHT + u128::from(h) * 10;
        b.foliage.prev_block_hash = prev;
        prev = b.header_hash().expect("header hash");
        out.push(b);
    }
    out
}

fn cfg() -> SyncConfig {
    SyncConfig {
        peers: 1,
        window: 32,
        batch: 32,
        request_timeout: Duration::from_secs(20),
        assume_valid: 10_000_000,
    }
}

// CATCH-UP band: following one N-block window costs exactly TWO writer commits — one staging
// transaction for the whole window's archive rows, one confirm transaction for coins + peak
// (t160 already pins the confirm half). N per-block staging commits is the defect: N sequential
// fsync round-trips of dead time per window on the single sqlite writer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catch_up_window_stages_in_one_commit() {
    let base = common::load_full_block(5_000_000);
    let chain = build_chain(&base, START, START + N - 1, common::synth_hash(0xaa, 99));

    let store = Arc::new(common::new_store().await);
    let telemetry = store.telemetry().expect("sqlite store exposes telemetry");
    // Catch-up band is the store default (near_tip = false); stated explicitly for the contrast
    // with the near-tip test below.
    store.set_near_tip(false);
    let mut chaser = Chaser::new(Engine::new(store, NativePrimitives, MAINNET), cfg());

    let before = telemetry.commit_catch_up.count.load(Ordering::Relaxed);
    let peak = chaser.follow_blocks(&chain).await.expect("window confirms");
    assert_eq!(
        peak,
        Some((chain.last().unwrap().header_hash().unwrap(), START + N - 1)),
        "the window confirmed to its tip"
    );
    let commits = telemetry.commit_catch_up.count.load(Ordering::Relaxed) - before;
    assert_eq!(
        commits, 2,
        "catch-up window persistence must be ONE staging commit + ONE confirm commit; \
         {N} blocks paying {commits} writer round-trips is the per-block staging serialization \
         (~100 ms fsync each on the iSCSI band = the ~12 s/window dead time)"
    );
}

// NEAR-TIP band: staging keeps its per-block commit (durability per block is CORRECT at tip — the
// liveness clock and the active WAL checkpointer both key on it, exactly as the confirm side's
// per-block near-tip transactions do). One N-block window near tip pays N staging commits + N
// per-block confirm commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn near_tip_window_still_stages_per_block() {
    let base = common::load_full_block(5_000_000);
    let chain = build_chain(&base, START, START + N - 1, common::synth_hash(0xaa, 99));

    let store = Arc::new(common::new_store().await);
    let telemetry = store.telemetry().expect("sqlite store exposes telemetry");
    store.set_near_tip(true);
    let mut chaser = Chaser::new(Engine::new(store, NativePrimitives, MAINNET), cfg());

    let before = telemetry.commit_near_tip.count.load(Ordering::Relaxed);
    let peak = chaser.follow_blocks(&chain).await.expect("window confirms");
    assert_eq!(
        peak,
        Some((chain.last().unwrap().header_hash().unwrap(), START + N - 1)),
        "the window confirmed to its tip"
    );
    let commits = telemetry.commit_near_tip.count.load(Ordering::Relaxed) - before;
    assert_eq!(
        commits,
        u64::from(N) * 2,
        "near-tip: per-block staging commit + per-block confirm commit ({N} blocks)"
    );
}
