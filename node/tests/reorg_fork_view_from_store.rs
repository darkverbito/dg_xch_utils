//! Store-backed fork view (no reorg-depth horizon): `Engine::fork_view` and
//! `Engine::candidate_branch` rebuild a fork's coin context / reorg branch from the DURABLE
//! STORE (persisted body + record) when the in-memory `staged_deltas`/`pending` caches miss —
//! not a bounded/volatile cache. Fork choice is weight-only and the reorg rolls the coin store
//! back to the fork height at ANY depth, so a heavier valid chain must always win, regardless of
//! fork depth or a process restart.
//!
//! The failing case: a 1-block EQUAL-WEIGHT tie-break strands the node after a restart because
//! the next block's prev (the orphan branch) is stored-but-non-confirmed and absent from the
//! empty `pending` cache. These tests reproduce that minimized, and the over-reorg guard proves
//! the store-backed walk does NOT reorg to a branch that is not heavier than the peak.

mod common;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::condition_with_args::ConditionWithArgs;
use dg_xch_core::blockchain::foliage_transaction_block::FoliageTransactionBlock;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use dg_xch_core::blockchain::transactions_info::TransactionsInfo;
use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::sexp::SExp;
use dg_xch_core::consensus::block_filter::chia_block_filter;
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, additions_for_conditions, additions_root,
    execute_block_generator_result, removals_for_conditions, removals_root,
    transactions_generator_refs_root, transactions_generator_root, transactions_info_hash,
};
use dg_xch_core::consensus::block_rewards::{calculate_base_farmer_reward, calculate_pool_reward};
use dg_xch_core::consensus::coinbase::{create_farmer_coin, create_pool_coin};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::traits::SizedBytes;
use dg_xch_core::utils::hash_256;
use dg_xch_node::engine::BlockDelta;
use dg_xch_node::{AddBlockOutcome, Engine, NativePrimitives};
use dg_xch_stores::{BlockStore, CoinStore, SqliteStore};

// Post-hard-fork (>5,496,000), pre-soft-fork-9 (<8,655,000) — the flag regime the synthetic
// generator vehicle in t070 uses. P (the fork parent, height H-1) sits at this base.
const P_HEIGHT: u32 = 6_000_100; // H-1
const H: u32 = P_HEIGHT + 1; // the tie height — A (confirmed) and B (orphan) both live here
const C_HEIGHT: u32 = H + 1; // the child of the orphan B

fn delta_for(record: &BlockRecord, additions: Vec<CoinRecord>) -> BlockDelta {
    BlockDelta {
        header_hash: record.header_hash,
        prev_hash: record.prev_hash,
        height: record.height,
        weight: record.weight,
        timestamp: record.timestamp.unwrap_or(0),
        record: record.clone(),
        additions,
        removals: Vec::new(),
        hints: Vec::new(),
    }
}

fn tx_record(
    template: &BlockRecord,
    tag: u8,
    height: u32,
    weight: u128,
    prev: Bytes32,
    timestamp: u64,
    fees: u64,
) -> BlockRecord {
    let mut r = template.clone();
    r.header_hash = common::synth_hash(tag, height);
    r.prev_hash = prev;
    r.height = height;
    r.weight = weight;
    r.total_iters = weight;
    r.timestamp = Some(timestamp);
    r.fees = Some(fees);
    r.sub_epoch_summary_included = None;
    r
}

/// The exact reward claims a child transaction block must incorporate for `record`
/// (a transaction block directly on top of another transaction block).
fn claims_for(record: &BlockRecord) -> Vec<Coin> {
    vec![
        create_pool_coin(
            record.height,
            record.pool_puzzle_hash,
            calculate_pool_reward(record.height),
            MAINNET.genesis_challenge,
        ),
        create_farmer_coin(
            record.height,
            record.farmer_puzzle_hash,
            calculate_base_farmer_reward(record.height) + record.fees.unwrap_or(0),
            MAINNET.genesis_challenge,
        ),
    ]
}

