// Two-branch delta chains with a per-height synthetic coin lineage — the at-scale reorg
// fixture shared by tests/long_reorg_scale.rs and tests/reorg_while_shed.rs. Block h on branch
// `tag` creates coin L(tag, h) and spends L(tag, h-1); the FIRST block of each branch spends a
// fork-common coin both branches contend for (chia's `_spend_reorg_coin` shape). Branch A (+10
// weight/block) is the incumbent peak; branch B (+8/block, one block longer, tip jumping past
// the A tip) parks WHOLLY as orphan candidates and flips in ONE reorg — so every coin above the
// fork has real unwind work: abandoned creations deleted, the below-fork spend reverted, the
// winning branch re-applied exactly.

use super::synth_hash;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_node::engine::BlockDelta;
use dg_xch_stores::{BlockStore, CoinStore};

pub const FORK: u32 = 100; // the last common height: base block B0@100
pub const BASE_WEIGHT: u128 = 1_000;
pub const A_TAG: u8 = 0xa1;
pub const B_TAG: u8 = 0xb1;

// Per-height branch lineage coin: distinct name per (branch tag, height) — the parent hash
// carries the branch tag so the two branches' coin sets are disjoint.
#[must_use]
pub fn lineage_coin(tag: u8, h: u32) -> Coin {
    Coin {
        parent_coin_info: synth_hash(tag ^ 0x0F, h),
        puzzle_hash: synth_hash(0x77, h),
        amount: 1_000 + u64::from(h),
    }
}

// The fork-common coin created below the fork and spent by the FIRST block of BOTH branches —
// the reorg must revert branch A's spend and re-apply it as branch B's.
#[must_use]
pub fn fork_coin() -> Coin {
    Coin {
        parent_coin_info: Bytes32::from([0xC0; 32]),
        puzzle_hash: Bytes32::from([0xC1; 32]),
        amount: 1_000_000,
    }
}

fn record_at(template: &BlockRecord, tag: u8, h: u32, weight: u128, prev: Bytes32) -> BlockRecord {
    let mut r = template.clone();
    r.header_hash = synth_hash(tag, h);
    r.prev_hash = prev;
    r.height = h;
    r.weight = weight;
    r.total_iters = weight;
    r.timestamp = Some(1_700_000_000 + u64::from(h));
    r.sub_epoch_summary_included = None;
    r
}

// One branch block's delta: creates L(tag, h), spends L(tag, h-1) (or the fork-common coin at
// the branch base).
fn branch_delta(
    template: &BlockRecord,
    tag: u8,
    h: u32,
    weight: u128,
    prev: Bytes32,
) -> BlockDelta {
    let ts = 1_700_000_000 + u64::from(h);
    let addition = CoinRecord {
        coin: lineage_coin(tag, h),
        confirmed_block_index: h,
        spent_block_index: 0,
        coinbase: false,
        timestamp: ts,
        spent: false,
    };
    let removal = if h == FORK + 1 {
        fork_coin().name()
    } else {
        lineage_coin(tag, h - 1).name()
    };
    BlockDelta {
        header_hash: synth_hash(tag, h),
        prev_hash: prev,
        height: h,
        weight,
        timestamp: ts,
        record: record_at(template, tag, h, weight, prev),
        additions: vec![addition],
        removals: vec![removal],
        hints: Vec::new(),
    }
}

// The base delta B0@FORK: no lineage, creates the fork-common coin.
fn base_delta(template: &BlockRecord) -> BlockDelta {
    let ts = 1_700_000_000 + u64::from(FORK);
    BlockDelta {
        header_hash: synth_hash(0x00, FORK),
        prev_hash: Bytes32::from([0u8; 32]),
        height: FORK,
        weight: BASE_WEIGHT,
        timestamp: ts,
        record: record_at(template, 0x00, FORK, BASE_WEIGHT, Bytes32::from([0u8; 32])),
        additions: vec![CoinRecord {
            coin: fork_coin(),
            confirmed_block_index: FORK,
            spent_block_index: 0,
            coinbase: false,
            timestamp: ts,
            spent: false,
        }],
        removals: Vec::new(),
        hints: Vec::new(),
    }
}

