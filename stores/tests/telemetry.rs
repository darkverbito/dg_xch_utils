//! Store-telemetry behavior: the instruments must record as a side effect of real store work
//! — a COMMIT files its latency under the band the store was in, the WAL file gauge sees the WAL,
//! and the near-tip checkpointer records its passes. These are the queries the Grafana panels run,
//! proven against a real SQLite store.

mod common;

use dg_xch_stores::BlockStore;
use std::sync::atomic::Ordering;
use std::time::Duration;

// A writer COMMIT is filed under the CURRENT phase: catch-up commits into commit_catch_up,
// near-tip commits into commit_near_tip — the label the regression query pivots on. The WAL file
// gauge must also see a non-empty `-wal` after WAL-mode commits.
#[tokio::test]
async fn commit_latency_is_filed_under_the_current_phase() {
    let store = common::new_store().await;
    let t = store.telemetry().expect("sqlite records telemetry");
    assert_eq!(t.commit_catch_up.count.load(Ordering::Relaxed), 0);
    assert_eq!(t.commit_near_tip.count.load(Ordering::Relaxed), 0);
    assert_eq!(
        t.last_commit_unix.load(Ordering::Relaxed),
        0,
        "no commit yet"
    );

    // Catch-up band (the default): one batch commit.
    let records = common::load_records();
    let mut batch = store.begin().await.expect("begin");
    store
        .add_block_records_in(&mut batch, &records)
        .await
        .expect("records in batch");
    store.commit(batch).await.expect("commit");
    assert_eq!(t.commit_catch_up.count.load(Ordering::Relaxed), 1);
    assert_eq!(t.commit_near_tip.count.load(Ordering::Relaxed), 0);
    assert!(
        t.last_commit_unix.load(Ordering::Relaxed) > 0,
        "last-commit witness set"
    );

    // Near-tip band: the same commit files under the other label.
    store.set_near_tip(true);
    let mut batch = store.begin().await.expect("begin near tip");
    store
        .add_block_records_in(&mut batch, &records)
        .await
        .expect("records in batch");
    store.commit(batch).await.expect("commit near tip");
    assert_eq!(t.commit_catch_up.count.load(Ordering::Relaxed), 1);
    assert_eq!(t.commit_near_tip.count.load(Ordering::Relaxed), 1);

    // WAL-mode commits land in `-wal` first: the file-size gauge must see them.
    assert!(
        store.wal_bytes() > 0,
        "wal_bytes must observe a non-empty WAL after commits, got 0"
    );
}

// The near-tip-gated checkpointer records its passes: with near_tip=true its 1s tick runs the
// PASSIVE checkpoint and the telemetry must show completed passes and no errors. (2.5s wait for a
// 1s tick — generous against a slow CI runner.)
#[tokio::test]
async fn checkpointer_records_passes_when_near_tip() {
    let store = common::new_store().await;
    let t = store.telemetry().expect("sqlite records telemetry");
    let records = common::load_records();
    let mut batch = store.begin().await.expect("begin");
    store
        .add_block_records_in(&mut batch, &records)
        .await
        .expect("records in batch");
    store.commit(batch).await.expect("commit");

    store.set_near_tip(true);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        t.checkpoint.count.load(Ordering::Relaxed) >= 1,
        "near-tip checkpointer must have completed at least one pass"
    );
    assert_eq!(
        t.checkpoint_errors_total.load(Ordering::Relaxed),
        0,
        "checkpoint passes must not error on a healthy store"
    );
}

// With near_tip=false (catch-up) the checkpointer stays quiet — the phase gate itself.
#[tokio::test]
async fn checkpointer_is_quiet_during_catch_up() {
    let store = common::new_store().await;
    let t = store.telemetry().expect("sqlite records telemetry");
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        t.checkpoint.count.load(Ordering::Relaxed),
        0,
        "catch-up band must not checkpoint"
    );
}

