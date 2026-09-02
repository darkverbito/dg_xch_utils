#![cfg(feature = "mmap")]

mod common;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_serialize::ChiaSerialize;
use dg_xch_stores::{BlockStatus, BlockStore, CoinStore, MmapStore};

// The mmap backend must serve the IDENTICAL trait contract the SQLite and Postgres backends do —
// same fixtures, same sequences, same expectations — plus a reopen-from-disk pass (the whole
// point of the files). Not env-gated: everything lives in a tempdir.

#[tokio::test]
async fn mmap_serves_the_block_and_coin_contract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MmapStore::open(dir.path()).await.expect("open mmap store");
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

    // Coin apply + point-get + reorg revert.
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
        .apply_block(10, 1_700_000_000, std::slice::from_ref(&cr), &[])
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

    // Reopen from disk: the files ARE the store — records, bodies, peak, and coins all survive.
    let peak_before = store.get_peak().await.unwrap();
    drop(store);
    let reopened = MmapStore::open(dir.path())
        .await
        .expect("reopen mmap store");
    assert_eq!(reopened.get_peak().await.unwrap(), peak_before);
    let got = reopened
        .get_block_record(&r0.header_hash)
        .await
        .unwrap()
        .expect("record survives reopen");
    assert_eq!(got.height, r0.height);
    assert!(
        reopened.get_block(&peak_hash).await.unwrap().is_some(),
        "body survives reopen"
    );
    let coin_back = reopened
        .get_coin_record(&coin.name())
        .await
        .unwrap()
        .expect("coin survives reopen");
    assert!(!coin_back.spent, "slot spend-state survives reopen");
    assert_eq!(
        reopened.get_status(&peak_hash).await.unwrap(),
        BlockStatus::Validated,
        "status survives reopen"
    );
}

#[tokio::test]
async fn mmap_sub_epoch_segments_round_trip_replace_and_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MmapStore::open(dir.path()).await.expect("open mmap store");
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
        "re-persist replaces the entry"
    );

    drop(store);
    let reopened = MmapStore::open(dir.path()).await.expect("reopen");
    assert_eq!(
        reopened.get_sub_epoch_segments(&ses_hash).await.unwrap(),
        Some(b"segments-v2".to_vec()),
        "segments survive a restart"
    );
}

// --- Batched sync-before-link ---------------------------------
// `apply_block_in` stages coin-table links in the batch; they publish at the batch's durability
// point (set_peak_in / commit) behind ONE log fsync + ONE ordering msync — never before. These
// tests pin the three semantics that changed: staged invisibility, publication at the boundary,
// and whole-batch loss on a dropped handle.

fn synth_coin(tag: u8, amount: u64) -> CoinRecord {
    CoinRecord {
        coin: Coin {
            parent_coin_info: Bytes32::from([tag; 32]),
            puzzle_hash: Bytes32::from([tag ^ 0xFF; 32]),
            amount,
        },
        confirmed_block_index: 0,
        spent_block_index: 0,
        coinbase: false,
        timestamp: 1_700_000_000,
        spent: false,
    }
}

#[tokio::test]
async fn mmap_batched_coin_links_publish_at_commit_not_before() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MmapStore::open(dir.path()).await.expect("open mmap store");
    let a = synth_coin(0x21, 111);
    let b = synth_coin(0x22, 222);
    let mut batch = store.begin().await.unwrap();
    store
        .apply_block_in(
            &mut batch,
            42,
            1_700_000_000,
            &[a, b],
            // b is ephemeral: created and spent inside the same staged block.
            &[b.coin.name()],
        )
        .await
        .unwrap();
    assert!(
        store
            .get_coin_record(&a.coin.name())
            .await
            .unwrap()
            .is_none(),
        "staged coin links are invisible before the batch's durability point"
    );
    store.commit(batch).await.unwrap();
    let got_a = store
        .get_coin_record(&a.coin.name())
        .await
        .unwrap()
        .expect("published at commit");
    assert_eq!(got_a.confirmed_block_index, 42);
    assert!(!got_a.spent);
    let got_b = store
        .get_coin_record(&b.coin.name())
        .await
        .unwrap()
        .expect("ephemeral coin published too");
    assert!(got_b.spent, "same-batch spend lands in the staged payload");
    assert_eq!(got_b.spent_block_index, 42);
}