fn quoted_generator(output: SExp<'static>) -> SerializedProgram {
    Program::to((1_u8, output)).serialized().unwrap()
}

fn puzzle_for_conditions(conditions: Vec<ConditionWithArgs>) -> Program<'static> {
    let condition_sexps = conditions
        .iter()
        .map(|condition| SExp::from(condition).to_owned())
        .collect::<Vec<_>>();
    Program::to((1_u8, SExp::from(condition_sexps)))
}

fn spend_output(parent: Bytes32, amount: u64, puzzle: &Program<'static>) -> SExp<'static> {
    SExp::from(vec![
        SExp::from(parent),
        puzzle.sexp().to_owned(),
        SExp::from(amount),
        SExp::default(),
    ])
}

/// A body-consistent synthetic transaction block on top of `prev_record`: the generator runs
/// through the same flag ladder the engine uses and ti/foliage are recomputed from the resulting
/// conditions, so every body gate (roots, filter, ti-hash, cost, reward claims) passes. Mirrors
/// the t070 `synth_tx_block` vehicle.
fn synth_tx_block(
    prev_record: &BlockRecord,
    height: u32,
    weight: u128,
    spends: Vec<SExp<'static>>,
    claims: Vec<Coin>,
    fees: u64,
) -> FullBlock {
    let output = SExp::from(vec![SExp::from(spends)]);
    let generator = quoted_generator(output);
    let input = BlockGeneratorInput {
        transactions_generator: generator.clone(),
        generator_refs: Vec::new(),
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::for_height(&MAINNET, height),
    };
    let conds = execute_block_generator_result(&input).expect("synthetic generator runs");
    let ti = TransactionsInfo {
        generator_root: transactions_generator_root(&generator),
        generator_refs_root: transactions_generator_refs_root(&[]).unwrap(),
        aggregated_signature: SpendBundle::empty().aggregated_signature,
        fees,
        cost: conds.cost,
        reward_claims_incorporated: claims,
    };
    let all_additions = additions_for_conditions(&conds, &ti.reward_claims_incorporated);
    let all_removals = removals_for_conditions(&conds);
    let mut filter_items: Vec<Vec<u8>> = Vec::new();
    for coin in &all_additions {
        filter_items.push(coin.puzzle_hash.bytes().to_vec());
    }
    for name in &all_removals {
        filter_items.push(name.bytes().to_vec());
    }
    let ftb = FoliageTransactionBlock {
        prev_transaction_block_hash: prev_record.header_hash,
        timestamp: prev_record.timestamp.unwrap_or(0) + 10,
        filter_hash: Bytes32::new(hash_256(chia_block_filter(&filter_items))),
        additions_root: additions_root(&conds, &ti.reward_claims_incorporated).unwrap(),
        removals_root: removals_root(&conds),
        transactions_info_hash: transactions_info_hash(&ti).unwrap(),
    };
    let mut b = common::load_full_block(5_000_000);
    b.reward_chain_block.height = height;
    b.reward_chain_block.weight = weight;
    b.foliage.prev_block_hash = prev_record.header_hash;
    b.foliage.foliage_transaction_block_hash = Some(ftb.hash().unwrap());
    b.foliage_transaction_block = Some(ftb);
    b.transactions_info = Some(ti);
    b.transactions_generator = Some(generator);
    b.transactions_generator_ref_list = Vec::new();
    b
}

/// A spendable coin whose puzzle creates exactly one child coin (`next_ph`, `next_amount`).
/// Returns (coin, its puzzle). Used to seed a coin below the fork and to chain a fork block's
/// created coin to the next spend.
fn spendable_coin(
    parent: Bytes32,
    amount: u64,
    next_ph: Bytes32,
    next_amount: u64,
) -> (Coin, Program<'static>) {
    let puzzle = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoin(
        next_ph,
        next_amount,
        Vec::new(),
    )]);
    let coin = Coin {
        parent_coin_info: parent,
        puzzle_hash: puzzle.tree_hash(),
        amount,
    };
    (coin, puzzle)
}

