#![cfg(feature = "postgres")]

mod common;

use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_serialize::ChiaSerialize;
use dg_xch_stores::{BlockStatus, BlockStore, CoinStore, PostgresStore};

// The Postgres backend must serve the IDENTICAL trait contract the SQLite backend does — same
// fixtures, same sequences, same expectations. Env-gated on a DEDICATED test database (the test
// truncates its tables): DGXCH_PG_URL=postgres://user:pass@host/db \
//   cargo test -p dg_xch_stores --features postgres --test postgres_contract -- --ignored

async fn open_clean() -> PostgresStore {
    let url = std::env::var("DGXCH_PG_URL").expect("set DGXCH_PG_URL to a dedicated test database");
    let store = PostgresStore::open(&url).await.expect("open postgres");
    // A dedicated test database: reset the contract tables so runs are independent.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("reset pool");
    sqlx::raw_sql(
        "TRUNCATE TABLE block_body, block_record, current_peak, coin_record, sub_epoch_segments_v3 \
         RESTART IDENTITY CASCADE",
    )
    .execute(&pool)
    .await
    .expect("truncate contract tables");
    store
}

#[tokio::test]
#[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
async fn postgres_serves_the_block_and_coin_contract() {
    let store = open_clean().await;
    let records = common::load_records();
    store.add_block_records(&records).await.unwrap();

    // Round-trip a record and a by-height miss (candidates are not main-chain yet).
    let r0 = &records[0];
    let got = store
        .get_block_record(&r0.header_hash)
        .await
        .unwrap()
        .expect("record round-trips");
    assert_eq!(
        got.to_bytes(Default::default()).unwrap(),
        r0.to_bytes(Default::default()).unwrap()
    );
    assert!(
        store
            .get_block_record_by_height(r0.height)
            .await
            .unwrap()
            .is_none(),
        "candidate records are not on the main chain"
    );

    // Out-of-order body append in one batch, then unassociated shrinks.
    let before = store.get_unassociated(64).await.unwrap().len();
    let hi = common::load_full_block(5_000_004);
    let lo = common::load_full_block(5_000_000);
    let mut batch = store.begin().await.unwrap();
    store
        .append_many(&mut batch, &[hi.clone(), lo.clone()])
        .await
        .unwrap();
    store.commit(batch).await.unwrap();
    let after = store.get_unassociated(64).await.unwrap().len();
    assert_eq!(before - after, 2, "two bodies landed");
    let round = store
        .get_block(&lo.header_hash().unwrap())
        .await
        .unwrap()
        .expect("body round-trips");
    assert_eq!(round.header_hash().unwrap(), lo.header_hash().unwrap());

    // Peak flip + status + savepoint/rollback.
    let peak_hash = lo.header_hash().unwrap();
    let links = store.set_peak(&peak_hash).await.unwrap();
    assert!(links >= 1, "at least the peak links onto the main chain");
    let (hh, _) = store.get_peak().await.unwrap().expect("peak set");
    assert_eq!(hh, peak_hash);
    store
        .set_status(&peak_hash, BlockStatus::Validated)
        .await
        .unwrap();
    assert_eq!(
        store.get_status(&peak_hash).await.unwrap(),
        BlockStatus::Validated
    );
    let sp = store.savepoint().await.unwrap();
    let touched = store.rollback(sp).await.unwrap();
    assert_eq!(touched, 0, "rollback to the current peak touches nothing");

    // Coin apply + point-get + multi-get + reorg revert.
    let coin = Coin {
        parent_coin_info: Bytes32::from([1u8; 32]),
        puzzle_hash: Bytes32::from([2u8; 32]),
        amount: 1_000_000,
    };
    let cr = CoinRecord {
        coin,
        confirmed_block_index: 10,
        spent_block_index: 0,
        coinbase: false,
        timestamp: 1_700_000_000,
        spent: false,
    };
    store
        .apply_block(10, 1_700_000_000, &[cr], &[])
        .await
        .unwrap();
    let got = store
        .get_coin_record(&coin.name())
        .await
        .unwrap()
        .expect("coin round-trips");
    assert_eq!(got.coin.amount, 1_000_000);
    assert!(!got.spent);
    store
        .apply_block(11, 1_700_000_100, &[], &[coin.name()])
        .await
        .unwrap();
    let spent = store.get_coin_record(&coin.name()).await.unwrap().unwrap();
    assert!(spent.spent, "removal marks the coin spent at height 11");
    let reverted = store.rollback_to(10).await.unwrap();
    assert_eq!(reverted, 1, "the spend above the fork is reverted");
    let unspent = store.get_coin_record(&coin.name()).await.unwrap().unwrap();
    assert!(!unspent.spent, "the coin is unspent again after the reorg");
}

