// Full-node RPC endpoints against a store seeded to a real mainnet block. Semantics under
// test: unknown block/coin = error (not null), get_block_records end-EXCLUSIVE with
// error-on-missing-height, include_spent_coins default FALSE, push_tx idempotent SUCCESS, the
// netspace formula, and the full blockchain_state shape. The HTTP envelope + TLS are exercised in tests/rpc_http.rs and
// the integration capstone.

mod common;

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::Mempool;
use dg_xch_stores::{BlockStore, CoinStore};
use full_node::{CoinQueryWindow, NodeRpc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

async fn rpc() -> (NodeRpc<dg_xch_stores::SqliteStore>, Arc<Mutex<Mempool>>) {
    let store = Arc::new(common::open_store().await);
    common::seed_peak(&store).await;
    let mempool = Arc::new(Mutex::new(Mempool::new(&MAINNET)));
    let synced = Arc::new(AtomicBool::new(true));
    let node = NodeRpc::new(
        store,
        mempool.clone(),
        MAINNET,
        synced,
        Arc::new(Mutex::new(Vec::new())),
    );
    (node, mempool)
}

fn spent_window() -> CoinQueryWindow {
    CoinQueryWindow {
        include_spent_coins: true,
        start_height: None,
        end_height: None,
    }
}

// get_blockchain_state serves the full shape: peak as a full BlockRecord, the sync
// sub-object, mempool gauges, block_max_cost, and a node_id (all-zero without live state).
#[tokio::test]
async fn blockchain_state_reports_the_chia_shape() {
    let (node, _mp) = rpc().await;
    let summary = node.get_blockchain_state().await.expect("state");
    let state = summary.state;
    let peak = state.peak.expect("peak present");
    assert_eq!(peak.height, common::PEAK_HEIGHT);
    assert_eq!(peak.header_hash, common::peak_record().header_hash);
    assert_eq!(state.sub_slot_iters, common::peak_record().sub_slot_iters);
    assert!(state.sync.synced);
    assert!(!state.sync.sync_mode, "synced node is not in sync_mode");
    assert!(state.genesis_challenge_initialized);
    assert_eq!(state.block_max_cost, MAINNET.max_block_cost_clvm);
    assert_eq!(state.mempool_size, 0);
    assert_eq!(state.mempool_cost, 0);
    assert_eq!(summary.mempool_fees, 0);
    assert!(
        state.mempool_max_total_cost > 0,
        "mempool capacity is reported"
    );
    // The single-record fixture has no 4608-deep history: space/average degrade to 0/None
    // rather than erroring.
    assert_eq!(state.space, 0);
    assert!(summary.average_block_time.is_none());
}

#[tokio::test]
async fn get_block_and_record_by_hash() {
    let (node, _mp) = rpc().await;
    let rec = common::peak_record();
    let got_rec = node
        .get_block_record(&rec.header_hash)
        .await
        .expect("record");
    assert_eq!(got_rec.header_hash, rec.header_hash);

    let got_block = node.get_block(&rec.header_hash).await.expect("block");
    assert_eq!(got_block.header_hash().expect("hh"), rec.header_hash);
}

// `get_block`: an unknown header hash is an ERROR (BLOCK_NOT_FOUND), never null.
#[tokio::test]
async fn get_block_unknown_hash_errors() {
    let (node, _mp) = rpc().await;
    let bogus = Bytes32::from([0x21u8; 32]);
    let err = node.get_block(&bogus).await.expect_err("unknown block");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
    let err = node
        .get_block_record(&bogus)
        .await
        .expect_err("unknown record");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

// `get_block_records` is END-EXCLUSIVE: [peak, peak) is empty, [peak, peak+1) is the peak.
// Heights above the peak end the walk instead of erroring.
#[tokio::test]
async fn get_block_records_is_end_exclusive() {
    let (node, _mp) = rpc().await;
    let empty = node
        .get_block_records(common::PEAK_HEIGHT, common::PEAK_HEIGHT)
        .await
        .expect("empty range");
    assert!(empty.is_empty(), "end == start yields nothing (exclusive)");

    let recs = node
        .get_block_records(common::PEAK_HEIGHT, common::PEAK_HEIGHT + 1)
        .await
        .expect("records");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].height, common::PEAK_HEIGHT);

    // end beyond the peak: the walk breaks at the peak, partial list.
    let recs = node
        .get_block_records(common::PEAK_HEIGHT, common::PEAK_HEIGHT + 5)
        .await
        .expect("records to peak");
    assert_eq!(recs.len(), 1);
}

