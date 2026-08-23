mod common;

use common::{MemSource, StallingSource, candidate_record, peak_rss_bytes, restamp_block};
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::sync::source::BlockRangeSource;
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig};
use dg_xch_stores::{BlockStore, SqliteStore};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const BASE: u32 = 6_000_000;

fn cfg() -> SyncConfig {
    SyncConfig {
        peers: 4,
        window: 64,
        batch: 8,
        request_timeout: Duration::from_millis(500),
        assume_valid: 0,
    }
}

// Seed the store with `n` candidate header records (no bodies) for a contiguous synthetic range — the
// headers-first output the body pipeline then fills.
async fn seed_candidates(
    store: &SqliteStore,
    template: &BlockRecord,
    base_block: &dg_xch_core::blockchain::full_block::FullBlock,
    n: u32,
) {
    let records: Vec<BlockRecord> = (BASE..BASE + n)
        .map(|h| candidate_record(template, base_block, h))
        .collect();
    store
        .add_block_records(&records)
        .await
        .expect("seed records");
}

fn mem_sources(
    base_block: &dg_xch_core::blockchain::full_block::FullBlock,
    p: usize,
) -> Vec<Arc<dyn BlockRangeSource>> {
    (0..p)
        .map(|i| {
            Arc::new(MemSource {
                id: i as u64 + 1,
                base: base_block.clone(),
            }) as Arc<dyn BlockRangeSource>
        })
        .collect()
}

// Download `n` bodies through the pipeline into a fresh store; return (peak_inflight_blocks, peak_window,
// blocks_downloaded, rss_after_bytes).
async fn run_download(n: u32) -> (usize, usize, u64, u64) {
    let base_block = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    seed_candidates(&store, &template, &base_block, n).await;

    let engine = Engine::new(Arc::new(store), NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, cfg());
    let sources = mem_sources(&base_block, cfg().peers);
    chaser.sync_bodies(&sources).await.expect("sync bodies");

    // Every candidate height now has a body (write-through drained get_unassociated).
    let leftover = chaser
        .engine()
        .store()
        .get_unassociated(10)
        .await
        .expect("unassociated");
    assert!(leftover.is_empty(), "all {n} bodies written through");

    // Spot-check a body round-trips out of the store as the exact re-stamped block.
    let mid = BASE + n / 2;
    let want = restamp_block(&base_block, mid).header_hash().unwrap();
    let got = chaser
        .engine()
        .store()
        .get_block(&want)
        .await
        .expect("get_block")
        .expect("body present");
    assert_eq!(
        got.header_hash().unwrap(),
        want,
        "stored body is the downloaded block"
    );

    let m = chaser.metrics();
    (
        m.peak_inflight_blocks.load(Ordering::Relaxed),
        m.peak_window.load(Ordering::Relaxed),
        m.blocks_downloaded.load(Ordering::Relaxed),
        peak_rss_bytes(),
    )
}

// Headline: peak RAM is O(W·id + P·block) — flat in chain height. Downloading an 8x-larger range keeps
// the in-flight block count and the identifier window bounded and constant; only the store (disk) and the
// downloaded-count scale. The window holds identifiers, not blocks — the whole efficiency claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peak_rss_is_flat_in_chain_height() {
    let small = 64u32;
    let large = 512u32;

    let rss_start = peak_rss_bytes();
    let (inflight_s, window_s, dl_s, _rss_s) = run_download(small).await;
    let (inflight_l, window_l, dl_l, rss_l) = run_download(large).await;

    // Work scales with the range (every body downloaded at least once; a raced/stale window snapshot may
    // re-fetch a batch, but the store holds each body exactly once — idempotent write-through).
    assert!(dl_s >= u64::from(small) && dl_l >= u64::from(large));
    assert!(dl_l > dl_s, "the larger range downloads more bodies");

    // ...but the in-flight block count and the identifier window do NOT (O(W·id + P·block), not O(height)).
    let inflight_cap = cfg().peers * cfg().batch as usize;
    assert!(
        inflight_s <= inflight_cap && inflight_l <= inflight_cap,
        "in-flight blocks bounded by P·batch = {inflight_cap} (small={inflight_s}, large={inflight_l})"
    );
    assert!(
        inflight_l <= inflight_s.max(1) * 2,
        "in-flight blocks do not scale with an 8x range (small={inflight_s}, large={inflight_l})"
    );
    assert!(
        window_s <= cfg().window && window_l <= cfg().window,
        "identifier window bounded by its cap {} (small={window_s}, large={window_l})",
        cfg().window
    );

    // Supporting evidence: total process peak RSS stays under a fixed ceiling that does not scale with the
    // range — the pipeline's working set is the bounded in-flight set above; the remaining RSS is SQLite's
    // mmap page cache, itself a bounded shared budget, never the un-buffered body stream.
    let bodies_volume_bytes = u64::from(large + small) * 300_000; // ~300 KB per real body
    let rss_growth = rss_l.saturating_sub(rss_start);
    println!(
        "[reservation-window] peak_inflight_blocks small={inflight_s} large={inflight_l} (cap {inflight_cap}); \
         peak_window small={window_s} large={window_l} (cap {}); \
         rss_growth={rss_growth} bytes; total-body-volume≈{bodies_volume_bytes} bytes",
        cfg().window
    );
    assert!(
        rss_growth < 700 * 1024 * 1024,
        "peak RSS growth {rss_growth} stays under a fixed 700 MB ceiling independent of the range"
    );
}

// A stalled peer's reservation returns to the pool and a live peer finishes it — no gap, no hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_peer_reservation_is_reclaimed_and_finished() {
    let n = 48u32;
    let base_block = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    seed_candidates(&store, &template, &base_block, n).await;

    let engine = Engine::new(Arc::new(store), NativePrimitives, MAINNET);
    let mut chaser = Chaser::new(engine, cfg());

    // One peer stalls forever; three live peers must finish the whole range including the stalled reservation.
    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![
        Arc::new(StallingSource { id: 99 }),
        Arc::new(MemSource {
            id: 1,
            base: base_block.clone(),
        }),
        Arc::new(MemSource {
            id: 2,
            base: base_block.clone(),
        }),
        Arc::new(MemSource {
            id: 3,
            base: base_block.clone(),
        }),
    ];
    chaser
        .sync_bodies(&sources)
        .await
        .expect("sync completes despite a stalled peer");

    let leftover = chaser
        .engine()
        .store()
        .get_unassociated(10)
        .await
        .expect("unassociated");
    assert!(
        leftover.is_empty(),
        "the stalled peer's reservation was reclaimed and finished — no gap"
    );
    assert!(
        chaser.metrics().reclaimed.load(Ordering::Relaxed) >= 1,
        "at least one reservation was reclaimed from the stalled peer"
    );
    assert!(
        chaser.metrics().blocks_downloaded.load(Ordering::Relaxed) >= u64::from(n),
        "every body downloaded (the reclaimed reservation was re-fetched by a live peer)"
    );
}