// The persisted weight-proof segment seam, identical expectations to the SQLite/mmap backends
// (chia's sub_epoch_segments_v3, block_store.py:85-88). Env-gated like the contract test above.
#[tokio::test]
#[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
async fn postgres_sub_epoch_segments_round_trip_and_replace() {
    let store = open_clean().await;
    let ses_hash = Bytes32::from([7u8; 32]);

    assert!(
        store
            .get_sub_epoch_segments(&ses_hash)
            .await
            .unwrap()
            .is_none(),
        "unknown ses hash misses"
    );
    store
        .persist_sub_epoch_segments(&ses_hash, b"segments-v1")
        .await
        .unwrap();
    assert_eq!(
        store.get_sub_epoch_segments(&ses_hash).await.unwrap(),
        Some(b"segments-v1".to_vec())
    );
    store
        .persist_sub_epoch_segments(&ses_hash, b"segments-v2")
        .await
        .unwrap();
    assert_eq!(
        store.get_sub_epoch_segments(&ses_hash).await.unwrap(),
        Some(b"segments-v2".to_vec()),
        "re-persist replaces the row (ON CONFLICT DO UPDATE)"
    );
}

// T0-4: the single-transaction reorg on Postgres — `rollback_to_in` (which also SET LOCALs
// synchronous_commit = on for the reorg transaction), the branch re-apply, and the peak-less
// abort path all share the batch's transaction: dropped, nothing happened; committed, one unit.
// Env-gated like the contract test above.
#[tokio::test]
#[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
async fn postgres_rollback_to_in_is_atomic_with_the_batch() {
    let store = open_clean().await;
    let below = Coin {
        parent_coin_info: Bytes32::from([0x71u8; 32]),
        puzzle_hash: Bytes32::from([0x8eu8; 32]),
        amount: 100,
    };
    let above = Coin {
        parent_coin_info: Bytes32::from([0x72u8; 32]),
        puzzle_hash: Bytes32::from([0x8du8; 32]),
        amount: 200,
    };
    let rec = |coin: Coin, h: u32| CoinRecord {
        coin,
        confirmed_block_index: h,
        spent_block_index: 0,
        coinbase: false,
        timestamp: 1_700_000_000,
        spent: false,
    };
    store
        .apply_block(10, 1_700_000_000, &[rec(below, 10)], &[])
        .await
        .unwrap();
    store
        .apply_block(11, 1_700_000_100, &[rec(above, 11)], &[below.name()])
        .await
        .unwrap();

    // Dropped batch (the crashed-reorg shape): the staged revert must not have happened.
    {
        let mut batch = store.begin().await.unwrap();
        let reverted = store.rollback_to_in(&mut batch, 10).await.unwrap();
        assert_eq!(reverted, 2, "one deletion + one un-spend staged");
    }
    assert!(
        store
            .get_coin_record(&above.name())
            .await
            .unwrap()
            .is_some(),
        "above-fork coin untouched by the dropped batch"
    );
    assert!(
        store
            .get_coin_record(&below.name())
            .await
            .unwrap()
            .unwrap()
            .spent,
        "spend untouched by the dropped batch"
    );

    // Committed batch: revert + branch re-apply + peak-flip-free commit as one unit.
    let branch = Coin {
        parent_coin_info: Bytes32::from([0x73u8; 32]),
        puzzle_hash: Bytes32::from([0x8cu8; 32]),
        amount: 300,
    };
    let mut batch = store.begin().await.unwrap();
    store.rollback_to_in(&mut batch, 10).await.unwrap();
    store
        .apply_block_in(
            &mut batch,
            11,
            1_700_000_200,
            &[rec(branch, 11)],
            &[below.name()],
        )
        .await
        .unwrap();
    store.commit(batch).await.unwrap();
    assert!(
        store
            .get_coin_record(&above.name())
            .await
            .unwrap()
            .is_none(),
        "old above-fork coin reverted"
    );
    assert!(
        store
            .get_coin_record(&branch.name())
            .await
            .unwrap()
            .is_some(),
        "branch coin applied"
    );
    let respent = store.get_coin_record(&below.name()).await.unwrap().unwrap();
    assert!(respent.spent && respent.spent_block_index == 11);
}

