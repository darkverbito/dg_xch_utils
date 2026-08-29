// Long reorgs at scale. There is NO reorg-depth cap: fork choice is weight-only and the coin
// rollback is unfloored, so a reorg ~1500 deep is legal. A fabricated in-memory reorg horizon
// would silently refuse valid reorgs; the fork walk is store-backed instead. These suites pin
// the contract at depth 100 and 1000, on every backend:
//
//   - fork choice is weight-only at any depth (no refusal, no cap trip),
//   - the coin unwind is EXACT: every abandoned-branch coin deleted, every spend of a
//     below-fork coin reverted to unspent, the winning branch's coins byte-equal a from-scratch
//     replay of the winning chain (the `tests/reorg.rs` replay-equality invariant, at depth),
//   - the whole flip is ONE store transaction (`rollback_to_in` + branch re-applies +
//     `set_peak_in` + commit): a crash at the peak-flip seam leaves the store EXACTLY at the
//     pre-reorg state, reopenable from disk, and the retry lands, and
//   - the wallet/announcer side effect (the ReorgReport) surfaces only AFTER the commit — a
//     failed flip reports nothing.
//
// Branch construction is the `tests/reorg.rs` delta convention scaled up with a per-height coin
// lineage (common/lineage.rs). `add_delta` drives the engine's real fork-choice/confirm/reorg
// path against the real store; body validation at bulk-entry depth is exercised by
// tests/deep_fork_bulk_entry.rs — at-scale coin/atomicity semantics are what these pin.

mod common;

