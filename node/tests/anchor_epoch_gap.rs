// The --sync-from anchor-window gap at a mainnet epoch boundary — a frozen sync leg
// (--sync-from=4575000, record floor 4,574,936, peak walled at 4,575,757):
//
// Staging the first new-slot block after epoch boundary 4,575,744 (= 993 * EPOCH_BLOCKS) runs the
// epoch retarget, whose `get_second_to_last_transaction_block_in_previous_epoch` walk descends
// prev-by-prev until it passes the PREVIOUS epoch surpass 4,571,136 — records one full epoch below
// anything the anchor span [H-64, H+31] ever seeded. The walk falls off the record floor and dies
// with `block record not found: <prev_hash of the floor record>`, and a forward retry of the same
// window can never succeed.
//
// This test reproduces that exact failing lookup against the real walk code with the real mainnet
// geometry, then proves the `epoch_backfill_low` coverage (the anchor/backfill/resume-repair depth
// contract) feeds the same walk to completion.

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::difficulty_adjustment::get_next_sub_slot_iters_and_difficulty;
use dg_xch_node::sync::{EPOCH_BACKFILL_SLACK, epoch_backfill_low};
use std::collections::HashMap;

// The live wall's geometry (mainnet constants: EPOCH_BLOCKS = 4608, SUB_EPOCH_BLOCKS = 384).
const PEAK: u32 = 4_575_757; // last confirmed height; 4,575,758 is the retarget trigger
const BOUNDARY: u32 = 4_575_744; // 993 * 4608, the epoch boundary 13 blocks below the peak
const PREV_SURPASS: u32 = BOUNDARY - 4608; // 4,571,136 — the walk must descend past this
const ANCHOR_FLOOR: u32 = 4_575_000 - 64; // 4,574,936 — the --sync-from span base (H - 64)

fn h32(n: u32) -> Bytes32 {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    Bytes32::from(b)
}

fn record(height: u32, ses: Option<SubEpochSummary>) -> BlockRecord {
    BlockRecord {
        header_hash: h32(height),
        prev_hash: h32(height.wrapping_sub(1)),
        height,
        weight: 7 * u128::from(height),
        total_iters: 10_000_000 * u128::from(height),
        signage_point_index: 0,
        challenge_vdf_output: ClassgroupElement::get_default_element(),
        infused_challenge_vdf_output: None,
        reward_infusion_new_challenge: Bytes32::default(),
        challenge_block_info_hash: Bytes32::default(),
        sub_slot_iters: MAINNET.sub_slot_iters_starting,
        pool_puzzle_hash: Bytes32::default(),
        farmer_puzzle_hash: Bytes32::default(),
        required_iters: 1,
        deficit: 0,
        overflow: false,
        prev_transaction_block_height: height.wrapping_sub(1),
        timestamp: Some(1_000 + u64::from(height)),
        prev_transaction_block_hash: None,
        fees: None,
        reward_claims_incorporated: None,
        finished_challenge_slot_hashes: None,
        finished_infused_challenge_slot_hashes: None,
        finished_reward_slot_hashes: None,
        sub_epoch_summary_included: ses,
    }
}

// A linear record chain over [low, high], with the previous epoch's sub-epoch summary carried by
// the record just above the previous surpass when the span reaches that deep (as the headers-first
// backfill stores it from the weight proof's summary chain).
fn chain(low: u32, high: u32) -> HashMap<Bytes32, BlockRecord> {
    let ses_height = PREV_SURPASS + 4; // first new-slot block of the previous epoch's window
    let mut blocks = HashMap::new();
    for h in low..=high {
        let ses = (h == ses_height).then(|| SubEpochSummary {
            prev_subepoch_summary_hash: Bytes32::default(),
            reward_chain_hash: Bytes32::default(),
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        });
        let b = record(h, ses);
        blocks.insert(b.header_hash, b);
    }
    blocks
}

// Anchor-span coverage only: the retarget walk at the trigger block
// must fail with the exact "block record not found" NotFound on the floor record's prev hash —
// the log line the pod repeated forever.
#[test]
fn retarget_at_the_boundary_falls_off_the_anchor_floor() {
    let blocks = chain(ANCHOR_FLOOR, PEAK);
    let prev_b = blocks.get(&h32(PEAK)).unwrap();
    let err = get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(prev_b), &blocks)
        .expect_err("the walk must fall off the record floor");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let msg = err.to_string();
    assert!(
        msg.contains("block record not found"),
        "unexpected error: {msg}"
    );
    // The precise failing lookup: the prev_hash of the floor record (height ANCHOR_FLOOR - 1),
    // matching the live Postgres evidence (floor 4,574,936; missing prev 4,574,935).
    assert!(
        msg.contains(&h32(ANCHOR_FLOOR - 1).to_string()),
        "the missing hash must be the floor record's prev: {msg}"
    );
}

// epoch_backfill_low coverage (the post-fix store contents): the identical walk completes.
#[test]
fn retarget_at_the_boundary_succeeds_with_epoch_backfill_coverage() {
    let low = epoch_backfill_low(PEAK, MAINNET.epoch_blocks, MAINNET.sub_epoch_blocks);
    assert_eq!(low, PREV_SURPASS - EPOCH_BACKFILL_SLACK); // 4,571,008
    assert!(low < ANCHOR_FLOOR, "the backfill must reach below the span");
    let blocks = chain(low, PEAK);
    let prev_b = blocks.get(&h32(PEAK)).unwrap();
    let (ssi, difficulty) =
        get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(prev_b), &blocks)
            .expect("the walk stays inside the backfilled records");
    assert!(ssi > 0);
    assert!(difficulty > 0);
}