// `get_block_records`: a height AT/BELOW the peak with no confirmed record is an ERROR
// (HEIGHT_NOT_IN_BLOCKCHAIN), never a silent skip.
#[tokio::test]
async fn get_block_records_errors_on_missing_sub_peak_height() {
    let (node, _mp) = rpc().await;
    let err = node
        .get_block_records(common::PEAK_HEIGHT - 1, common::PEAK_HEIGHT + 1)
        .await
        .expect_err("hole below the peak must error");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

// An over-cap range errors LOUDLY (bounded, never silently truncated).
#[tokio::test]
async fn get_block_records_over_cap_errors() {
    let (node, _mp) = rpc().await;
    let err = node
        .get_block_records(0, full_node::rpc::MAX_BLOCK_RECORDS_PER_REQUEST + 1)
        .await
        .expect_err("over-cap range");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

// `get_block_record_by_height`: clamps NOTHING — a height above the peak is an error; the
// peak itself resolves to the canonical record.
#[tokio::test]
async fn get_block_record_by_height_peak_and_beyond() {
    let (node, _mp) = rpc().await;
    let rec = node
        .get_block_record_by_height(common::PEAK_HEIGHT)
        .await
        .expect("peak by height");
    assert_eq!(rec.header_hash, common::peak_record().header_hash);
    let err = node
        .get_block_record_by_height(common::PEAK_HEIGHT + 1)
        .await
        .expect_err("beyond peak");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

// get_blocks: end-exclusive height range of full blocks with their header hashes.
#[tokio::test]
async fn get_blocks_serves_the_range() {
    let (node, _mp) = rpc().await;
    let blocks = node
        .get_blocks(common::PEAK_HEIGHT, common::PEAK_HEIGHT + 1)
        .await
        .expect("blocks");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].1, common::peak_record().header_hash);
    assert_eq!(
        blocks[0].0.header_hash().expect("hh"),
        common::peak_record().header_hash
    );
    // Missing heights are skipped (`get_full_blocks_at` returns what exists).
    let blocks = node
        .get_blocks(common::PEAK_HEIGHT - 2, common::PEAK_HEIGHT)
        .await
        .expect("blocks with holes");
    assert!(blocks.is_empty());
    // Over-cap errors loudly.
    let err = node
        .get_blocks(0, full_node::rpc::MAX_BLOCKS_PER_REQUEST + 1)
        .await
        .expect_err("over-cap");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

// `get_coin_records_by_names`: include_spent_coins defaults FALSE — a spent coin is only
// visible when explicitly requested; the height window filters on the confirmed height.
#[tokio::test]
async fn coin_records_by_names_spent_default_and_window() {
    let (node, _mp) = rpc().await;
    let adds = common::additions();
    let target = adds[0].coin.name();

    // Unspent coin: visible under the default window.
    let got = node
        .get_coin_records_by_names(&[target], CoinQueryWindow::default())
        .await
        .expect("coins");
    assert_eq!(got.len(), 1);
    assert!(!got[0].spent);

    // Spend it (a later block consumes it) — the default window must now HIDE it.
    node.store()
        .apply_block(common::PEAK_HEIGHT + 1, 0, &[], &[target])
        .await
        .expect("spend coin");
    let hidden = node
        .get_coin_records_by_names(&[target], CoinQueryWindow::default())
        .await
        .expect("default excludes spent");
    assert!(
        hidden.is_empty(),
        "spent coin hidden by default (include_spent_coins=False)"
    );
    let shown = node
        .get_coin_records_by_names(&[target], spent_window())
        .await
        .expect("spent included on request");
    assert_eq!(shown.len(), 1);
    assert!(shown[0].spent);

    // Height window: confirmed at PEAK_HEIGHT; start_height beyond it excludes, end_height is
    // EXCLUSIVE of the confirmed height.
    let mut w = spent_window();
    w.start_height = Some(common::PEAK_HEIGHT + 1);
    assert!(
        node.get_coin_records_by_names(&[target], w)
            .await
            .expect("windowed")
            .is_empty()
    );
    let mut w = spent_window();
    w.end_height = Some(common::PEAK_HEIGHT);
    assert!(
        node.get_coin_records_by_names(&[target], w)
            .await
            .expect("windowed")
            .is_empty(),
        "end_height is exclusive"
    );
    let mut w = spent_window();
    w.end_height = Some(common::PEAK_HEIGHT + 1);
    assert_eq!(
        node.get_coin_records_by_names(&[target], w)
            .await
            .expect("windowed")
            .len(),
        1
    );
}

// `get_coin_record_by_name`: unknown coin is an ERROR.
#[tokio::test]
async fn coin_record_by_name_errors_on_unknown() {
    let (node, _mp) = rpc().await;
    let known = common::additions()[0].coin.name();
    let got = node.get_coin_record_by_name(&known).await.expect("known");
    assert_eq!(got.coin.name(), known);
    let err = node
        .get_coin_record_by_name(&Bytes32::from([0x33u8; 32]))
        .await
        .expect_err("unknown coin");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

#[tokio::test]
async fn push_tx_runs_bundle_and_admits_to_mempool() {
    let (node, mempool) = rpc().await;
    mempool.lock().await.set_peak(common::PEAK_HEIGHT, 0);
    // spend a seeded, unspent coin locked by the easy puzzle — the endpoint runs the CLVM itself
    let coin = common::seed_easy_coin(node.store(), 1_000).await;
    let name = node
        .push_tx(common::easy_bundle(&coin, 1))
        .await
        .expect("admitted");
    assert_eq!(mempool.lock().await.len(), 1);
    assert!(mempool.lock().await.get(&name).is_some());
}

// push_tx: a bundle already resident answers SUCCESS (idempotent), no duplicate admission.
#[tokio::test]
async fn push_tx_is_idempotent_on_duplicate() {
    let (node, mempool) = rpc().await;
    mempool.lock().await.set_peak(common::PEAK_HEIGHT, 0);
    let coin = common::seed_easy_coin(node.store(), 1_000).await;
    let bundle = common::easy_bundle(&coin, 1);
    let first = node.push_tx(bundle.clone()).await.expect("admitted");
    let second = node.push_tx(bundle).await.expect("duplicate is SUCCESS");
    assert_eq!(first, second);
    assert_eq!(mempool.lock().await.len(), 1, "no duplicate admission");
}

#[tokio::test]
async fn push_tx_rejects_wrong_puzzle_reveal() {
    let (node, mempool) = rpc().await;
    mempool.lock().await.set_peak(common::PEAK_HEIGHT, 0);
    // a coin whose puzzle hash does NOT match the revealed puzzle: the server-side run must reject it
    // before it ever reaches the mempool
    let mut coin = common::seed_easy_coin(node.store(), 1_000).await;
    coin.puzzle_hash = common::additions()[0].coin.puzzle_hash;
    let err = node
        .push_tx(common::easy_bundle(&coin, 1))
        .await
        .expect_err("rejected");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
    assert_eq!(mempool.lock().await.len(), 0);
}

// The mempool read endpoints: tx ids, items, item-by-id (error on unknown), items-by-coin-name.
#[tokio::test]
async fn mempool_read_endpoints_serve_the_resident_item() {
    let (node, mempool) = rpc().await;
    mempool.lock().await.set_peak(common::PEAK_HEIGHT, 0);
    let coin = common::seed_easy_coin(node.store(), 1_000).await;
    let name = node
        .push_tx(common::easy_bundle(&coin, 7))
        .await
        .expect("admitted");

    let ids = node.get_all_mempool_tx_ids().await;
    assert_eq!(ids, vec![name]);

    let items = node.get_all_mempool_items().await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].spend_bundle_name, name);
    // fee = removals - additions: the easy bundle spends the whole 1000-mojo coin and creates
    // no outputs (RESERVE_FEE only asserts the surplus, it does not set it).
    assert_eq!(items[0].fee, 1_000);
    assert_eq!(
        items[0].removals,
        vec![coin],
        "removals are the spent coins"
    );

    let item = node.get_mempool_item_by_tx_id(&name).await.expect("by id");
    assert_eq!(item.spend_bundle_name, name);

    let err = node
        .get_mempool_item_by_tx_id(&Bytes32::from([0x44u8; 32]))
        .await
        .expect_err("unknown tx id");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));

    let by_coin = node.get_mempool_items_by_coin_name(&coin.name()).await;
    assert_eq!(by_coin.len(), 1);
    assert_eq!(by_coin[0].spend_bundle_name, name);
    assert!(
        node.get_mempool_items_by_coin_name(&Bytes32::from([0x55u8; 32]))
            .await
            .is_empty()
    );
}