// The pr-verify killer, inverted into a test: during bulk sync the WAL must be drained by SIZE,
// not only by the slow 20-tick cadence. Live, the checkpointer completed zero passes while the
// WAL grew to 1.44 GB — past the ~1 GB in-writer wal_autocheckpoint failsafe, whose blocking
// copy-into-DB then fired INSIDE confirm COMMITs (70-80% of all commit time concentrated in <9%
// of commits, 5-120 s stalls -> frozen peak -> liveness exit 137). With a size-triggered drain
// the crossing itself forces an off-writer checkpoint pass within a tick or two, escalating from
// PASSIVE (copy) to TRUNCATE (reset + shrink) so the file provably comes back under the
// threshold — long before the 20 s cadence, and orders of magnitude before the failsafe.
#[tokio::test]
async fn bulk_wal_past_the_drain_trigger_is_checkpointed_by_size_not_cadence() {
    use dg_xch_stores::CoinStore;

    const TRIGGER: u64 = 64 * 1024; // tiny in-test stand-in for the 128 MiB production trigger
    let path = common::unique_db_path();
    let store = dg_xch_stores::SqliteStore::open_with_wal_drain_trigger(&path, TRIGGER)
        .await
        .expect("open store");
    let t = store.telemetry().expect("sqlite records telemetry");
    assert!(!store.near_tip(), "bulk phase is the default");

    // Grow the WAL past the trigger with real coin applies (each lands in `-wal` first).
    let (adds, _) = common::load_adds_rems(5_000_000);
    let mut height = 10u32;
    while store.wal_bytes() <= TRIGGER {
        store
            .apply_block(height, 0, &adds, &[])
            .await
            .expect("apply");
        // Re-key the batch per height so every pass writes fresh rows, not no-op upserts.
        height += 1;
        assert!(height < 200, "the WAL must grow past the trigger");
    }
    let peak_wal = store.wal_bytes();
    assert!(peak_wal > TRIGGER);

    // Within a few 1 s ticks — far below the 20-tick bulk cadence — the size trigger must have
    // drained AND reset the WAL file back under the threshold, off the writer.
    let mut drained = false;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if store.wal_bytes() < TRIGGER {
            drained = true;
            break;
        }
    }
    assert!(
        drained,
        "size-triggered drain must bring the WAL under the trigger within ~6 s \
         (still {} bytes, was {peak_wal})",
        store.wal_bytes()
    );
    assert!(
        t.checkpoint.count.load(Ordering::Relaxed) >= 1,
        "the drain must be a completed off-writer checkpoint pass"
    );
    assert_eq!(t.checkpoint_errors_total.load(Ordering::Relaxed), 0);

    // The writer stays free: a follow-up commit succeeds immediately on the drained store.
    store
        .apply_block(height, 0, &adds[..8], &[])
        .await
        .expect("post-drain apply");
}

// The writer's page-cache profile follows the sync phase: 256 MiB during bulk catch-up (the
// dirty set of a cross-window batch commit — the largest measured confirm window spilled
// ~240 MiB to the WAL, i.e. the 64 MiB cache was the spill), dropping back to 64 MiB at the
// tip where a single block's dirty set fits. Writer-only: the read pool and checkpointer keep
// the small default, so the bulk profile costs one connection's cache, released at tip.
#[tokio::test]
async fn writer_cache_profile_follows_the_sync_phase() {
    let store = common::new_store().await;
    assert_eq!(
        store.writer_cache_size().await.expect("probe"),
        -262_144,
        "bulk (default) phase opens with the 256 MiB writer cache"
    );

    store.set_near_tip(true);
    let mut near = 0i64;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        near = store.writer_cache_size().await.expect("probe");
        if near == -65_536 {
            break;
        }
    }
    assert_eq!(near, -65_536, "near-tip flip shrinks the writer cache");

    store.set_near_tip(false);
    let mut bulk = 0i64;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        bulk = store.writer_cache_size().await.expect("probe");
        if bulk == -262_144 {
            break;
        }
    }
    assert_eq!(
        bulk, -262_144,
        "falling back to bulk restores the big cache"
    );
}
