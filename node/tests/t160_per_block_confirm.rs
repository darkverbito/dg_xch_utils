mod common;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_node::engine::BlockDelta;
use dg_xch_node::{AddBlockOutcome, Engine, NativePrimitives};
use dg_xch_stores::{BlockStore, CoinStore};

fn synth_hash(tag: u8, height: u32) -> Bytes32 {
    let mut h = [tag; 32];
    h[28..32].copy_from_slice(&height.to_be_bytes());
    Bytes32::from(h)
}

// A synthetic extending delta carrying one distinct coin, built off a real record template (as reorg.rs does).
// confirm_staged_batch validates nothing (staging did), so a template record + chained hashes is enough to
// exercise the per-block apply-coins + set-peak path.
fn extending_delta(
    template: &BlockRecord,
    height: u32,
    weight: u128,
    prev_hash: Bytes32,
) -> BlockDelta {
    let ts = 1_700_000_000u64 + u64::from(height);
    let hh = synth_hash(0xc0, height);
    let mut record = template.clone();
    record.header_hash = hh;
    record.prev_hash = prev_hash;
    record.height = height;
    record.weight = weight;
    record.total_iters = weight;
    record.timestamp = Some(ts);
    record.sub_epoch_summary_included = None;
    let coin = Coin {
        parent_coin_info: synth_hash(0xee, height),
        puzzle_hash: synth_hash(0xdd, height),
        amount: 1_000 + u64::from(height),
    };
    let additions = vec![CoinRecord {
        coin,
        confirmed_block_index: height,
        spent_block_index: 0,
        coinbase: false,
        timestamp: ts,
        spent: false,
    }];
    BlockDelta {
        header_hash: hh,
        prev_hash,
        height,
        weight,
        timestamp: ts,
        record,
        additions,
        removals: Vec::new(),
        hints: Vec::new(),
    }
}

// The window confirm commits ONE atomic transaction PER BLOCK. Confirming a 3-block window advances the
// DURABLE peak to the tip, one outcome per block, and every block's coin is committed at its own height.
#[tokio::test]
async fn near_tip_mode_commits_per_block_and_advances_peak() {
    let records = common::load_records();
    let template = &records[0];
    let d0 = extending_delta(template, 100, 1000, Bytes32::from([0u8; 32]));
    let d1 = extending_delta(template, 101, 1100, d0.header_hash);
    let d2 = extending_delta(template, 102, 1200, d1.header_hash);

    let store = common::new_store().await;
    // Staging persists the records before confirm; set_peak resolves height + walks ancestry, so pre-seed them.
    store
        .add_block_records(&[d0.record.clone(), d1.record.clone(), d2.record.clone()])
        .await
        .unwrap();
    // NEAR-TIP band: per-block commits.
    store.set_near_tip(true);
    let mut engine = Engine::new(store, NativePrimitives, MAINNET);

    let outcomes = engine
        .confirm_staged_batch(vec![d0.clone(), d1.clone(), d2.clone()])
        .await
        .unwrap();
    assert_eq!(
        outcomes,
        vec![
            AddBlockOutcome::NewPeak { height: 100 },
            AddBlockOutcome::Extended { height: 101 },
            AddBlockOutcome::Extended { height: 102 },
        ],
        "one outcome per block: the first is the peak, the rest plain extensions"
    );

    // The durable confirmed peak is the tip of the window.
    let peak = engine.store().get_peak().await.unwrap().unwrap();
    assert_eq!(
        peak,
        (d2.header_hash, 102),
        "peak advanced to the window tip"
    );

    // Each block's coin is durably committed at its OWN height (the per-block transactions).
    for d in [&d0, &d1, &d2] {
        let name = d.additions[0].coin.name();
        let got = engine.store().get_coin_records(&[name]).await.unwrap();
        assert_eq!(got.len(), 1, "block {} coin is committed", d.height);
        assert_eq!(
            got[0].confirmed_block_index, d.height,
            "coin confirmed at its own block height"
        );
    }
}