// get_network_space: the netspace formula over two records — verified against an independent
// recomputation at pinned deltas, plus the same-hash and unknown-block error paths.
#[tokio::test]
async fn network_space_formula_and_errors() {
    use dg_xch_core::blockchain::block_record::BlockRecord;
    let (node, _mp) = rpc().await;
    let newer = common::peak_record();
    let mut older: BlockRecord = newer.clone();
    older.header_hash = Bytes32::from([0x77u8; 32]);
    older.height = newer.height - 4608;
    older.weight = newer.weight - 1024;
    older.total_iters = newer.total_iters - (1u128 << 40);
    node.store()
        .add_block_records(std::slice::from_ref(&older))
        .await
        .expect("older record");

    let space = node
        .get_network_space(&newer.header_hash, &older.header_hash)
        .await
        .expect("space");
    // Independent recomputation: height 5,000,000 is past
    // no plot-filter halving and below the hard fork, so prefix bits stay at 9.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    let expected = (0.762 * (1024f64 / (1u128 << 40) as f64) * 2f64.powi(67) * 512.0) as u128;
    assert_eq!(space, expected);

    let err = node
        .get_network_space(&newer.header_hash, &newer.header_hash)
        .await
        .expect_err("same hash");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
    let err = node
        .get_network_space(&Bytes32::from([0x88u8; 32]), &older.header_hash)
        .await
        .expect_err("unknown newer");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

// get_block_spends on a PRE-hard-fork block: the ROM generator surfaces no reveals — a clean
// error, never a panic. A non-transaction block
// (generator stripped) answers an empty list.
#[tokio::test]
async fn block_spends_pre_fork_errors_and_non_tx_block_is_empty() {
    let (node, _mp) = rpc().await;
    let hh = common::peak_record().header_hash;
    const { assert!(common::PEAK_HEIGHT < MAINNET.hard_fork_height) };
    let err = node
        .get_block_spends(&hh)
        .await
        .expect_err("pre-fork ROM generator cannot serve reveals");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
    let err = node
        .get_block_spends_with_conditions(&hh)
        .await
        .expect_err("pre-fork ROM generator cannot serve reveals");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));

    // Strip the generator (foliage untouched — the header hash is unchanged) in a fresh store:
    // a non-tx block answers [].
    let store = Arc::new(common::open_store().await);
    let rec = common::peak_record();
    let mut block = common::full_block();
    block.transactions_generator = None;
    store
        .add_block_records(std::slice::from_ref(&rec))
        .await
        .expect("records");
    let mut batch = store.begin().await.expect("begin");
    store
        .append_many(&mut batch, std::slice::from_ref(&block))
        .await
        .expect("append");
    store.commit(batch).await.expect("commit");
    store.set_peak(&rec.header_hash).await.expect("peak");
    let node = NodeRpc::new(
        store,
        Arc::new(Mutex::new(Mempool::new(&MAINNET))),
        MAINNET,
        Arc::new(AtomicBool::new(true)),
        Arc::new(Mutex::new(Vec::new())),
    );
    assert!(
        node.get_block_spends(&rec.header_hash)
            .await
            .expect("non-tx block")
            .is_empty()
    );
}