#[tokio::test]
async fn mmap_set_peak_in_drains_staged_coins_before_the_peak_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MmapStore::open(dir.path()).await.expect("open mmap store");
    // A confirmed archive row to hang the peak on.
    let records = common::load_records();
    store.add_block_records(&records).await.unwrap();
    let peak_hash = records[0].header_hash;
    let c = synth_coin(0x33, 333);
    let mut batch = store.begin().await.unwrap();
    store
        .apply_block_in(
            &mut batch,
            records[0].height,
            1_700_000_000,
            std::slice::from_ref(&c),
            &[],
        )
        .await
        .unwrap();
    store.set_peak_in(&mut batch, &peak_hash).await.unwrap();
    // The durability ordering under test: at the moment the peak pointer is written, the staged
    // coin links are already published (a durable peak can never cover unlinked coins).
    let got = store
        .get_coin_record(&c.coin.name())
        .await
        .unwrap()
        .expect("set_peak_in published the staged links before the peak walk");
    assert_eq!(got.confirmed_block_index, records[0].height);
    store.commit(batch).await.unwrap();
    let (hh, _) = store.get_peak().await.unwrap().expect("peak set");
    assert_eq!(hh, peak_hash);
}

#[tokio::test]
async fn mmap_dropped_batch_loses_staged_coins_wholly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = MmapStore::open(dir.path()).await.expect("open mmap store");
    let c = synth_coin(0x44, 444);
    {
        let mut batch = store.begin().await.unwrap();
        store
            .apply_block_in(&mut batch, 7, 1_700_000_000, std::slice::from_ref(&c), &[])
            .await
            .unwrap();
        // Dropped without commit: the staged links vanish with the handle (the appended log
        // frames become unreferenced — invisible to every read path).
    }
    assert!(
        store
            .get_coin_record(&c.coin.name())
            .await
            .unwrap()
            .is_none(),
        "a dropped batch loses its staged coins wholly"
    );
    // The same coin re-applies cleanly afterwards (the resume path).
    store
        .apply_block(7, 1_700_000_000, std::slice::from_ref(&c), &[])
        .await
        .unwrap();
    let got = store
        .get_coin_record(&c.coin.name())
        .await
        .unwrap()
        .expect("re-apply after a dropped batch");
    assert_eq!(got.confirmed_block_index, 7);
}

#[tokio::test]
async fn mmap_batched_links_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let store = MmapStore::open(dir.path()).await.expect("open mmap store");
        let mut coins = Vec::new();
        // Enough keys to exercise intra-batch bucket-chain threading in insert_batch.
        for i in 0..64u8 {
            coins.push(synth_coin(i.wrapping_add(0x50), u64::from(i) + 1));
        }
        let mut batch = store.begin().await.unwrap();
        store
            .apply_block_in(&mut batch, 9, 1_700_000_000, &coins, &[])
            .await
            .unwrap();
        store.commit(batch).await.unwrap();
    }
    let reopened = MmapStore::open(dir.path()).await.expect("reopen");
    for i in 0..64u8 {
        let c = synth_coin(i.wrapping_add(0x50), u64::from(i) + 1);
        let got = reopened
            .get_coin_record(&c.coin.name())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("coin {i} survives the restart"));
        assert_eq!(got.coin.amount, u64::from(i) + 1);
    }
}

// --- T0-4: reorg crash-convergence (the reorg journal) -----------------------------------------
// The batched reorg publish (fork sweep + branch re-applies + peak flip) has no transaction to
// abort; its crash windows are bracketed by `reorg.journal` — written durably before the first
// published mutation, removed after the peak meta write. A journal found at open converges the
// store back to the fork: a consistent pre-reorg state (peak's coin set == its chain's deltas)
// from which the node re-syncs the heavier branch. These tests fabricate the exact on-disk states
// a kill leaves at each ordering point and prove reopen converges. Journal format (documented in
// mmap/mod.rs): [version=1][fork_height:4 LE][has_hash:1][fork_hash:32].

fn write_reorg_journal_file(dir: &std::path::Path, fork_height: u32, fork_hash: &Bytes32) {
    let mut buf = [0u8; 38];
    buf[0] = 1;
    buf[1..5].copy_from_slice(&fork_height.to_le_bytes());
    buf[5] = 1;
    let hb: &[u8] = fork_hash;
    buf[6..38].copy_from_slice(hb);
    std::fs::write(dir.join("reorg.journal"), buf).expect("write journal");
}

fn synth_record(template: &BlockRecord, tag: u8, height: u32, prev: Bytes32) -> BlockRecord {
    let mut r = template.clone();
    r.header_hash = Bytes32::from([tag; 32]);
    r.prev_hash = prev;
    r.height = height;
    r.weight = u128::from(height) * 100 + u128::from(tag);
    r.total_iters = r.weight;
    r.sub_epoch_summary_included = None;
    r
}