/// The confirmed base `grand(H-2) → P(H-1)`, with `coin_z` (amount 12000) seeded unspent at P for
/// a fork block to spend. Returns the engine, P's record, coin_z, and coin_z's puzzle (which
/// creates `coin_w` = (parent coin_z, `ph_w`, 11000)).
async fn seed_base_with_coin(
    path: &std::path::Path,
    ph_w: Bytes32,
) -> (
    Engine<SqliteStore, NativePrimitives>,
    BlockRecord,
    Coin,
    Program<'static>,
) {
    let template = &common::load_records()[0];
    let (coin_z, puzzle_z) = spendable_coin(common::synth_hash(0xcc, 1), 12_000, ph_w, 11_000);

    let grand = tx_record(
        template,
        0xf1,
        P_HEIGHT - 1,
        9_000,
        common::synth_hash(0xf1, P_HEIGHT - 2),
        1_700_000_000,
        0,
    );
    let p = tx_record(
        template,
        0xf0,
        P_HEIGHT,
        9_010,
        grand.header_hash,
        1_700_000_010,
        0,
    );
    let coin_z_rec = CoinRecord {
        coin: coin_z,
        confirmed_block_index: p.height,
        spent_block_index: 0,
        coinbase: false,
        timestamp: p.timestamp.unwrap(),
        spent: false,
    };

    let store = SqliteStore::open(path).await.expect("open");
    let mut engine = Engine::new(store, NativePrimitives, MAINNET).with_enforced_coin_rules();
    assert_eq!(
        engine
            .add_delta(delta_for(&grand, Vec::new()))
            .await
            .unwrap(),
        AddBlockOutcome::NewPeak {
            height: P_HEIGHT - 1
        }
    );
    // P confirms and applies coin_z to the store (unspent, at the fork height).
    assert_eq!(
        engine
            .add_delta(delta_for(&p, vec![coin_z_rec]))
            .await
            .unwrap(),
        AddBlockOutcome::Extended { height: P_HEIGHT }
    );
    (engine, p, coin_z, puzzle_z)
}