// The constant/identity endpoints work without live state: aggsig additional data is the
// mainnet constant, network info infers mainnet/xch, connections and unfinished headers answer
// empty, and a signage-point probe reports not-in-cache.
#[tokio::test]
async fn constant_endpoints_without_live_state() {
    let (node, _mp) = rpc().await;
    assert_eq!(
        node.get_aggsig_additional_data(),
        MAINNET.agg_sig_me_additional_data
    );
    let (name, prefix, genesis) = node.get_network_info();
    assert_eq!(name, "mainnet");
    assert_eq!(prefix, "xch");
    assert_eq!(genesis, MAINNET.genesis_challenge);
    assert!(node.get_connections(None).await.is_empty());
    assert!(
        node.get_unfinished_block_headers()
            .await
            .expect("headers")
            .is_empty()
    );
    let err = node
        .get_recent_signage_point_or_eos(Some(&Bytes32::from([0x99u8; 32])), None)
        .await
        .expect_err("sp not in cache");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

#[cfg(feature = "coin-index")]
#[tokio::test]
async fn get_coin_records_by_puzzle_hash_service_tier() {
    let (node, _mp) = rpc().await;
    let adds = common::additions();
    let ph = adds[0].coin.puzzle_hash;
    let got = node
        .get_coin_records_by_puzzle_hash(&ph, CoinQueryWindow::default())
        .await
        .expect("coins by ph");
    assert!(got.iter().any(|c| c.coin.puzzle_hash == ph));
    // The plural endpoint answers the union.
    let got_plural = node
        .get_coin_records_by_puzzle_hashes(&[ph], CoinQueryWindow::default())
        .await
        .expect("coins by phs");
    assert_eq!(got.len(), got_plural.len());
}

// coin-index tier: include_spent_coins=true resolves spent coins through the coin-state index.
#[cfg(feature = "coin-index")]
#[tokio::test]
async fn coin_records_by_puzzle_hash_spent_inclusion() {
    let (node, _mp) = rpc().await;
    let adds = common::additions();
    let target = adds[0].coin;
    node.store()
        .apply_block(common::PEAK_HEIGHT + 1, 0, &[], &[target.name()])
        .await
        .expect("spend");
    let unspent_only = node
        .get_coin_records_by_puzzle_hash(&target.puzzle_hash, CoinQueryWindow::default())
        .await
        .expect("unspent");
    assert!(unspent_only.iter().all(|c| c.coin.name() != target.name()));
    let with_spent = node
        .get_coin_records_by_puzzle_hash(&target.puzzle_hash, spent_window())
        .await
        .expect("with spent");
    assert!(
        with_spent
            .iter()
            .any(|c| c.coin.name() == target.name() && c.spent)
    );
}

// coin-index tier: coins by parent id, spent-default false.
#[cfg(feature = "coin-index")]
#[tokio::test]
async fn coin_records_by_parent_ids_service_tier() {
    let (node, _mp) = rpc().await;
    let adds = common::additions();
    let target = adds[0].coin;
    let got = node
        .get_coin_records_by_parent_ids(&[target.parent_coin_info], CoinQueryWindow::default())
        .await
        .expect("by parent");
    assert!(got.iter().any(|c| c.coin.name() == target.name()));
    assert!(
        node.get_coin_records_by_parent_ids(
            &[Bytes32::from([0xAAu8; 32])],
            CoinQueryWindow::default()
        )
        .await
        .expect("unknown parent")
        .is_empty()
    );
}

// get_additions_and_removals: seed_peak applies block 5,000,000's real additions (unspent) at the
// peak height, so the endpoint returns them as additions with no removals — and rejects a header
// hash that is not the confirmed block at its height (the fork check).
#[cfg(feature = "coin-index")]
#[tokio::test]
async fn additions_and_removals_at_the_peak_block() {
    let (node, _mp) = rpc().await;
    let ar = node
        .get_additions_and_removals(&common::peak_record().header_hash)
        .await
        .expect("additions and removals");
    assert!(!ar.additions.is_empty(), "peak block created coins");
    assert!(
        ar.additions
            .iter()
            .all(|c| c.confirmed_block_index == common::PEAK_HEIGHT),
        "every addition is confirmed at the peak height",
    );
    assert!(ar.removals.is_empty(), "seeded additions are unspent");

    // A header hash that is not the confirmed block at any height must be refused, not answered.
    let bogus = Bytes32::from([0x11u8; 32]);
    assert!(
        node.get_additions_and_removals(&bogus).await.is_err(),
        "unknown / forked header hash must error",
    );
}

// get_coin_records_by_hint: a coin indexed under a 32-byte hint resolves back through the
// coin_hint index; an unknown hint resolves to nothing; the default excludes spent coins.
#[cfg(feature = "hint")]
#[tokio::test]
async fn coin_records_by_hint_resolves_indexed_coins() {
    use dg_xch_core::blockchain::coin::Coin;
    use dg_xch_core::blockchain::coin_record::CoinRecord;
    use dg_xch_stores::CoinStore;

    let (node, _mp) = rpc().await;
    let hint = Bytes32::from([0x7Au8; 32]);
    let coin = Coin {
        parent_coin_info: Bytes32::from([0x01u8; 32]),
        puzzle_hash: Bytes32::from([0x02u8; 32]),
        amount: 500,
    };
    let rec = CoinRecord {
        coin,
        confirmed_block_index: common::PEAK_HEIGHT,
        spent_block_index: 0,
        coinbase: false,
        timestamp: 0,
        spent: false,
    };
    node.store()
        .apply_block(common::PEAK_HEIGHT, 0, std::slice::from_ref(&rec), &[])
        .await
        .expect("apply hinted coin");
    node.store()
        .apply_hints(&[(hint, coin.name())])
        .await
        .expect("index hint");

    let got = node
        .get_coin_records_by_hint(&hint, CoinQueryWindow::default())
        .await
        .expect("by hint");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].coin.name(), coin.name());

    let none = node
        .get_coin_records_by_hint(&Bytes32::from([0xFFu8; 32]), CoinQueryWindow::default())
        .await
        .expect("unknown hint");
    assert!(none.is_empty(), "an unindexed hint resolves to no coins");
}

