// Hardening — the parallel body-download worker must validate a peer's `RespondBlocks` batch
// against the headers-first candidate chain BEFORE it is written through. A `RespondBlocks` is
// untrusted: a lying/hostile peer can (a) answer with a different height range, (b) return a
// non-contiguous / short batch, (c) re-stamp a body (serve a real neighbouring block relabelled to a
// requested height), or (d) on the from-empty / anchor path, serve a first block that does not connect
// to the weight-proof-attested anchor header. The returned blocks must be the requested heights,
// contiguous, and connect — never "trust because empty".
//
// Our architecture makes the candidate chain the connect target: the headers-first pass has already
// stored the WP-attested candidate record for every reserved height (`sync_header_chain`), and
// that chain was validated to be contiguous and anchor-connected. So a batch is sound iff it covers
// EXACTLY the reserved range (ascending, contiguous) and every body's `header_hash` binds to a committed
// candidate record AT its own height. These tests pin that: a deceitful batch is reclaimed like any miss
// and NEVER reaches `append_many`, the confirm pipeline, or the from-empty prev=None entry; a legitimate
// batch still proceeds (the critical regression — real genesis/anchor sync must not break).

mod common;

use async_trait::async_trait;
use common::{MemSource, candidate_record, restamp_block, synth_hash};
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::sync::source::BlockRangeSource;
use dg_xch_node::{Chaser, Engine, NativePrimitives, SyncConfig, SyncError};
use dg_xch_stores::{BlockStore, SqliteStore};
use std::sync::Arc;
use std::time::Duration;

const BASE: u32 = 6_000_000;

fn cfg() -> SyncConfig {
    SyncConfig {
        peers: 2,
        window: 16,
        batch: 4,
        request_timeout: Duration::from_millis(300),
        assume_valid: 0,
    }
}

// A body at the correct height whose header_hash matches NO seeded candidate — the re-stamp / forgery
// shape. `restamp_block` keys candidate records on the 0xcd prev-tag; a 0xEE tag gives a distinct
// foliage and therefore a distinct header_hash that binds to no candidate at any height.
fn foreign_body(base: &FullBlock, h: u32) -> FullBlock {
    let mut b = restamp_block(base, h);
    b.foliage.prev_block_hash = synth_hash(0xEE, h);
    b
}

// The ways a hostile peer can answer a `fetch_range(start, end)` with a batch that must be rejected.
#[derive(Clone, Copy, Debug)]
enum Deceit {
    // Serve blocks for `start+shift ..= end+shift` — the requested-vs-returned height RANGE is
    // wrong. Fails the coverage check.
    ShiftRange(u32),
    // Serve the range minus its middle height — a short, non-contiguous batch. Fails the coverage check.
    DropMiddle,
    // Serve bodies at the right heights that hash to NO candidate — a re-stamped/forged body. Fails the
    // header-binding check.
    ForeignBodies,
    // Serve real neighbour bodies (`h+k`) relabelled to the requested height `h`: `height()` passes the
    // coverage check but the body binds to a candidate at a DIFFERENT height (h+k). Fails header-binding.
    RelabelNeighbour(u32),
}

struct DeceitfulSource {
    id: u64,
    base: FullBlock,
    mode: Deceit,
}

#[async_trait]
impl BlockRangeSource for DeceitfulSource {
    fn peer_id(&self) -> u64 {
        self.id
    }
    fn is_closed(&self) -> bool {
        false
    }
    async fn fetch_range(&self, start: u32, end: u32) -> Result<Vec<FullBlock>, SyncError> {
        let blocks = match self.mode {
            Deceit::ShiftRange(s) => (start..=end)
                .map(|h| restamp_block(&self.base, h + s))
                .collect(),
            Deceit::DropMiddle => {
                let mid = start + (end - start) / 2;
                (start..=end)
                    .filter(|h| *h != mid)
                    .map(|h| restamp_block(&self.base, h))
                    .collect()
            }
            Deceit::ForeignBodies => (start..=end).map(|h| foreign_body(&self.base, h)).collect(),
            Deceit::RelabelNeighbour(k) => (start..=end)
                .map(|h| {
                    let mut b = restamp_block(&self.base, h + k);
                    b.reward_chain_block.height = h; // pass the coverage height check, fail header-binding
                    b
                })
                .collect(),
        };
        Ok(blocks)
    }
}

async fn seed_candidates(store: &SqliteStore, template: &BlockRecord, base: &FullBlock, n: u32) {
    let records: Vec<BlockRecord> = (BASE..BASE + n)
        .map(|h| candidate_record(template, base, h))
        .collect();
    store
        .add_block_records(&records)
        .await
        .expect("seed records");
}