// Fork block F@10 (creates cf), old tip A@11 (creates ca, spends cf), new tip B@11 (creates cb,
// spends cf) — the two-branch scenario every crash window below reorgs across.
struct ReorgRig {
    f: BlockRecord,
    a: BlockRecord,
    b: BlockRecord,
    cf: CoinRecord,
    ca: CoinRecord,
    cb: CoinRecord,
}

fn reorg_rig() -> ReorgRig {
    let template = &common::load_records()[0];
    let f = synth_record(template, 0xf0, 10, Bytes32::from([0u8; 32]));
    let a = synth_record(template, 0xa0, 11, f.header_hash);
    let b = synth_record(template, 0xb0, 11, f.header_hash);
    ReorgRig {
        f,
        a,
        b,
        cf: synth_coin(0x61, 100),
        ca: synth_coin(0x62, 200),
        cb: synth_coin(0x63, 300),
    }
}

// Drive the store to the pre-reorg state: peak = A@11, coins = {cf spent@11, ca}.
async fn seed_old_branch(store: &MmapStore, rig: &ReorgRig) {
    store
        .add_block_records(&[rig.f.clone(), rig.a.clone(), rig.b.clone()])
        .await
        .unwrap();
    store
        .apply_block(10, 1_700_000_000, std::slice::from_ref(&rig.cf), &[])
        .await
        .unwrap();
    store.set_peak(&rig.f.header_hash).await.unwrap();
    store
        .apply_block(
            11,
            1_700_000_100,
            std::slice::from_ref(&rig.ca),
            &[rig.cf.coin.name()],
        )
        .await
        .unwrap();
    store.set_peak(&rig.a.header_hash).await.unwrap();
    assert_eq!(
        store.get_peak().await.unwrap(),
        Some((rig.a.header_hash, 11))
    );
}

// The consistent pre-reorg state convergence must restore: peak = F@10, cf unspent, neither
// branch's height-11 effects visible, no journal left.
async fn assert_converged_to_fork(store: &MmapStore, rig: &ReorgRig, dir: &std::path::Path) {
    assert_eq!(
        store.get_peak().await.unwrap(),
        Some((rig.f.header_hash, 10)),
        "converged peak is the fork"
    );
    let cf = store
        .get_coin_record(&rig.cf.coin.name())
        .await
        .unwrap()
        .expect("fork coin survives");
    assert!(!cf.spent, "the above-fork spend is reverted");
    assert!(
        store
            .get_coin_record(&rig.ca.coin.name())
            .await
            .unwrap()
            .is_none(),
        "old-branch coin gone"
    );
    assert!(
        store
            .get_coin_record(&rig.cb.coin.name())
            .await
            .unwrap()
            .is_none(),
        "new-branch coin gone"
    );
    assert!(
        store
            .get_block_record_by_height(11)
            .await
            .unwrap()
            .is_none(),
        "no main-chain block above the fork"
    );
    assert_eq!(
        store
            .get_block_record_by_height(10)
            .await
            .unwrap()
            .expect("fork on main chain")
            .header_hash,
        rig.f.header_hash
    );
    assert!(
        !dir.join("reorg.journal").exists(),
        "journal removed after convergence"
    );
}

// Run the engine's batched reorg sequence to B: one batch, sweep + re-apply + peak flip + commit.
async fn run_batched_reorg(store: &MmapStore, rig: &ReorgRig) {
    let mut batch = store.begin().await.unwrap();
    store.rollback_to_in(&mut batch, 10).await.unwrap();
    store
        .apply_block_in(
            &mut batch,
            11,
            1_700_000_200,
            std::slice::from_ref(&rig.cb),
            &[rig.cf.coin.name()],
        )
        .await
        .unwrap();
    store
        .set_peak_in(&mut batch, &rig.b.header_hash)
        .await
        .unwrap();
    store.commit(batch).await.unwrap();
}

async fn assert_on_new_branch(store: &MmapStore, rig: &ReorgRig) {
    assert_eq!(
        store.get_peak().await.unwrap(),
        Some((rig.b.header_hash, 11)),
        "peak is the new tip"
    );
    let cf = store
        .get_coin_record(&rig.cf.coin.name())
        .await
        .unwrap()
        .expect("fork coin survives");
    assert!(cf.spent, "spent by the new branch");
    assert_eq!(cf.spent_block_index, 11);
    assert!(
        store
            .get_coin_record(&rig.ca.coin.name())
            .await
            .unwrap()
            .is_none(),
        "old-branch coin reverted"
    );
    let cb = store
        .get_coin_record(&rig.cb.coin.name())
        .await
        .unwrap()
        .expect("new-branch coin present");
    assert_eq!(cb.confirmed_block_index, 11);
    assert_eq!(
        store
            .get_block_record_by_height(11)
            .await
            .unwrap()
            .expect("new tip on main chain")
            .header_hash,
        rig.b.header_hash
    );
}