// get_puzzle_and_solution's height gate: an unspent coin (or a mismatched height) is refused before
// any generator run — INVALID_HEIGHT_FOR_COIN. Real reveal+solution extraction over a
// post-hard-fork generator is proven in core's `coin_spend_extraction` fixture test.
#[tokio::test]
async fn puzzle_and_solution_rejects_unspent_coin() {
    let (node, _mp) = rpc().await;
    let target = common::additions()[0].coin.name();
    let err = node
        .get_puzzle_and_solution(&target, common::PEAK_HEIGHT)
        .await
        .expect_err("an unspent coin has no puzzle/solution at this height");
    assert!(matches!(err, full_node::RpcError::BadRequest(_)));
}

#[tokio::test]
async fn synced_flag_reflects_pipeline_state() {
    let store = Arc::new(common::open_store().await);
    common::seed_peak(&store).await;
    let synced = Arc::new(AtomicBool::new(false));
    let node = NodeRpc::new(
        store,
        Arc::new(Mutex::new(Mempool::new(&MAINNET))),
        MAINNET,
        synced.clone(),
        Arc::new(Mutex::new(Vec::new())),
    );
    let state = node.get_blockchain_state().await.unwrap().state;
    assert!(!state.sync.synced);
    assert!(state.sync.sync_mode, "not-synced reports sync_mode");
    synced.store(true, Ordering::Relaxed);
    assert!(node.get_blockchain_state().await.unwrap().state.sync.synced);
}