// ---------------------------------------------------------------------------------------------
// Main: a 1-block EQUAL-WEIGHT tie-break, then a heavier child on the parked branch, resolved
// ACROSS A RESTART — the minimized failing case:
//
//   grand(H-2) ── P(H-1, seeds coin_z) ── A(H, weight W)          [confirmed main chain]
//                                       └─ B(H, weight W == A)     [orphan via add_block: record
//                                       │                           AND BODY persisted, delta only
//                                       │                           in the in-memory pending]
//                                            └─ C(H+1, weight > W) [heavier — must reorg]
//
// B spends coin_z (below the fork) and creates coin_w; C spends coin_w (a coin that exists ONLY on
// the branch). After the restart the fresh engine's pending/staged_deltas are empty, so C's body
// validation must rebuild B's coin delta — including coin_w and the coin_z spend — from B's
// persisted STORE BODY, fork at P (H-1), and reorg.
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn reorg_to_heavier_branch_when_fork_ancestor_only_in_store() {
    let path = common::unique_db_path();
    let template = common::load_records()[0].clone();

    // Coin chain: coin_z (at P) --B--> coin_w --C--> coin_v (terminal 0x77).
    let puzzle_w = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoin(
        Bytes32::new([0x77; 32]),
        10_000,
        Vec::new(),
    )]);
    let ph_w = puzzle_w.tree_hash();

    let tie_weight = 9_100u128;
    let (mut engine, p, coin_z, puzzle_z) = seed_base_with_coin(&path, ph_w).await;
    let coin_w = Coin {
        parent_coin_info: coin_z.name(),
        puzzle_hash: ph_w,
        amount: 11_000,
    };

    // A@H confirms first and becomes the peak (the tie winner, `7f14ee5b`).
    let a = tx_record(
        &template,
        0xa0,
        H,
        tie_weight,
        p.header_hash,
        1_700_000_020,
        0,
    );
    assert_eq!(
        engine.add_delta(delta_for(&a, Vec::new())).await.unwrap(),
        AddBlockOutcome::Extended { height: H }
    );

    // B@H, equal weight, spends coin_z and creates coin_w — added via add_block so its BODY is
    // persisted (persist_archive runs before fork choice), then parked as an orphan.
    let b = synth_tx_block(
        &p,
        H,
        tie_weight, // == A ⇒ orphan (equal weight keeps the peak)
        vec![spend_output(
            coin_z.parent_coin_info,
            coin_z.amount,
            &puzzle_z,
        )],
        claims_for(&p),
        1_000,
    );
    let b_hash = b.header_hash().unwrap();
    assert_eq!(
        engine
            .add_block(&b)
            .await
            .expect("B validates as an orphan"),
        AddBlockOutcome::Orphan { height: H }
    );
    // C's reward claims must match B's DERIVED record (compute_record fills pool/farmer hashes from
    // the body), so build C from B's stored record now.
    let b_record = engine
        .store()
        .get_block_record(&b_hash)
        .await
        .unwrap()
        .expect("B's record is persisted");
    let c = synth_tx_block(
        &b_record,
        C_HEIGHT,
        tie_weight + 100, // strictly heavier than the peak A@H ⇒ must reorg
        vec![spend_output(
            coin_w.parent_coin_info,
            coin_w.amount,
            &puzzle_w,
        )],
        claims_for(&b_record),
        1_000,
    );
    let c_hash = c.header_hash().unwrap();
    drop(engine); // KILL — the restart.

    // Session 2: reopen the same file. B's non-confirmed record AND body survive; pending/staged
    // are empty. The confirmed peak is still A@H.
    let store = SqliteStore::open(&path).await.expect("reopen");
    assert_eq!(
        store.get_peak().await.unwrap(),
        Some((a.header_hash, H)),
        "the confirmed peak survives the restart"
    );
    assert_ne!(
        store
            .get_block_record_by_height(H)
            .await
            .unwrap()
            .unwrap()
            .header_hash,
        b_hash,
        "B is stored NON-confirmed: height H resolves to A, not B"
    );
    let mut engine = Engine::new(store, NativePrimitives, MAINNET).with_enforced_coin_rules();

    // C reorgs to the heavier branch, forking at P (H-1) — B's coin delta rebuilt from the store.
    let outcome = engine
        .add_block(&c)
        .await
        .expect("heavier branch whose fork ancestor is only in the store must reorg");
    assert_eq!(
        outcome,
        AddBlockOutcome::Reorg {
            fork_height: P_HEIGHT,
            links: 2
        },
        "fork at the confirmed parent P (H-1), branch = [B, C]"
    );

    // The reorg landed the WHOLE reconstructed branch: peak = C, coin_z and coin_w spent, coin_v
    // (created by C) present and unspent, A's height reverted to C's branch.
    assert_eq!(
        engine.store().get_peak().await.unwrap(),
        Some((c_hash, C_HEIGHT))
    );
    let z = engine
        .store()
        .get_coin_record(&coin_z.name())
        .await
        .unwrap()
        .expect("coin_z present");
    assert!(
        z.spent && z.spent_block_index == H,
        "coin_z spent by B at H"
    );
    let w = engine
        .store()
        .get_coin_record(&coin_w.name())
        .await
        .unwrap()
        .expect("coin_w present (created by the reconstructed B)");
    assert!(
        w.spent && w.spent_block_index == C_HEIGHT,
        "coin_w created by B, spent by C"
    );
    let coin_v = Coin {
        parent_coin_info: coin_w.name(),
        puzzle_hash: Bytes32::new([0x77; 32]),
        amount: 10_000,
    };
    let v = engine
        .store()
        .get_coin_record(&coin_v.name())
        .await
        .unwrap()
        .expect("coin_v present");
    assert!(!v.spent && v.confirmed_block_index == C_HEIGHT);
}