// A partial window leaves the peak at the LAST fully-committed block, never ahead of it. The window
// [d0, gap] skips height 101, so `gap` (prev = the never-confirmed 101) does not extend d0: the loop commits
// d0 per-block (peak = d0) and the non-extending block takes the sequential path without advancing the durable
// peak. This is the reorg-safety invariant: on any mid-window stop, peak = last committed block, re-fetch from
// there.
#[tokio::test]
async fn partial_window_leaves_peak_at_last_committed_block() {
    let records = common::load_records();
    let template = &records[0];
    let d0 = extending_delta(template, 100, 1000, Bytes32::from([0u8; 32]));
    // A LIGHTER block on a separate branch (weight 900 < d0 weight 1000): it cannot extend d0 and loses
    // fork choice, so it parks as an orphan and never becomes the peak.
    let orphan = extending_delta(template, 101, 900, synth_hash(0xff, 100));

    let store = common::new_store().await;
    store
        .add_block_records(&[d0.record.clone(), orphan.record.clone()])
        .await
        .unwrap();
    // NEAR-TIP band: per-block commits.
    store.set_near_tip(true);
    let mut engine = Engine::new(store, NativePrimitives, MAINNET);

    let outcomes = engine
        .confirm_staged_batch(vec![d0.clone(), orphan.clone()])
        .await
        .unwrap();

    // d0 committed per-block as the peak; the lighter orphan did not extend and never became the peak.
    assert_eq!(
        outcomes[0],
        AddBlockOutcome::NewPeak { height: 100 },
        "the extending prefix commits per-block"
    );
    let peak = engine.store().get_peak().await.unwrap().unwrap();
    assert_eq!(
        peak,
        (d0.header_hash, 100),
        "peak stays at the last committed block; the partial window did not advance it past d0"
    );

    // d0 coin is durable even though the window did not complete (partial commit, not all-or-nothing).
    let name = d0.additions[0].coin.name();
    assert_eq!(
        engine
            .store()
            .get_coin_records(&[name])
            .await
            .unwrap()
            .len(),
        1,
        "the committed prefix is durable"
    );
}

// Never-wedge invariant: if a per-block commit sequence fails mid-window (the BatchHandle is dropped
// without a commit, e.g. a DB error at block K), the writer must NOT be left with a dangling transaction that
// wedges every later begin. The next begin -- the follow retry, or the next window -- must succeed.
#[tokio::test]
async fn dropped_uncommitted_batch_does_not_wedge_the_next_begin() {
    let store = common::new_store().await;
    let b = store.begin().await.expect("first begin");
    drop(b); // an error after begin, before commit: the batch (and its BEGIN) is dropped
    let b2 = store.begin().await;
    assert!(
        b2.is_ok(),
        "begin after a dropped uncommitted batch must recover, not wedge the writer: {:?}",
        b2.err()
    );
}

// CATCH-UP band -- with near_tip=false the window is confirmed as ONE batch transaction:
// the same 3-block window still advances the durable peak to the tip and commits every coin, so
// catch-up correctness matches near-tip; only the commit granularity (and thus WAL/liveness profile) differs.
#[tokio::test]
async fn catch_up_mode_batches_the_window_and_advances_peak() {
    let records = common::load_records();
    let template = &records[0];
    let d0 = extending_delta(template, 100, 1000, Bytes32::from([0u8; 32]));
    let d1 = extending_delta(template, 101, 1100, d0.header_hash);
    let d2 = extending_delta(template, 102, 1200, d1.header_hash);

    let store = common::new_store().await;
    store
        .add_block_records(&[d0.record.clone(), d1.record.clone(), d2.record.clone()])
        .await
        .unwrap();
    // CATCH-UP band: one batch transaction for the whole window.
    store.set_near_tip(false);
    let mut engine = Engine::new(store, NativePrimitives, MAINNET);

    let outcomes = engine
        .confirm_staged_batch(vec![d0.clone(), d1.clone(), d2.clone()])
        .await
        .unwrap();
    assert_eq!(
        outcomes,
        vec![
            AddBlockOutcome::NewPeak { height: 100 },
            AddBlockOutcome::Extended { height: 101 },
            AddBlockOutcome::Extended { height: 102 },
        ],
        "batch mode confirms the same window with one outcome per block"
    );
    let peak = engine.store().get_peak().await.unwrap().unwrap();
    assert_eq!(
        peak,
        (d2.header_hash, 102),
        "the batch commit advanced the peak to the window tip"
    );
    for d in [&d0, &d1, &d2] {
        let name = d.additions[0].coin.name();
        assert_eq!(
            engine
                .store()
                .get_coin_records(&[name])
                .await
                .unwrap()
                .len(),
            1,
            "block {} coin committed in the batch",
            d.height
        );
    }
}
