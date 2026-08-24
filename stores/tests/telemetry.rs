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