// The batch_coin_states_by_puzzle_hashes contract on the Postgres backend (chia
// coin_store.py:590): the same paging + filter + hint semantics the SQLite leg proves in
// coin_store.rs, against the GREATEST()-ordered SQL leg. Requires the coin-index/hint features
// the `postgres` full-node profile enables.
#[cfg(all(feature = "coin-index", feature = "hint"))]
#[tokio::test]
#[ignore = "requires a dedicated Postgres test database (DGXCH_PG_URL)"]
async fn postgres_batch_coin_states_pages_filters_and_joins_hints() {
    use dg_xch_core::blockchain::coin::Coin;
    use dg_xch_core::protocols::wallet::{CoinState, CoinStateFilters};

    fn state_height(cs: &CoinState) -> u32 {
        cs.created_height
            .unwrap_or(0)
            .max(cs.spent_height.unwrap_or(0))
    }
    let filters = |spent: bool, unspent: bool, hinted: bool, min_amount: u64| CoinStateFilters {
        include_spent: spent,
        include_unspent: unspent,
        include_hinted: hinted,
        min_amount,
    };

    let store = open_clean().await;
    let ph = Bytes32::from([0x55; 32]);
    let mut tag = 0u8;
    let mut seeded = Vec::new();
    for h in [10u32, 11, 12] {
        let recs: Vec<CoinRecord> = (0u64..3)
            .map(|i| {
                tag += 1;
                let mut parent = [tag; 32];
                parent[0] = 0xaa;
                CoinRecord {
                    coin: Coin {
                        parent_coin_info: Bytes32::from(parent),
                        puzzle_hash: ph,
                        amount: 1_000 + i,
                    },
                    confirmed_block_index: h,
                    spent_block_index: 0,
                    coinbase: false,
                    timestamp: 0,
                    spent: false,
                }
            })
            .collect();
        store.apply_block(h, 0, &recs, &[]).await.unwrap();
        seeded.extend(recs);
    }
    let victim = seeded[0].coin.name();
    store.apply_block(13, 0, &[], &[victim]).await.unwrap();
    let cat = CoinRecord {
        coin: Coin {
            parent_coin_info: Bytes32::from([0xCB; 32]),
            puzzle_hash: Bytes32::from([0xCA; 32]),
            amount: 5_000,
        },
        confirmed_block_index: 12,
        spent_block_index: 0,
        coinbase: false,
        timestamp: 0,
        spent: false,
    };
    store.apply_block(12, 0, &[cat], &[]).await.unwrap();
    store.apply_hints(&[(ph, cat.coin.name())]).await.unwrap();

    let (all, next) = store
        .batch_coin_states_by_puzzle_hashes(&[ph], 0, &filters(true, true, true, 0), 50_000)
        .await
        .unwrap();
    assert_eq!(next, None);
    assert_eq!(all.len(), 10, "9 plain + the hinted CAT");
    let heights: Vec<u32> = all.iter().map(state_height).collect();
    let mut sorted = heights.clone();
    sorted.sort_unstable();
    assert_eq!(heights, sorted, "ascending activity height");

    let (spent_only, _) = store
        .batch_coin_states_by_puzzle_hashes(&[ph], 0, &filters(true, false, true, 0), 50_000)
        .await
        .unwrap();
    assert_eq!(spent_only.len(), 1);
    let (rich, _) = store
        .batch_coin_states_by_puzzle_hashes(&[ph], 0, &filters(true, true, true, 1_001), 50_000)
        .await
        .unwrap();
    assert_eq!(rich.len(), 7);

    let mut min_height = 0u32;
    let mut names = std::collections::HashSet::new();
    let mut pages = 0;
    loop {
        let (states, next) = store
            .batch_coin_states_by_puzzle_hashes(&[ph], min_height, &filters(true, true, true, 0), 4)
            .await
            .unwrap();
        pages += 1;
        assert!(states.len() <= 4);
        for cs in &states {
            assert!(names.insert(cs.coin.name()), "no duplicates across pages");
        }
        match next {
            Some(h) => {
                assert!(states.iter().all(|cs| state_height(cs) < h));
                min_height = h;
            }
            None => break,
        }
        assert!(pages < 20, "the page loop must terminate");
    }
    assert!(pages > 1, "the scenario must actually page");
    assert_eq!(
        names.len(),
        10,
        "the loop recovers every state exactly once"
    );
}