fn chaser_with(store: SqliteStore) -> Chaser<Arc<SqliteStore>, NativePrimitives> {
    let engine = Engine::new(Arc::new(store), NativePrimitives, MAINNET);
    Chaser::new(engine, cfg())
}

// CASE 1 — a peer that answers with a DIFFERENT height range (heights shifted by +1000) is rejected: the
// batch never drains a reserved height, so with only that peer the window cannot fill and the sync
// surfaces `Exhausted` FAST (bounded by the failure budget), never wedging on an endlessly re-accepted
// wrong-range batch. Without the coverage check the off-range bodies are appended and the
// reservation is marked complete, but the reserved candidates stay unfilled → the worker
// re-reserves and re-accepts forever (a wedge).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_range_batch_is_rejected_fast() {
    let base = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    seed_candidates(&store, &template, &base, 8).await;
    let mut chaser = chaser_with(store);

    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![Arc::new(DeceitfulSource {
        id: 1,
        base: base.clone(),
        mode: Deceit::ShiftRange(1000),
    })];

    let result = tokio::time::timeout(Duration::from_secs(8), chaser.sync_bodies(&sources))
        .await
        .expect(
            "a wrong-range peer must fail the sync FAST, never wedge on re-accepted bad batches",
        );
    assert!(
        matches!(result, Err(SyncError::Exhausted(_))),
        "a wrong-range-only peer must drain nothing and surface Exhausted, got {result:?}"
    );
    // The off-range bodies (heights BASE+1000..) the peer served must NEVER have been written.
    for h in BASE..BASE + 8 {
        let off = restamp_block(&base, h + 1000).header_hash().unwrap();
        assert!(
            chaser
                .engine()
                .store()
                .get_block(&off)
                .await
                .unwrap()
                .is_none(),
            "an off-range body (height {}) must not be written to the store",
            h + 1000
        );
    }
}

// CASE 2 — a non-contiguous / short batch (the reserved range minus its middle height) is rejected on the
// coverage check: same fast-Exhausted posture. Without the check: the short batch is appended and the
// reservation completed while the dropped height stays unfilled → re-reserve/re-accept wedge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_contiguous_batch_is_rejected_fast() {
    let base = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    seed_candidates(&store, &template, &base, 8).await;
    let mut chaser = chaser_with(store);

    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![Arc::new(DeceitfulSource {
        id: 1,
        base: base.clone(),
        mode: Deceit::DropMiddle,
    })];

    let result = tokio::time::timeout(Duration::from_secs(8), chaser.sync_bodies(&sources))
        .await
        .expect("a non-contiguous peer must fail the sync FAST, never wedge");
    assert!(
        matches!(result, Err(SyncError::Exhausted(_))),
        "a non-contiguous-only peer must surface Exhausted, got {result:?}"
    );
}

// CASE 3 — a re-stamped / forged body (right height, hashes to no candidate) is NEVER written to the
// store. A good peer runs alongside so the sync terminates by draining the real candidates; the forged
// bodies the hostile peer served must be absent, and the real bodies present. Without the check: the forged
// bodies are appended (get_block returns them) — the soundness gap that would then feed re-stamped bodies
// into validation / the from-empty entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restamped_foreign_body_is_never_written() {
    let base = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    seed_candidates(&store, &template, &base, 8).await;
    let mut chaser = chaser_with(store);

    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![
        Arc::new(DeceitfulSource {
            id: 99,
            base: base.clone(),
            mode: Deceit::ForeignBodies,
        }),
        Arc::new(MemSource {
            id: 1,
            base: base.clone(),
        }),
    ];

    tokio::time::timeout(Duration::from_secs(30), chaser.sync_bodies(&sources))
        .await
        .expect("the sync must terminate (the good peer drains the real candidates)")
        .expect("sync_bodies completes beside the forging peer");

    let store = chaser.engine().store();
    for h in BASE..BASE + 8 {
        let forged = foreign_body(&base, h).header_hash().unwrap();
        assert!(
            store.get_block(&forged).await.unwrap().is_none(),
            "a forged body (height {h}) that binds to no candidate must never be written"
        );
        let real = restamp_block(&base, h).header_hash().unwrap();
        assert!(
            store.get_block(&real).await.unwrap().is_some(),
            "the real candidate body (height {h}) must be filled by the good peer"
        );
    }
    assert!(
        store.get_unassociated(10).await.unwrap().is_empty(),
        "every reserved candidate is filled by the good peer — no gap"
    );
}