// ---------------------------------------------------------------------------------------------
// Over-reorg guard: the store-backed fork walk must NOT reorg to a branch that is not heavier than
// the peak (an equal/lighter-weight competitor keeps the peak). Same
// store reconstruction as the main test (C's fork ancestor B is only in the store after a
// restart), but the main chain is extended one block beyond the fork so the peak outweighs C. C
// must validate (fork_view rebuilds B from the store — no horizon error) yet park as an ORPHAN,
// never reorg.
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn reconstructable_branch_that_is_not_heavier_parks_as_orphan_and_does_not_reorg() {
    let path = common::unique_db_path();
    let template = common::load_records()[0].clone();

    let puzzle_w = puzzle_for_conditions(vec![ConditionWithArgs::CreateCoin(
        Bytes32::new([0x77; 32]),
        10_000,
        Vec::new(),
    )]);
    let ph_w = puzzle_w.tree_hash();

    let tie_weight = 9_100u128;
    let (mut engine, p, coin_z, puzzle_z) = seed_base_with_coin(&path, ph_w).await;
    let coin_w = Coin {
        parent_coin_info: coin_z.name(),
        puzzle_hash: ph_w,
        amount: 11_000,
    };

    // Main chain A@H then A2@H+1 — the peak (weight 9_300) outweighs any child of B.
    let a = tx_record(
        &template,
        0xa0,
        H,
        tie_weight,
        p.header_hash,
        1_700_000_020,
        0,
    );
    let a2 = tx_record(
        &template,
        0xa2,
        H + 1,
        9_300,
        a.header_hash,
        1_700_000_030,
        0,
    );
    assert_eq!(
        engine.add_delta(delta_for(&a, Vec::new())).await.unwrap(),
        AddBlockOutcome::Extended { height: H }
    );
    assert_eq!(
        engine.add_delta(delta_for(&a2, Vec::new())).await.unwrap(),
        AddBlockOutcome::Extended { height: H + 1 }
    );

    // B@H orphan (weight tie < peak), spends coin_z, creates coin_w — body persisted.
    let b = synth_tx_block(
        &p,
        H,
        tie_weight,
        vec![spend_output(
            coin_z.parent_coin_info,
            coin_z.amount,
            &puzzle_z,
        )],
        claims_for(&p),
        1_000,
    );
    let b_hash = b.header_hash().unwrap();
    assert_eq!(
        engine.add_block(&b).await.expect("B orphan"),
        AddBlockOutcome::Orphan { height: H }
    );
    let b_record = engine
        .store()
        .get_block_record(&b_hash)
        .await
        .unwrap()
        .unwrap();
    // C@H+1 on B: heavier than B (9_200 > 9_100) but LIGHTER than the peak A2 (9_300).
    let c = synth_tx_block(
        &b_record,
        C_HEIGHT,
        tie_weight + 100,
        vec![spend_output(
            coin_w.parent_coin_info,
            coin_w.amount,
            &puzzle_w,
        )],
        claims_for(&b_record),
        1_000,
    );
    drop(engine);

    // Restart: pending empty; B only in the store.
    let store = SqliteStore::open(&path).await.expect("reopen");
    let mut engine = Engine::new(store, NativePrimitives, MAINNET).with_enforced_coin_rules();

    // C's body validates (fork_view rebuilds B from the store — no reorg-horizon error), but C is
    // NOT heavier than the peak, so it parks as an orphan and the peak is unchanged.
    let outcome = engine
        .add_block(&c)
        .await
        .expect("C validates via the store-backed fork walk");
    assert_eq!(
        outcome,
        AddBlockOutcome::Orphan { height: C_HEIGHT },
        "a reconstructable branch that is not heavier than the peak must NOT reorg"
    );
    assert_eq!(
        engine.store().get_peak().await.unwrap(),
        Some((a2.header_hash, H + 1)),
        "the peak is unchanged — no over-reorg"
    );
    // coin_z stays unspent (B was never applied): the abandoned branch touched no confirmed state.
    let z = engine
        .store()
        .get_coin_record(&coin_z.name())
        .await
        .unwrap()
        .unwrap();
    assert!(!z.spent, "coin_z remains unspent — the branch did not land");
}