use common::fault::FaultStore;
use common::lineage::{
    A_TAG, FORK, assert_equals_replay, build_branches, fork_coin, lineage_coin, touched_names,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::engine::BlockDelta;
use dg_xch_node::{AddBlockOutcome, Engine, NativePrimitives};
use dg_xch_stores::{BlockStore, CoinStore, SqliteStore};
use std::sync::atomic::Ordering;

// The generic at-scale drive: confirm base + branch A (the peak), park ALL of branch B as
// orphans, then flip on B's tip. Weight-only fork choice at depth n, exact coin unwind,
// post-commit-only reporting.
async fn run_long_reorg<S: CoinStore + BlockStore + Sync>(store: S, n: u32) {
    let br = build_branches(n);
    let mut engine = Engine::new(store, NativePrimitives, MAINNET);

    assert_eq!(
        engine.add_delta(br.base.clone()).await.unwrap(),
        AddBlockOutcome::NewPeak { height: FORK }
    );
    for d in &br.a {
        assert_eq!(
            engine.add_delta(d.clone()).await.unwrap(),
            AddBlockOutcome::Extended { height: d.height }
        );
    }
    // Every branch-B block below the tip is lighter than the A peak: all n park as orphans —
    // no depth cap trips, no refusal, and NOTHING is reported (the chain has not changed).
    for d in &br.b[..n as usize] {
        assert_eq!(
            engine.add_delta(d.clone()).await.unwrap(),
            AddBlockOutcome::Orphan { height: d.height },
            "a lighter branch block at depth {} parks as an orphan candidate",
            d.height - FORK
        );
    }
    assert!(
        engine.pop_reorg_report().is_none(),
        "no wallet-facing report before any reorg lands"
    );

    // The (n+1)-th branch block outweighs the peak: ONE weight-only reorg at depth n+1.
    let tip = br.b.last().unwrap().clone();
    match engine.add_delta(tip.clone()).await.unwrap() {
        AddBlockOutcome::Reorg { fork_height, .. } => {
            assert_eq!(
                fork_height, FORK,
                "the store walk found the true fork point"
            );
        }
        other => panic!("expected a depth-{} reorg, got {other:?}", n + 1),
    }

    let names = touched_names(n);
    let chain: Vec<&BlockDelta> = std::iter::once(&br.base).chain(br.b.iter()).collect();
    assert_equals_replay(
        engine.store(),
        (tip.header_hash, FORK + n + 1),
        &chain,
        &names,
        &format!("after the depth-{} flip", n + 1),
    )
    .await;

    // The wallet-facing summary surfaced exactly once, after the commit, and carries the whole
    // unwind: the true fork height, the full re-applied branch in height order, every
    // abandoned-branch coin reverted to not-on-chain, and the fork-common coin (created below
    // the fork, spent on the abandoned branch) reverted to unspent.
    let report = engine
        .pop_reorg_report()
        .expect("exactly one report per landed reorg");
    assert!(
        engine.pop_reorg_report().is_none(),
        "FIFO drains one report"
    );
    assert_eq!(report.fork_height, FORK);
    assert_eq!(
        report
            .reapplied
            .iter()
            .map(|d| (d.height, d.header_hash))
            .collect::<Vec<_>>(),
        br.b.iter()
            .map(|d| (d.height, d.header_hash))
            .collect::<Vec<_>>(),
        "the winning branch fork+1..=tip in height order, all {} blocks",
        n + 1
    );
    let fc = report
        .rolled_back
        .iter()
        .find(|r| r.coin == fork_coin())
        .expect("the abandoned branch's spend of the fork-common coin is rolled back");
    assert_eq!(fc.spent_block_index, 0, "unspent again");
    assert!(!fc.spent);
    assert_eq!(fc.confirmed_block_index, FORK, "its creation stands");
    for h in FORK + 1..=FORK + n {
        let rolled = report
            .rolled_back
            .iter()
            .find(|r| r.coin == lineage_coin(A_TAG, h))
            .unwrap_or_else(|| panic!("abandoned lineage coin at height {h} in the report"));
        assert_eq!(rolled.confirmed_block_index, 0, "no longer on chain");
        assert_eq!(rolled.timestamp, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_reorg_100_deep_flips_atomically_with_exact_coin_unwind() {
    run_long_reorg(common::new_store().await, 100).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_reorg_1000_deep_flips_atomically_with_exact_coin_unwind() {
    run_long_reorg(common::new_store().await, 1000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mmap_reorg_100_deep_flips_atomically_with_exact_coin_unwind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dg_xch_stores::MmapStore::open(dir.path())
        .await
        .expect("open mmap store");
    run_long_reorg(store, 100).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mmap_reorg_1000_deep_flips_atomically_with_exact_coin_unwind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dg_xch_stores::MmapStore::open(dir.path())
        .await
        .expect("open mmap store");
    run_long_reorg(store, 1000).await;
}

// The crash contract at depth: the flip is ONE transaction, so a store fault at the LAST
// statement (the peak flip — after the rollback and all n+1 branch re-applies already executed
// inside the transaction) must leave the store EXACTLY at the pre-reorg state — durable across
// a process restart (the reopen) — report nothing, and the in-process retry must land the full
// flip, also durable across a reopen. This is `tests/reorg.rs`
// `crash_before_peak_flip_leaves_store_untouched_and_retry_succeeds` at depth 100 plus the
// kill-point reopen the restart-resume suite owes (the store file IS the crash artifact:
// dropping the failed batch without commit is what a killed process leaves in the WAL).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_crash_at_the_peak_flip_of_a_100_deep_reorg_is_atomic_and_recoverable() {
    const N: u32 = 100;
    let path = common::unique_db_path();
    let inner = SqliteStore::open(&path).await.expect("open");
    let (store, _fail_apply, fail_set_peak) = FaultStore::new(inner);
    let br = build_branches(N);
    let mut engine = Engine::new(store, NativePrimitives, MAINNET);

    engine.add_delta(br.base.clone()).await.unwrap();
    for d in &br.a {
        engine.add_delta(d.clone()).await.unwrap();
    }
    for d in &br.b[..N as usize] {
        engine.add_delta(d.clone()).await.unwrap();
    }
    let a_tip = br.a.last().unwrap().clone();
    let b_tip = br.b.last().unwrap().clone();
    let names = touched_names(N);
    let a_chain: Vec<&BlockDelta> = std::iter::once(&br.base).chain(br.a.iter()).collect();
    let b_chain: Vec<&BlockDelta> = std::iter::once(&br.base).chain(br.b.iter()).collect();

    // The crash: the injected fault fires on `set_peak_in` INSIDE the reorg transaction — the
    // seam between the rollback+re-applies and the pointer flip. The transaction never commits.
    fail_set_peak.store(true, Ordering::Relaxed);
    engine
        .add_delta(b_tip.clone())
        .await
        .expect_err("injected set_peak fault surfaces");
    assert!(
        engine.pop_reorg_report().is_none(),
        "a failed flip must report NOTHING to wallet subscribers (post-commit only)"
    );
    assert_equals_replay(
        engine.store(),
        (a_tip.header_hash, FORK + N),
        &a_chain,
        &names,
        "after the crashed deep flip",
    )
    .await;

    // The in-process retry (the branch is still in the engine's pending overlay) lands the flip.
    fail_set_peak.store(false, Ordering::Relaxed);
    match engine.add_delta(b_tip.clone()).await.unwrap() {
        AddBlockOutcome::Reorg { fork_height, .. } => assert_eq!(fork_height, FORK),
        other => panic!("expected the retried deep reorg, got {other:?}"),
    }
    assert_equals_replay(
        engine.store(),
        (b_tip.header_hash, FORK + N + 1),
        &b_chain,
        &names,
        "after the retried deep flip",
    )
    .await;

    // The kill: drop everything and reopen the store file cold. The landed flip is durable and
    // consistent — no torn state survives into the restart.
    drop(engine);
    let reopened = SqliteStore::open(&path).await.expect("reopen after kill");
    assert_equals_replay(
        &reopened,
        (b_tip.header_hash, FORK + N + 1),
        &b_chain,
        &names,
        "after reopen",
    )
    .await;
}

// ── Postgres: the identical at-scale contract on the multi-writer SQL backend ────────────────
// Env-gated on a DEDICATED test database (the tests truncate its tables), exactly like
// stores/tests/postgres_contract.rs:
//   DGXCH_PG_URL=postgres://user:pass@host/db cargo test -p dg_xch_node --features postgres \
//     --test long_reorg_scale -- --ignored --test-threads=1

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use dg_xch_stores::PostgresStore;

    async fn open_clean() -> PostgresStore {
        let url =
            std::env::var("DGXCH_PG_URL").expect("set DGXCH_PG_URL to a dedicated test database");
        let store = PostgresStore::open(&url).await.expect("open postgres");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("reset pool");
        sqlx::raw_sql(
            "TRUNCATE TABLE block_body, block_record, current_peak, coin_record, \
             sub_epoch_segments_v3 RESTART IDENTITY CASCADE",
        )
        .execute(&pool)
        .await
        .expect("truncate contract tables");
        store
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
    async fn postgres_reorg_100_deep_flips_atomically_with_exact_coin_unwind() {
        run_long_reorg(open_clean().await, 100).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
    async fn postgres_reorg_1000_deep_flips_atomically_with_exact_coin_unwind() {
        run_long_reorg(open_clean().await, 1000).await;
    }

    // The Postgres crash contract at depth: same seam as the SQLite twin — the fault fires on
    // the peak flip inside the single reorg transaction, the dropped sqlx transaction rolls
    // back, and a FRESH PostgresStore over the same database (the restarted process) sees the
    // untouched pre-reorg state. This is the observable half of the synchronous-commit reorg
    // posture (stores/src/postgres/coin.rs `rollback_to_in`): the reorg is one transaction, so
    // the crash loss is all-or-nothing, never a torn rollback-without-flip. (The `SET LOCAL
    // synchronous_commit = on` scoping itself is pinned by the stores-side unit
    // `reorg_transaction_commits_synchronously`.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
    async fn postgres_crash_at_the_peak_flip_of_a_100_deep_reorg_is_atomic_and_recoverable() {
        const N: u32 = 100;
        let url =
            std::env::var("DGXCH_PG_URL").expect("set DGXCH_PG_URL to a dedicated test database");
        let (store, _fail_apply, fail_set_peak) = FaultStore::new(open_clean().await);
        let br = build_branches(N);
        let mut engine = Engine::new(store, NativePrimitives, MAINNET);

        engine.add_delta(br.base.clone()).await.unwrap();
        for d in &br.a {
            engine.add_delta(d.clone()).await.unwrap();
        }
        for d in &br.b[..N as usize] {
            engine.add_delta(d.clone()).await.unwrap();
        }
        let a_tip = br.a.last().unwrap().clone();
        let b_tip = br.b.last().unwrap().clone();
        let names = touched_names(N);
        let a_chain: Vec<&BlockDelta> = std::iter::once(&br.base).chain(br.a.iter()).collect();
        let b_chain: Vec<&BlockDelta> = std::iter::once(&br.base).chain(br.b.iter()).collect();

        fail_set_peak.store(true, Ordering::Relaxed);
        engine
            .add_delta(b_tip.clone())
            .await
            .expect_err("injected set_peak fault surfaces");
        // The restarted process: a fresh store over the same database sees pre-reorg state.
        let restarted = PostgresStore::open(&url).await.expect("reopen");
        assert_equals_replay(
            &restarted,
            (a_tip.header_hash, FORK + N),
            &a_chain,
            &names,
            "after the crashed deep flip (fresh connection)",
        )
        .await;

        fail_set_peak.store(false, Ordering::Relaxed);
        match engine.add_delta(b_tip.clone()).await.unwrap() {
            AddBlockOutcome::Reorg { fork_height, .. } => assert_eq!(fork_height, FORK),
            other => panic!("expected the retried deep reorg, got {other:?}"),
        }
        assert_equals_replay(
            &restarted,
            (b_tip.header_hash, FORK + N + 1),
            &b_chain,
            &names,
            "after the retried deep flip (fresh connection)",
        )
        .await;
    }
}