// CASE 2b — a re-stamped REAL neighbour (block h+1000 relabelled to height h) passes the naive height
// check but binds to a candidate at the WRONG height, and is rejected: its (out-of-window) hash is never
// written. Without the check: the relabelled body is appended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relabelled_neighbour_body_is_rejected() {
    let base = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    seed_candidates(&store, &template, &base, 8).await;
    let mut chaser = chaser_with(store);

    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![
        Arc::new(DeceitfulSource {
            id: 99,
            base: base.clone(),
            mode: Deceit::RelabelNeighbour(1000),
        }),
        Arc::new(MemSource {
            id: 1,
            base: base.clone(),
        }),
    ];

    tokio::time::timeout(Duration::from_secs(30), chaser.sync_bodies(&sources))
        .await
        .expect("the sync must terminate")
        .expect("sync_bodies completes beside the re-stamping peer");

    let store = chaser.engine().store();
    for h in BASE..BASE + 8 {
        // The relabelled body carries block (h+1000)'s header_hash — an out-of-window height that is not a
        // seeded candidate, so it must be rejected and never written.
        let relabelled = restamp_block(&base, h + 1000).header_hash().unwrap();
        assert!(
            store.get_block(&relabelled).await.unwrap().is_none(),
            "a neighbour body relabelled to height {h} must be rejected (binds to a candidate at h+1000)"
        );
    }
    assert!(
        store.get_unassociated(10).await.unwrap().is_empty(),
        "the good peer still fills every reserved candidate"
    );
}

// CASE 3 (from-empty / anchor) — from an EMPTY store (no peak) carrying only the anchor's headers-first
// candidate record, a first block that does NOT connect to that WP-attested anchor header is rejected:
// the forged anchor body is never written, and the peak never advances off empty. A good peer fills the
// real anchor so the sync terminates. This is the core of the bug: the from-empty path must validate
// against the anchor header, NOT trust the first body because there is no in-store parent. RED
// (pre-fix): the forged anchor body is appended and would flow into the prev=None confirm entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn from_empty_anchor_rejects_a_nonconnecting_first_block() {
    let base = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    // ONE anchor candidate, no body, no peak — the exact from-empty precondition.
    store
        .add_block_records(&[candidate_record(&template, &base, BASE)])
        .await
        .expect("seed anchor candidate");
    let mut chaser = chaser_with(store);
    assert_eq!(
        chaser.engine().store().get_peak().await.unwrap(),
        None,
        "precondition: empty store, no confirmed peak"
    );

    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![
        Arc::new(DeceitfulSource {
            id: 99,
            base: base.clone(),
            mode: Deceit::ForeignBodies,
        }),
        Arc::new(MemSource {
            id: 1,
            base: base.clone(),
        }),
    ];

    tokio::time::timeout(Duration::from_secs(30), chaser.sync_bodies(&sources))
        .await
        .expect("the sync must terminate (the good peer fills the anchor)")
        .expect("sync_bodies completes");

    let store = chaser.engine().store();
    let forged = foreign_body(&base, BASE).header_hash().unwrap();
    assert!(
        store.get_block(&forged).await.unwrap().is_none(),
        "a from-empty anchor body that does not match the WP-attested candidate must never be written"
    );
    let real = restamp_block(&base, BASE).header_hash().unwrap();
    assert!(
        store.get_block(&real).await.unwrap().is_some(),
        "the legitimate anchor body (matching the candidate) is filled by the good peer"
    );
    assert_eq!(
        store.get_peak().await.unwrap(),
        None,
        "a body download never confirms — the peak stays empty (no forged advance)"
    );
}

// REGRESSION (critical) — the LEGITIMATE from-empty / anchor sync still works: from an empty store with
// only the anchor candidate, a single honest peer serving the matching body fills the anchor. The
// hardening must reject forgeries WITHOUT breaking real genesis/anchor sync.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn from_empty_anchor_still_accepts_the_legit_body() {
    let base = common::load_full_block(5_000_000);
    let template = common::load_records()[0].clone();
    let store = common::new_store().await;
    // The candidate is keyed on the served body's header_hash: the honest MemSource serves
    // restamp_block(base, BASE) for the anchor height, and candidate_record matches it by construction.
    store
        .add_block_records(&[candidate_record(&template, &base, BASE)])
        .await
        .expect("seed anchor candidate");
    let mut chaser = chaser_with(store);

    let sources: Vec<Arc<dyn BlockRangeSource>> = vec![Arc::new(MemSource {
        id: 1,
        base: base.clone(),
    })];

    tokio::time::timeout(Duration::from_secs(30), chaser.sync_bodies(&sources))
        .await
        .expect("the legitimate anchor sync must terminate")
        .expect("sync_bodies completes on a legit anchor batch");

    let store = chaser.engine().store();
    let real = restamp_block(&base, BASE).header_hash().unwrap();
    assert!(
        store.get_block(&real).await.unwrap().is_some(),
        "the legitimate anchor body must be written — real from-empty sync is not broken"
    );
    assert!(
        store.get_unassociated(10).await.unwrap().is_empty(),
        "the anchor candidate is filled — the from-empty window drains"
    );
}