// ---- get_fee_estimate --------------------------------------------------------------------------

// `get_fee_estimate` response shape: estimates[]/target_times[] (sorted), a float
// current_fee_rate, the mempool gauges, synced flag, and peak/last-block telemetry. An empty
// mempool with no confirmation history yields the FLOOR (0) for every estimate — never a constant.
#[tokio::test]
async fn get_fee_estimate_empty_returns_floor_and_chia_shape() {
    let (node, _mp) = rpc().await;
    let resp = node
        .get_fee_estimate(None, Some(5_000_000), vec![300, 60, 120])
        .await
        .expect("estimate");
    // target_times echoed back SORTED ascending.
    assert_eq!(resp.target_times, vec![60, 120, 300]);
    assert_eq!(resp.estimates.len(), 3);
    assert!(
        resp.estimates.iter().all(|&e| e == 0),
        "empty mempool → floor 0, got {:?}",
        resp.estimates
    );
    assert_eq!(resp.current_fee_rate, 0.0);
    assert!(resp.full_node_synced);
    assert_eq!(resp.mempool_size, 0);
    assert_eq!(resp.mempool_fees, 0);
    assert_eq!(resp.num_spends, 0);
    assert!(resp.mempool_max_size > 0, "mempool capacity is reported");
    assert_eq!(resp.peak_height, common::PEAK_HEIGHT);
}