#[must_use]
pub fn a_weight(h: u32) -> u128 {
    BASE_WEIGHT + u128::from(h - FORK) * 10
}

#[must_use]
pub fn b_weight(h: u32, n: u32) -> u128 {
    if h == FORK + n + 1 {
        a_weight(FORK + n) + 1
    } else {
        BASE_WEIGHT + u128::from(h - FORK) * 8
    }
}

pub struct Branches {
    pub base: BlockDelta,
    pub a: Vec<BlockDelta>,
    pub b: Vec<BlockDelta>,
}

// Branch A: n blocks (the incumbent peak). Branch B: n+1 blocks — every B block LIGHTER than
// the A tip until the (n+1)-th, which jumps past it.
#[must_use]
pub fn build_branches(n: u32) -> Branches {
    let records = super::load_records();
    let template = &records[0];
    let base = base_delta(template);
    let mut a = Vec::with_capacity(n as usize);
    let mut prev = base.header_hash;
    for h in FORK + 1..=FORK + n {
        let d = branch_delta(template, A_TAG, h, a_weight(h), prev);
        prev = d.header_hash;
        a.push(d);
    }
    let mut b = Vec::with_capacity(n as usize + 1);
    let mut prev = base.header_hash;
    for h in FORK + 1..=FORK + n + 1 {
        let d = branch_delta(template, B_TAG, h, b_weight(h, n), prev);
        prev = d.header_hash;
        b.push(d);
    }
    Branches { base, a, b }
}

// Every coin name either branch touches, plus the fork-common coin: the domain over which the
// flipped store must byte-equal a from-scratch replay of the winning chain.
#[must_use]
pub fn touched_names(n: u32) -> Vec<Bytes32> {
    let mut names = vec![fork_coin().name()];
    for h in FORK + 1..=FORK + n + 1 {
        names.push(lineage_coin(A_TAG, h).name());
        names.push(lineage_coin(B_TAG, h).name());
    }
    names
}

pub async fn all_coin_records(
    store: &impl CoinStore,
    names: &[Bytes32],
) -> Vec<(Bytes32, Option<CoinRecord>)> {
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        out.push((*n, store.get_coin_record(n).await.unwrap()));
    }
    out
}

// The invariant: peak == expected tip AND the store's coin records over every touched name
// byte-equal a fresh replay of `chain` (the `tests/reorg.rs` assert_peak_chain_consistent
// shape, replayed into a throwaway SQLite store regardless of the backend under test —
// CoinRecord equality is backend-independent).
pub async fn assert_equals_replay<S: CoinStore + BlockStore>(
    store: &S,
    expected_peak: (Bytes32, u32),
    chain: &[&BlockDelta],
    names: &[Bytes32],
    context: &str,
) {
    let peak = store.get_peak().await.unwrap().unwrap();
    assert_eq!(peak, expected_peak, "{context}: peak");
    let replay = super::new_store().await;
    for d in chain {
        replay
            .apply_block(d.height, d.timestamp, &d.additions, &d.removals)
            .await
            .unwrap();
    }
    let actual = all_coin_records(store, names).await;
    let expected = all_coin_records(&replay, names).await;
    // Compare pairwise so a failure names the first diverging coin instead of dumping the set.
    for (a, e) in actual.iter().zip(expected.iter()) {
        assert_eq!(
            a, e,
            "{context}: coin state must equal the winning-chain replay"
        );
    }
    // The confirmed by-height chain must be the winning branch everywhere above the fork.
    for d in chain {
        let by_height = store
            .get_block_record_by_height(d.height)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{context}: height {} confirmed", d.height));
        assert_eq!(
            by_height.header_hash, d.header_hash,
            "{context}: main chain at height {}",
            d.height
        );
    }
}
