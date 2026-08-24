//! Real-mainnet parity proof for the order-independent validation core.
//!
//! The synthetic proofs in `validate_v2.rs` prove order-independence and fail-closed cancellation
//! on hand-built coins. THIS proof runs the same [`RangeValidator`] over a REAL mainnet coin set
//! read from a synced store, and welds it to the independently-derived phase-1 root:
//!
//!   1. `dg_xch_roots::derive::derive` streams the store's `coin_record` in canonical
//!      `(confirmed_index, coin_name)` order and emits the phase-1 v1 root at the store's peak
//!      main-chain boundary — the exact `coin-root-derive` path.
//!   2. Every `coin_record` row is reduced to a [`BlockObservation`] (a creation at
//!      `confirmed_index`; a spend at `spent_index`), fed to [`RangeValidator`] in FORWARD and 64
//!      seeded-SHUFFLED block orders, and `reconcile`d against that derived root.
//!
//! The gate: derived root == reconciled root, BYTE-IDENTICAL, in forward AND every shuffled order,
//! and the MSet-XOR residual is invariant across orders — order-independent validation proven on
//! real mainnet data. The store is the reduced corpus (built by `node`'s `era_replay` / a synced
//! node); this harness never parses generators, matching the core's corpus-free contract.
//!
//! Env-gated on a populated store the derive tool reads:
//!   DGXCH_PARITY_STORE=sqlite:///path/to/chain.db \
//!     cargo test -p dg_xch_roots --test coin_root_mainnet_parity -- --ignored --nocapture

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_roots::derive::{StoreUrl, derive};
use dg_xch_roots::validate::{
    BlockObservation, CoinMeta, Creation, RangeValidator, SpendRef, TimeLockConditions,
};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row};
use std::collections::BTreeMap;

// Seeded Fisher-Yates over indices; identical LCG to `validate_v2.rs` (Numerical Recipes),
// no external RNG dep, deterministic, clippy-clean.
fn permutation(n: usize, seed: u64) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..n).collect();
    let mut state = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    for i in (1..n).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = (state >> 33) as usize % (i + 1);
        idx.swap(i, j);
    }
    idx
}

#[derive(Default)]
struct BlockAcc {
    creations: Vec<Creation>,
    spends: Vec<SpendRef>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a populated store (DGXCH_PARITY_STORE=sqlite:///path/to/chain.db)"]
async fn real_mainnet_aggregate_reconciles_to_phase1_root() {
    let url_s = std::env::var("DGXCH_PARITY_STORE")
        .expect("set DGXCH_PARITY_STORE=sqlite:///path/to/chain.db");
    let url = StoreUrl::parse(&url_s).expect("parse store url");
    let path = url_s
        .strip_prefix("sqlite://")
        .expect("this harness reads a sqlite:// store");

    let opts = SqliteConnectOptions::new().filename(path).read_only(true);
    let mut conn = sqlx::SqliteConnection::connect_with(&opts)
        .await
        .expect("open store read-only");

    // Boundary = the store peak main-chain height (guaranteed to carry a header the derive tool
    // resolves). The corpus peak sits above every pinned root, so we derive a FRESH phase-1 root
    // here rather than reconciling to a recorded one.
    let boundary: i64 =
        sqlx::query("SELECT MAX(height) AS h FROM block_record WHERE in_main_chain = 1")
            .fetch_one(&mut conn)
            .await
            .expect("peak query")
            .try_get("h")
            .expect("peak height");
    let boundary = u32::try_from(boundary).expect("boundary fits u32");

    // (1) The authoritative phase-1 root, via the coin-root-derive streaming path.
    let derived = derive(&url, &[boundary])
        .await
        .expect("derive phase-1 root")
        .into_iter()
        .next()
        .expect("exactly one root");
    eprintln!(
        "boundary height {boundary}: coin_count {} spent_count {} phase1_root {}",
        derived.coin_count, derived.spent_count, derived.root_v1
    );

    // (2) Reduce every coin_record row to add/remove observations.
    let rows = sqlx::query(
        "SELECT coin_name, confirmed_index, spent_index, timestamp FROM coin_record \
         WHERE confirmed_index <= ? ORDER BY confirmed_index ASC, coin_name ASC",
    )
    .bind(i64::from(boundary))
    .fetch_all(&mut conn)
    .await
    .expect("coin rows");
    assert!(
        !rows.is_empty(),
        "store has no coin_record rows <= {boundary}"
    );

    let mut blocks: BTreeMap<u32, BlockAcc> = BTreeMap::new();
    let mut survivors: Vec<Bytes32> = Vec::new();
    for row in &rows {
        let coin_id: Bytes32 = row.try_get("coin_name").expect("coin_name");
        let confirmed = u32::try_from(row.try_get::<i64, _>("confirmed_index").expect("confirmed"))
            .expect("confirmed u32");
        let spent =
            u32::try_from(row.try_get::<i64, _>("spent_index").expect("spent")).expect("spent u32");
        let timestamp = row.try_get::<i64, _>("timestamp").expect("timestamp") as u64;
        let meta = CoinMeta {
            created_height: confirmed,
            created_timestamp: timestamp,
        };

        blocks
            .entry(confirmed)
            .or_default()
            .creations
            .push(Creation { coin_id, meta });

        if spent != 0 && spent <= boundary {
            // The spend carries the coin OWN creation meta (self-authenticating): a wrong value
            // would break XOR cancellation. Bucket-C conditions are not re-derived here (the core
            // is corpus-free); the synthetic proofs cover time-lock semantics.
            blocks.entry(spent).or_default().spends.push(SpendRef {
                coin_id,
                spent_height: spent,
                meta,
                time_locks: TimeLockConditions::default(),
            });
        } else {
            survivors.push(coin_id);
        }
    }

    let observations: Vec<BlockObservation> = blocks
        .into_iter()
        .map(|(height, acc)| BlockObservation {
            height,
            now_height: height,
            now_timestamp: 0,
            self_valid: true,
            creations: acc.creations,
            spends: acc.spends,
        })
        .collect();
    eprintln!(
        "reduced {} coin rows to {} blocks, {} survivors",
        rows.len(),
        observations.len(),
        survivors.len()
    );

    // Forward order.
    let mut forward = RangeValidator::new();
    for o in &observations {
        forward.observe(o);
    }
    let base_xor = forward.xor_digest();
    let forward_root = forward
        .reconcile(&survivors, boundary, derived.header_hash, derived.root_v1)
        .expect("forward order reconciles to the derived root");
    assert_eq!(
        forward_root.root_v1, derived.root_v1,
        "forward reconcile root must byte-equal the derived phase-1 root"
    );

    // 64 seeded shuffles: identical XOR residual AND identical phase-1 root.
    for seed in 0..64u64 {
        let perm = permutation(observations.len(), seed);
        let mut v = RangeValidator::new();
        for &i in &perm {
            v.observe(&observations[i]);
        }
        assert_eq!(
            v.xor_digest(),
            base_xor,
            "MSet-XOR residual must be order-independent (seed {seed})"
        );
        let root = v
            .reconcile(&survivors, boundary, derived.header_hash, derived.root_v1)
            .expect("shuffled order reconciles to the derived root");
        assert_eq!(
            root.root_v1, derived.root_v1,
            "shuffled reconcile root must byte-equal the derived phase-1 root (seed {seed})"
        );
    }

    conn.close().await.expect("close store");
    eprintln!(
        "PARITY OK: derived == forward == 64x shuffled, byte-identical; root_v1 = {}",
        derived.root_v1
    );
}
