// The restart-resume record-floor decision: can the next possible epoch retarget's deepest record
// read (`epoch_backfill_low`) be served from the store, or does the resume repair need to fetch a
// weight proof and backfill headers first?
//
// The floor MUST be measured by prev-hash walk from the peak, for two reasons a real
// anchored-store restart-resume stall exposed:
//   1. `get_block_record_by_height` is main-chain-only on every backend (sqlite/postgres
//      `in_main_chain = 1`; mmap `heights.dat` is populated only by `set_peak`). Epoch-depth
//      backfill writes CANDIDATE records, which a by-height read never sees — so an anchored leg's
//      by-height floor sat at the confirmed span base forever, and every restart re-ran the full
//      weight-proof fetch + multi-minute validation + header backfill it had already done.
//   2. A by-height binary search assumes hole-free monotone presence. A mid-span record hole (a
//      lost candidate link batch on mmap; a `synchronous_commit=off` suffix drop on Postgres)
//      above the by-height floor read as "nothing to repair" while the stage walk kept dying on
//      the hole — the MissingRecord livelock.
// The prev-hash walk reads records BY HASH (candidate-visible on every backend) and detects a hole
// naturally: the walk breaks exactly at the record whose parent is missing.

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_stores::BlockStore;
use dg_xch_stores::error::StoreError;

// Step bound for the floor walk: the needed span plus slack. Heights decrease by exactly one per
// prev-hash link on a well-formed chain, so this is never the limiting factor there — it only
// guarantees termination against a corrupt store (a prev-hash cycle), reported as Broken.
const FLOOR_WALK_SLACK: u32 = 16;

/// The outcome of the record-floor walk from the peak toward `needed_low`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordFloor {
    /// The prev-hash chain from the peak reaches `needed_low` (or genesis) unbroken: the next
    /// epoch retarget's deepest read is servable. `floor` is the height at which the walk crossed
    /// `needed_low` — the deepen anchor when a re-armed repair must reach further down.
    Reached { floor: u32 },
    /// The chain breaks above `needed_low`: `stop` is the lowest record still reachable from the
    /// peak; the record below it is missing (an anchored span's un-backfilled base, or a mid-span
    /// hole). Backfilling headers anchored at `stop` covers the missing span.
    Broken { stop: u32 },
}

