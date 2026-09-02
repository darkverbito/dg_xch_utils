// The general difficulty/sub-slot-iters retarget path driven over real mainnet block records (the
// same recent-chain slice the header-validation test uses). get_next_sub_slot_iters_and_difficulty must reproduce mainnet's actual
// difficulty (from real weight deltas) and carry sub_slot_iters forward within an epoch. The change math at
// a boundary is covered by the synthetic epoch-boundary tests in difficulty_adjustment.rs and the real
// retarget-value clamp/rounding cross-check in weight-proof/tests/difficulty_adjustment_mainnet.rs.

use std::collections::HashMap;
use std::io::Cursor;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::weight_proof::RecentChainData;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::deficit::calculate_deficit;
use dg_xch_core::consensus::difficulty_adjustment::get_next_sub_slot_iters_and_difficulty;
use dg_xch_core::consensus::full_block_to_block_record::header_block_to_sub_block_record;
use dg_xch_core::consensus::pot_iterations::is_overflow_block;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

// Real mainnet ssi/difficulty for the sliced region (heights 9054336..=9054620) — constant, no epoch
// boundary inside it (the slice starts on the sub-epoch boundary at 9054336 so the can_finish walk-back
// terminates in-window). Difficulty is the on-chain weight delta between adjacent blocks.
const SSI: u64 = 574_619_648;
const DIFF: u64 = 2608;

fn load_chain() -> Vec<HeaderBlock> {
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054336_9054620.bin");
    RecentChainData::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("recent chain slice deserializes")
        .recent_chain_data
}

fn placeholder_ses() -> SubEpochSummary {
    SubEpochSummary {
        prev_subepoch_summary_hash: Bytes32::default(),
        reward_chain_hash: Bytes32::default(),
        num_blocks_overflow: 0,
        new_difficulty: None,
        new_sub_slot_iters: None,
    }
}

// Fast record build (no proof-of-space/VDF): the retarget path reads only weight, total_iters, timestamp,
// sub_slot_iters, deficit, is_transaction_block, and sub_epoch_summary_included — required_iters is not read.
fn build_records(chain: &[HeaderBlock]) -> HashMap<Bytes32, BlockRecord> {
    let c = &MAINNET;
    let mut records = HashMap::new();
    let mut prev: Option<BlockRecord> = None;
    for block in chain {
        let overflow =
            is_overflow_block(c, block.reward_chain_block.signage_point_index).expect("overflow");
        let deficit = calculate_deficit(
            c,
            block.height(),
            prev.as_ref(),
            overflow,
            block.finished_sub_slots.len(),
        );
        let ses = block
            .finished_sub_slots
            .iter()
            .any(|ss| ss.challenge_chain.subepoch_summary_hash.is_some())
            .then(placeholder_ses);
        let rec = header_block_to_sub_block_record(
            c,
            1,
            block,
            SSI,
            overflow,
            deficit,
            block.height(),
            ses,
        )
        .expect("record");
        records.insert(rec.header_hash, rec.clone());
        prev = Some(rec);
    }
    records
}

#[test]
fn general_retarget_reproduces_mainnet_difficulty_and_ssi() {
    let chain = load_chain();
    let records = build_records(&chain);
    let by_height: HashMap<u32, &HeaderBlock> = chain.iter().map(|b| (b.height(), b)).collect();

    // Several real mid-epoch blocks: the general engine entry point must derive mainnet's on-chain difficulty
    // (the real weight delta) and carry sub_slot_iters forward unchanged (no spurious mid-epoch retarget).
    let mut checked = 0usize;
    for &height in &[9_054_610u32, 9_054_612, 9_054_615, 9_054_618] {
        let block = by_height.get(&height).expect("block in slice");
        let prev_rec = records
            .get(&block.header_hash().expect("hh"))
            .expect("record built");
        let is_first_in_sub_slot = !block.finished_sub_slots.is_empty();

        let (ssi, difficulty) = get_next_sub_slot_iters_and_difficulty(
            &MAINNET,
            is_first_in_sub_slot,
            Some(prev_rec),
            &records,
        )
        .unwrap_or_else(|e| panic!("retarget at {height}: {e}"));

        // Difficulty is derived, not seeded: it is prev_b.weight - prev_prev.weight over real mainnet weights.
        let prev_prev = records.get(&prev_rec.prev_hash).expect("prev_prev");
        let on_chain_delta = u64::try_from(prev_rec.weight - prev_prev.weight).unwrap();
        assert_eq!(on_chain_delta, DIFF, "real weight delta at {height}");
        assert_eq!(difficulty, DIFF, "general retarget difficulty at {height}");
        assert_eq!(
            ssi, SSI,
            "sub_slot_iters carried forward within the epoch at {height}"
        );
        checked += 1;
    }
    assert_eq!(checked, 4);
}