// A mempool whose estimator has seen sustained confirmed pressure quotes POSITIVE estimates, and
// they are monotonically non-increasing in target time (sooner ⇒ pricier) —
// make_monotonically_decreasing over the sorted target_times.
#[tokio::test]
async fn get_fee_estimate_loaded_is_positive_and_monotonic() {
    let (node, mp) = rpc().await;
    {
        let mut m = mp.lock().await;
        // wait = 1: confirmed the next block, so short targets also have data.
        for height in 100u32..300 {
            m.fee_estimator_mut().ingest_block(
                height,
                &[(5_000_000, 100_000_000, height - 1)],
                5_000_000,
            );
        }
    }
    let resp = node
        .get_fee_estimate(None, Some(5_000_000), vec![300, 60, 120])
        .await
        .expect("estimate");
    assert_eq!(resp.target_times, vec![60, 120, 300]);
    assert!(
        resp.current_fee_rate > 0.0,
        "loaded mempool → positive current fee rate, got {}",
        resp.current_fee_rate
    );
    assert!(
        resp.estimates[0] > 0,
        "loaded mempool → positive estimate, got {:?}",
        resp.estimates
    );
    assert!(
        resp.estimates.windows(2).all(|w| w[0] >= w[1]),
        "estimates monotonically decrease with target time: {:?}",
        resp.estimates
    );
}

// `_validate_fee_estimate_cost`: exactly one of {spend_bundle, cost} — neither or both errors.
#[tokio::test]
async fn get_fee_estimate_requires_exactly_one_cost_source() {
    let (node, _mp) = rpc().await;
    let neither = node.get_fee_estimate(None, None, vec![60]).await;
    assert!(matches!(neither, Err(full_node::RpcError::BadRequest(_))));
}