/// Add a branch transaction block that spends `spend_coin`, asserting it parks as an orphan
/// (its record + body are persisted), and return its DERIVED record (so the next block's reward
/// claims can be built from it). Used to lay down a deep, all-orphan branch.
async fn add_branch_orphan(
    engine: &mut Engine<SqliteStore, NativePrimitives>,
    prev_record: &BlockRecord,
    height: u32,
    weight: u128,
    spend_coin: Coin,
    spend_puzzle: &Program<'static>,
) -> BlockRecord {
    let block = synth_tx_block(
        prev_record,
        height,
        weight,
        vec![spend_output(
            spend_coin.parent_coin_info,
            spend_coin.amount,
            spend_puzzle,
        )],
        claims_for(prev_record),
        1_000,
    );
    let hash = block.header_hash().unwrap();
    assert_eq!(
        engine
            .add_block(&block)
            .await
            .expect("branch block validates as an orphan"),
        AddBlockOutcome::Orphan { height }
    );
    engine
        .store()
        .get_block_record(&hash)
        .await
        .unwrap()
        .expect("branch block record persisted")
}

// ---------------------------------------------------------------------------------------------
// Depth-unbounded guard: a 3-block orphan branch (B1@H, B2@H+1, B3@H+2), each spending a distinct
// coin seeded below the fork, is parked with only records + bodies in the store. After a RESTART
// (pending empty), a heavier tip B4@H+3 on B3 must reorg — its fork context and reorg branch are
// rebuilt for ALL THREE stored ancestors from the durable store, forking at P (H-1). A reorg of
// ANY depth wins; there is no reorg-depth cap — the walk streams ancestor-by-ancestor to the
// confirmed chain.
// ---------------------------------------------------------------------------------------------
#[tokio::test]
async fn deep_reorg_reconstructs_a_multi_block_branch_from_the_store_across_a_restart() {
    let path = common::unique_db_path();
    let template = common::load_records()[0].clone();

    // Four independent spendable coins seeded unspent at P (one per branch block). Each creates a
    // distinct terminal coin, so no branch block depends on another's output — the reconstruction
    // exercised is purely the depth of the fork walk.
    let terminals = [
        Bytes32::new([0x71; 32]),
        Bytes32::new([0x72; 32]),
        Bytes32::new([0x73; 32]),
        Bytes32::new([0x74; 32]),
    ];
    let coins: Vec<(Coin, Program<'static>)> = (0..4u32)
        .map(|i| {
            spendable_coin(
                common::synth_hash(0xcc, i + 1),
                12_000,
                terminals[i as usize],
                11_000,
            )
        })
        .collect();

    let grand = tx_record(
        &template,
        0xf1,
        P_HEIGHT - 1,
        9_000,
        common::synth_hash(0xf1, P_HEIGHT - 2),
        1_700_000_000,
        0,
    );
    let p = tx_record(
        &template,
        0xf0,
        P_HEIGHT,
        9_010,
        grand.header_hash,
        1_700_000_010,
        0,
    );
    let p_adds: Vec<CoinRecord> = coins
        .iter()
        .map(|(c, _)| CoinRecord {
            coin: *c,
            confirmed_block_index: p.height,
            spent_block_index: 0,
            coinbase: false,
            timestamp: p.timestamp.unwrap(),
            spent: false,
        })
        .collect();

    let store = SqliteStore::open(&path).await.expect("open");
    let mut engine = Engine::new(store, NativePrimitives, MAINNET).with_enforced_coin_rules();
    assert_eq!(
        engine
            .add_delta(delta_for(&grand, Vec::new()))
            .await
            .unwrap(),
        AddBlockOutcome::NewPeak {
            height: P_HEIGHT - 1
        }
    );
    assert_eq!(
        engine.add_delta(delta_for(&p, p_adds)).await.unwrap(),
        AddBlockOutcome::Extended { height: P_HEIGHT }
    );

    // Main chain A@H, A2@H+1, A3@H+2 — the peak (weight 9_300) is heavier than every branch block
    // until the tip, so B1..B3 all park as orphans.
    let a = tx_record(&template, 0xa0, H, 9_100, p.header_hash, 1_700_000_020, 0);
    let a2 = tx_record(
        &template,
        0xa2,
        H + 1,
        9_200,
        a.header_hash,
        1_700_000_030,
        0,
    );
    let a3 = tx_record(
        &template,
        0xa3,
        H + 2,
        9_300,
        a2.header_hash,
        1_700_000_040,
        0,
    );
    for r in [&a, &a2, &a3] {
        assert_eq!(
            engine.add_delta(delta_for(r, Vec::new())).await.unwrap(),
            AddBlockOutcome::Extended { height: r.height }
        );
    }

    // The 3-block orphan branch on P, each spending its own below-fork coin. Bodies persisted;
    // deltas live only in the in-memory pending.
    let b1 = add_branch_orphan(&mut engine, &p, H, 9_050, coins[0].0, &coins[0].1).await;
    let b2 = add_branch_orphan(&mut engine, &b1, H + 1, 9_060, coins[1].0, &coins[1].1).await;
    let b3 = add_branch_orphan(&mut engine, &b2, H + 2, 9_070, coins[2].0, &coins[2].1).await;

    // The heavier tip B4@H+3 on B3 (weight 9_400 > the peak 9_300), spending the fourth coin.
    let b4 = synth_tx_block(
        &b3,
        H + 3,
        9_400,
        vec![spend_output(
            coins[3].0.parent_coin_info,
            coins[3].0.amount,
            &coins[3].1,
        )],
        claims_for(&b3),
        1_000,
    );
    let b4_hash = b4.header_hash().unwrap();
    drop(engine); // restart

    let store = SqliteStore::open(&path).await.expect("reopen");
    assert_eq!(
        store.get_peak().await.unwrap(),
        Some((a3.header_hash, H + 2)),
        "the main-chain peak A3 survives the restart"
    );
    let mut engine = Engine::new(store, NativePrimitives, MAINNET).with_enforced_coin_rules();

    // B4 reorgs, forking at P (H-1). The whole 4-block branch [B1,B2,B3,B4] is re-applied — B1..B3
    // rebuilt from the store, B4 from the incoming block.
    let outcome = engine
        .add_block(&b4)
        .await
        .expect("a heavier tip on a deep store-only branch must reorg at any depth");
    assert_eq!(
        outcome,
        AddBlockOutcome::Reorg {
            fork_height: P_HEIGHT,
            links: 4
        },
        "depth-unbounded reorg: fork at P (H-1), 4-block branch"
    );
    assert_eq!(
        engine.store().get_peak().await.unwrap(),
        Some((b4_hash, H + 3)),
        "peak is the branch tip B4"
    );
    // Every branch coin was spent by the re-applied branch — proof the reconstructed deltas
    // (not just the records) were applied.
    for (i, (c, _)) in coins.iter().enumerate() {
        let rec = engine
            .store()
            .get_coin_record(&c.name())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("coin {i} present"));
        assert!(rec.spent, "branch coin {i} spent by the re-applied branch");
    }
}