// Kill INSIDE the publish: journal on disk, sweep published (old-branch coins reverted), peak
// meta unmoved — the exact torn state T0-4 is about. Reopen must converge to the fork, and the
// re-driven batched reorg must then land cleanly.
#[tokio::test]
async fn mmap_kill_inside_reorg_publish_converges_at_open_and_retry_lands() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = reorg_rig();
    {
        let store = MmapStore::open(dir.path()).await.expect("open");
        seed_old_branch(&store, &rig).await;
        // Byte-for-byte the crash state: the journal landed, the sweep's in-place reverts
        // published (the standalone rollback_to is the same sweep), the peak meta never moved.
        write_reorg_journal_file(dir.path(), 10, &rig.f.header_hash);
        store.rollback_to(10).await.unwrap();
        assert_eq!(
            store.get_peak().await.unwrap(),
            Some((rig.a.header_hash, 11)),
            "torn: coins reverted while the peak still points at the old branch"
        );
    }
    let store = MmapStore::open(dir.path()).await.expect("reopen");
    assert_converged_to_fork(&store, &rig, dir.path()).await;

    // The retried reorg (the node re-syncs the heavier branch) lands and survives reopen.
    run_batched_reorg(&store, &rig).await;
    assert!(
        !dir.path().join("reorg.journal").exists(),
        "journal comes off once the peak meta is durable"
    );
    assert_on_new_branch(&store, &rig).await;
    drop(store);
    let reopened = MmapStore::open(dir.path()).await.expect("reopen 2");
    assert_on_new_branch(&reopened, &rig).await;
}

// Kill AFTER the peak meta write but BEFORE the journal removal: the reorg completed, the bracket
// did not come off. Convergence takes the deliberate conservative arm — roll back to the fork
// (consistent; the branch is re-synced from already-archived work) rather than trust a peak the
// bracket cannot vouch for.
#[tokio::test]
async fn mmap_journal_surviving_a_completed_reorg_converges_conservatively() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rig = reorg_rig();
    {
        let store = MmapStore::open(dir.path()).await.expect("open");
        seed_old_branch(&store, &rig).await;
        run_batched_reorg(&store, &rig).await;
        assert_on_new_branch(&store, &rig).await;
        // The kill window: re-create the journal the completed reorg would have just removed.
        write_reorg_journal_file(dir.path(), 10, &rig.f.header_hash);
    }
    let store = MmapStore::open(dir.path()).await.expect("reopen");
    assert_converged_to_fork(&store, &rig, dir.path()).await;
}

#[cfg(all(feature = "coin-index", feature = "hint"))]
#[tokio::test]
async fn mmap_batch_coin_states_pages_filters_and_joins_hints() {
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

    let dir = tempfile::tempdir().expect("tempdir");
    let store = MmapStore::open(dir.path()).await.expect("open mmap store");
    let ph = Bytes32::from([0x55; 32]);
    // 3 heights x 3 coins on ph; one spent later; plus a hinted CAT-shaped coin.
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

    // Single page: everything (9 plain + 1 hinted), height-ordered, finished.
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
    assert!(all.iter().any(|cs| cs.coin.name() == cat.coin.name()));

    // Hinted excluded without the flag; spent/unspent split; min_amount floor.
    let (plain, _) = store
        .batch_coin_states_by_puzzle_hashes(&[ph], 0, &filters(true, true, false, 0), 50_000)
        .await
        .unwrap();
    assert_eq!(plain.len(), 9);
    let (spent_only, _) = store
        .batch_coin_states_by_puzzle_hashes(&[ph], 0, &filters(true, false, true, 0), 50_000)
        .await
        .unwrap();
    assert_eq!(spent_only.len(), 1);
    assert_eq!(spent_only[0].coin.name(), victim);
    let (rich, _) = store
        .batch_coin_states_by_puzzle_hashes(&[ph], 0, &filters(true, true, true, 1_001), 50_000)
        .await
        .unwrap();
    assert_eq!(
        rich.len(),
        7,
        "the three 1_000-amount coins fall below the floor"
    );

    // Paging: max_items=4 across 3-coin heights forces whole-height page cuts; the loop
    // recovers everything exactly once.
    let mut min_height = 0u32;
    let mut collected = 0usize;
    let mut names = std::collections::HashSet::new();
    let mut pages = 0;
    loop {
        let (states, next) = store
            .batch_coin_states_by_puzzle_hashes(&[ph], min_height, &filters(true, true, true, 0), 4)
            .await
            .unwrap();
        pages += 1;
        assert!(states.len() <= 4);
        collected += states.len();
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
    assert_eq!(collected, 10, "the loop recovers every state exactly once");
}