/// Measure the record floor for a resume over peak `(peak, local)`.
///
/// # Errors
/// Returns [`StoreError`] on a store read failure.
pub(crate) async fn measure_record_floor<S: BlockStore + Send + Sync + ?Sized>(
    store: &S,
    peak: Bytes32,
    local: u32,
    needed_low: u32,
) -> Result<RecordFloor, StoreError> {
    // Bounded: heights decrease by exactly one per link on a well-formed chain, so the walk takes
    // at most `local - needed_low + 1` steps; the slack only bounds a corrupt store (see above).
    let max_steps = u64::from(local.saturating_sub(needed_low)) + u64::from(FLOOR_WALK_SLACK);
    let mut hash = peak;
    let mut lowest_reached = local.saturating_add(1);
    let mut steps = 0u64;
    loop {
        if steps > max_steps {
            // A prev-hash cycle or a non-decreasing height chain: report the break at the lowest
            // height legitimately reached so the repair re-fetches below it.
            return Ok(RecordFloor::Broken {
                stop: lowest_reached,
            });
        }
        let Some(rec) = store.get_block_record(&hash).await? else {
            return Ok(RecordFloor::Broken {
                stop: lowest_reached,
            });
        };
        if rec.height <= needed_low {
            return Ok(RecordFloor::Reached { floor: rec.height });
        }
        lowest_reached = lowest_reached.min(rec.height);
        hash = rec.prev_hash;
        steps += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::blockchain::block_record::BlockRecord;
    use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
    use dg_xch_core::blockchain::vdf_output::VdfOutput;
    use dg_xch_core::consensus::constants::MAINNET;
    #[cfg(feature = "mmap")]
    use dg_xch_stores::MmapStore;
    use dg_xch_stores::SqliteStore;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn h32(n: u32) -> Bytes32 {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        Bytes32::from(b)
    }

    fn record(height: u32) -> BlockRecord {
        BlockRecord {
            header_hash: h32(height),
            prev_hash: h32(height.wrapping_sub(1)),
            height,
            weight: 7 * u128::from(height),
            total_iters: 10_000_000 * u128::from(height),
            signage_point_index: 0,
            challenge_vdf_output: VdfOutput {
                data: UnsizedBytes::new(vec![]),
            },
            infused_challenge_vdf_output: None,
            reward_infusion_new_challenge: Bytes32::default(),
            challenge_block_info_hash: Bytes32::default(),
            sub_slot_iters: MAINNET.sub_slot_iters_starting,
            pool_puzzle_hash: Bytes32::default(),
            farmer_puzzle_hash: Bytes32::default(),
            required_iters: 1,
            deficit: 0,
            overflow: false,
            prev_transaction_block_height: height.wrapping_sub(1),
            timestamp: Some(1_000 + u64::from(height)),
            prev_transaction_block_hash: None,
            fees: None,
            reward_claims_incorporated: None,
            finished_challenge_slot_hashes: None,
            finished_infused_challenge_slot_hashes: None,
            finished_reward_slot_hashes: None,
            sub_epoch_summary_included: None,
        }
    }

    fn records(range: impl Iterator<Item = u32>) -> Vec<BlockRecord> {
        range.map(record).collect()
    }

    async fn sqlite_store() -> SqliteStore {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "resume_floor_{}_{nanos}.sqlite",
            std::process::id()
        ));
        SqliteStore::open(&path).await.expect("open sqlite")
    }

    #[cfg(feature = "mmap")]
    async fn mmap_store() -> MmapStore {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("resume_floor_{}_{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        MmapStore::open(&dir).await.expect("open mmap")
    }

    // The anchored-leg shape a deployed anchored store is in: the confirmed span [A, P] is
    // main-chain (set_peak ran while nothing below A existed), then the epoch-depth backfill
    // landed CANDIDATE records [L, A). A restart must see those candidates as floor-satisfying —
    // by-height they are invisible, and the pre-fix decision re-ran the full weight-proof fetch +
    // validate on EVERY boot. (Postgres shares sqlite's `in_main_chain = 1` by-height SQL
    // verbatim — stores/src/postgres/block.rs — and the walk goes through the same generic
    // `BlockStore::get_block_record`, so sqlite+mmap coverage carries; the postgres contract
    // suite pins the by-hash/by-height visibility semantics themselves.)
    async fn anchored_shape<S: BlockStore + Send + Sync>(store: &S) {
        const L: u32 = 9_400;
        const A: u32 = 9_700;
        const P: u32 = 10_000;
        store
            .add_block_records(&records(A..=P))
            .await
            .expect("seed confirmed span");
        store.set_peak(&h32(P)).await.expect("set peak");
        // The backfill lands AFTER the peak exists — candidates, never flipped main.
        store
            .add_block_records(&records(L..A))
            .await
            .expect("seed backfilled candidates");
    }

    async fn assert_anchored_reached<S: BlockStore + Send + Sync>(store: &S) {
        const P: u32 = 10_000;
        const NEEDED_LOW: u32 = 9_500;
        let got = measure_record_floor(store, h32(P), P, NEEDED_LOW)
            .await
            .expect("measure");
        assert!(
            matches!(got, RecordFloor::Reached { floor } if floor <= NEEDED_LOW),
            "backfilled candidate records must satisfy the floor walk \
             (no weight-proof refetch on a clean restart), got {got:?}"
        );
    }

    #[tokio::test]
    async fn anchored_backfilled_candidates_satisfy_the_floor_on_sqlite() {
        let store = sqlite_store().await;
        anchored_shape(&store).await;
        assert_anchored_reached(&store).await;
    }

    #[cfg(feature = "mmap")]
    #[tokio::test]
    async fn anchored_backfilled_candidates_satisfy_the_floor_on_mmap() {
        let store = mmap_store().await;
        anchored_shape(&store).await;
        assert_anchored_reached(&store).await;
    }

    // The mid-span-hole shape (HOLE 3): records main-chain-visible on BOTH sides of a single
    // missing record (a lost mmap link batch / a dropped Postgres candidate batch, later peaks
    // landed). The by-height floor search converges below the hole and reports "nothing to
    // repair" while every stage walk from the peak dies crossing the hole — the livelock. The
    // walk must report Broken exactly at the hole so the repair backfills THE HOLE.
    const HOLE_P: u32 = 1_024;
    const HOLE_H: u32 = HOLE_P - 3; // never a binary-search probe point on the all-present descent
    async fn hole_shape<S: BlockStore + Send + Sync>(store: &S) {
        store
            .add_block_records(&records(0..HOLE_H))
            .await
            .expect("seed below hole");
        store.set_peak(&h32(HOLE_H - 1)).await.expect("peak below");
        store
            .add_block_records(&records(HOLE_H + 1..=HOLE_P))
            .await
            .expect("seed above hole");
        // set_peak's prev-hash link walk breaks at the hole: [H+1, P] flip main, H stays missing.
        store.set_peak(&h32(HOLE_P)).await.expect("peak above");
    }

    async fn assert_hole_detected_and_converges<S: BlockStore + Send + Sync>(store: &S) {
        const NEEDED_LOW: u32 = 100;
        let got = measure_record_floor(store, h32(HOLE_P), HOLE_P, NEEDED_LOW)
            .await
            .expect("measure");
        assert_eq!(
            got,
            RecordFloor::Broken { stop: HOLE_H + 1 },
            "a mid-span hole above the by-height floor must be DETECTED (Broken at the hole), \
             not read as nothing-to-repair"
        );
        // The repair backfills the hole (headers anchored at `stop` cover it); once the record
        // lands, the same walk must converge to Reached.
        store
            .add_block_records(&records(HOLE_H..=HOLE_H))
            .await
            .expect("backfill the hole");
        let after = measure_record_floor(store, h32(HOLE_P), HOLE_P, NEEDED_LOW)
            .await
            .expect("measure after backfill");
        assert!(
            matches!(after, RecordFloor::Reached { floor } if floor <= NEEDED_LOW),
            "after the hole is backfilled the floor walk must converge, got {after:?}"
        );
    }

    #[tokio::test]
    async fn mid_span_hole_is_detected_and_repair_converges_on_sqlite() {
        let store = sqlite_store().await;
        hole_shape(&store).await;
        assert_hole_detected_and_converges(&store).await;
    }

    #[cfg(feature = "mmap")]
    #[tokio::test]
    async fn mid_span_hole_is_detected_and_repair_converges_on_mmap() {
        let store = mmap_store().await;
        hole_shape(&store).await;
        assert_hole_detected_and_converges(&store).await;
    }

    // Regression guard: a clean full-history chain (the genesis-synced sqlite legs that never
    // stalled) short-circuits exactly as before.
    #[tokio::test]
    async fn clean_genesis_chain_reaches_the_floor_on_sqlite() {
        let store = sqlite_store().await;
        store
            .add_block_records(&records(0..=600))
            .await
            .expect("seed");
        store.set_peak(&h32(600)).await.expect("peak");
        let got = measure_record_floor(&store, h32(600), 600, 300)
            .await
            .expect("measure");
        assert!(
            matches!(got, RecordFloor::Reached { floor } if floor <= 300),
            "clean chain must reach the floor, got {got:?}"
        );
    }

    // The walk is bounded even against a corrupt self-referential chain: it terminates Broken
    // rather than spinning (bound = the needed span + slack). No `set_peak` here — the store's
    // own peak walk assumes an acyclic chain; the floor walk takes the peak explicitly and must
    // stay bounded regardless.
    #[tokio::test]
    async fn corrupt_prev_hash_cycle_terminates_broken() {
        let store = sqlite_store().await;
        let peak = record(500);
        let mut cyc = record(499);
        cyc.prev_hash = cyc.header_hash; // self-loop below the peak
        store
            .add_block_records(&[peak.clone(), cyc])
            .await
            .expect("seed cycle");
        let got = measure_record_floor(&store, peak.header_hash, 500, 100)
            .await
            .expect("measure");
        assert!(
            matches!(got, RecordFloor::Broken { .. }),
            "a prev-hash cycle must terminate Broken, got {got:?}"
        );
    }
}
