// Epoch-boundary difficulty and sub-slot-iteration retargeting.

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::sized_bytes::Bytes32;
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::missing;
use std::collections::HashMap;
use std::io::{Error, ErrorKind};

// Truncate to significant bits.
#[must_use]
pub fn truncate_to_significant_bits(value: u128, num_significant_bits: u64) -> u128 {
    let bit_length = u64::from(128 - value.leading_zeros());
    if num_significant_bits >= bit_length {
        return value;
    }
    let lower = bit_length - num_significant_bits;
    (value >> lower) << lower
}

// Count significant bits.
#[must_use]
pub fn count_significant_bits(value: u128) -> u64 {
    if value == 0 {
        return 0;
    }
    u64::from(128 - value.leading_zeros() - value.trailing_zeros())
}

#[must_use]
pub fn height_can_be_first_in_epoch(constants: &ConsensusConstants, height: u32) -> bool {
    (height - (height % constants.sub_epoch_blocks)).is_multiple_of(constants.epoch_blocks)
}

// Returns (can_finish_sub_epoch, can_finish_full_epoch).
// General-node port: always takes the walk-back branch (prev_ses_block=None).
pub fn can_finish_sub_and_full_epoch(
    constants: &ConsensusConstants,
    blocks: &HashMap<Bytes32, BlockRecord>,
    height: u32,
    prev_header_hash: Bytes32,
    deficit: u8,
    block_at_height_included_ses: bool,
) -> Result<(bool, bool), Error> {
    if height < constants.sub_epoch_blocks - 1 {
        return Ok((false, false));
    }
    if deficit > 0 {
        return Ok((false, false));
    }
    if block_at_height_included_ses {
        return Ok((false, false));
    }
    if (height + 1) % constants.sub_epoch_blocks > 1 {
        let mut curr = blocks
            .get(&prev_header_hash)
            .ok_or_else(|| missing(prev_header_hash))?;
        while curr.height % constants.sub_epoch_blocks > 0 {
            if curr.sub_epoch_summary_included.is_some() {
                return Ok((false, false));
            }
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        if curr.sub_epoch_summary_included.is_some() {
            return Ok((false, false));
        }
    }
    Ok((true, height_can_be_first_in_epoch(constants, height + 1)))
}

pub fn get_second_to_last_transaction_block_in_previous_epoch<'a>(
    constants: &ConsensusConstants,
    blocks: &'a HashMap<Bytes32, BlockRecord>,
    last_b: &'a BlockRecord,
) -> Result<&'a BlockRecord, Error> {
    let height_in_next_epoch = last_b.height
        + 2 * constants.max_sub_slot_blocks
        + u32::from(constants.min_blocks_per_challenge_block)
        + 5;
    let height_epoch_surpass =
        height_in_next_epoch - (height_in_next_epoch % constants.epoch_blocks);
    let height_prev_epoch_surpass = height_epoch_surpass - constants.epoch_blocks;

    if height_in_next_epoch - height_epoch_surpass >= 5 * constants.max_sub_slot_blocks {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "height_in_next_epoch too far past the epoch boundary",
        ));
    }

    if height_prev_epoch_surpass == 0 {
        let mut curr = last_b;
        while curr.height > 0 {
            curr = blocks
                .get(&curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        return Ok(curr);
    }

    let mut by_height: HashMap<u32, &BlockRecord> = HashMap::new();
    let mut curr = last_b;
    loop {
        by_height.insert(curr.height, curr);
        if curr.height < height_prev_epoch_surpass {
            break;
        }
        curr = blocks
            .get(&curr.prev_hash)
            .ok_or_else(|| missing(curr.prev_hash))?;
    }

    let mut next_height = height_prev_epoch_surpass;
    loop {
        let next_b = by_height.get(&next_height).copied().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous-epoch sub-epoch-summary block not found in ancestor window",
            )
        })?;
        if next_b.sub_epoch_summary_included.is_some() {
            break;
        }
        next_height += 1;
    }
    let mut curr_b = *by_height.get(&(next_height - 1)).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            "block before previous-epoch sub-epoch-summary block missing",
        )
    })?;

    let mut found_tx_block = u8::from(curr_b.is_transaction_block());
    while found_tx_block < 2 {
        curr_b = blocks
            .get(&curr_b.prev_hash)
            .ok_or_else(|| missing(curr_b.prev_hash))?;
        if curr_b.is_transaction_block() {
            found_tx_block += 1;
        }
    }
    Ok(curr_b)
}

#[allow(clippy::too_many_arguments)]
pub fn get_next_difficulty(
    constants: &ConsensusConstants,
    blocks: &HashMap<Bytes32, BlockRecord>,
    prev_header_hash: Bytes32,
    height: u32,
    current_difficulty: u64,
    deficit: u8,
    block_at_height_included_ses: bool,
    new_slot: bool,
    signage_point_total_iters: u128,
) -> Result<u64, Error> {
    let next_height = height + 1;

    if next_height < constants.epoch_blocks - 3 * constants.max_sub_slot_blocks {
        return Ok(constants.difficulty_starting);
    }

    let prev_b = blocks
        .get(&prev_header_hash)
        .ok_or_else(|| missing(prev_header_hash))?;

    let (_, can_finish_epoch) = can_finish_sub_and_full_epoch(
        constants,
        blocks,
        height,
        prev_header_hash,
        deficit,
        block_at_height_included_ses,
    )?;
    if !new_slot || !can_finish_epoch {
        return Ok(current_difficulty);
    }

    let last_block_prev =
        get_second_to_last_transaction_block_in_previous_epoch(constants, blocks, prev_b)?;

    let mut last_block_curr = prev_b;
    while last_block_curr.total_iters > signage_point_total_iters
        || !last_block_curr.is_transaction_block()
    {
        last_block_curr = blocks
            .get(&last_block_curr.prev_hash)
            .ok_or_else(|| missing(last_block_curr.prev_hash))?;
    }

    let curr_ts = last_block_curr
        .timestamp
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "last_block_curr has no timestamp"))?;
    let prev_ts = last_block_prev
        .timestamp
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "last_block_prev has no timestamp"))?;
    let actual_epoch_time = curr_ts
        .checked_sub(prev_ts)
        .filter(|t| *t > 0)
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "non-positive actual epoch time"))?;

    let prev_prev = blocks
        .get(&prev_b.prev_hash)
        .ok_or_else(|| missing(prev_b.prev_hash))?;
    let old_difficulty = u64::try_from(prev_b.weight - prev_prev.weight)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "old difficulty exceeds u64"))?;

    let numerator = (last_block_curr.weight - last_block_prev.weight)
        * u128::from(constants.sub_slot_time_target);
    let denominator = u128::from(constants.slot_blocks_target) * u128::from(actual_epoch_time);
    let mut new_difficulty_precise = numerator / denominator;

    let max_diff = u128::from(constants.difficulty_change_max_factor) * u128::from(old_difficulty);
    let min_diff = u128::from(old_difficulty) / u128::from(constants.difficulty_change_max_factor);
    if new_difficulty_precise >= u128::from(old_difficulty) {
        new_difficulty_precise = new_difficulty_precise.min(max_diff);
    } else {
        new_difficulty_precise = new_difficulty_precise.max(1).max(min_diff);
    }

    let new_difficulty =
        truncate_to_significant_bits(new_difficulty_precise, constants.significant_bits);
    u64::try_from(new_difficulty)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "new difficulty exceeds u64"))
}

#[allow(clippy::too_many_arguments)]
pub fn get_next_sub_slot_iters(
    constants: &ConsensusConstants,
    blocks: &HashMap<Bytes32, BlockRecord>,
    prev_header_hash: Bytes32,
    height: u32,
    curr_sub_slot_iters: u64,
    deficit: u8,
    block_at_height_included_ses: bool,
    new_slot: bool,
    signage_point_total_iters: u128,
) -> Result<u64, Error> {
    let next_height = height + 1;

    if next_height < constants.epoch_blocks {
        return Ok(constants.sub_slot_iters_starting);
    }

    blocks
        .get(&prev_header_hash)
        .ok_or_else(|| missing(prev_header_hash))?;

    let (_, can_finish_epoch) = can_finish_sub_and_full_epoch(
        constants,
        blocks,
        height,
        prev_header_hash,
        deficit,
        block_at_height_included_ses,
    )?;
    if !new_slot || !can_finish_epoch {
        return Ok(curr_sub_slot_iters);
    }

    let prev_b = blocks
        .get(&prev_header_hash)
        .ok_or_else(|| missing(prev_header_hash))?;
    let last_block_prev =
        get_second_to_last_transaction_block_in_previous_epoch(constants, blocks, prev_b)?;

    let mut last_block_curr = prev_b;
    while last_block_curr.total_iters > signage_point_total_iters
        || !last_block_curr.is_transaction_block()
    {
        last_block_curr = blocks
            .get(&last_block_curr.prev_hash)
            .ok_or_else(|| missing(last_block_curr.prev_hash))?;
    }

    let curr_ts = last_block_curr
        .timestamp
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "last_block_curr has no timestamp"))?;
    let prev_ts = last_block_prev
        .timestamp
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "last_block_prev has no timestamp"))?;
    let block_time = curr_ts
        .checked_sub(prev_ts)
        .filter(|t| *t > 0)
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "non-positive block time"))?;

    let numerator = u128::from(constants.sub_slot_time_target)
        * (last_block_curr.total_iters - last_block_prev.total_iters);
    let mut new_ssi_precise = numerator / u128::from(block_time);

    let curr_ssi = u128::from(last_block_curr.sub_slot_iters);
    let max_ssi = u128::from(constants.difficulty_change_max_factor) * curr_ssi;
    let min_ssi = curr_ssi / u128::from(constants.difficulty_change_max_factor);
    if new_ssi_precise >= curr_ssi {
        new_ssi_precise = new_ssi_precise.min(max_ssi);
    } else {
        new_ssi_precise = new_ssi_precise
            .max(u128::from(constants.num_sps_sub_slot))
            .max(min_ssi);
    }

    let truncated = truncate_to_significant_bits(new_ssi_precise, constants.significant_bits);
    let new_ssi = truncated - (truncated % u128::from(constants.num_sps_sub_slot));
    u64::try_from(new_ssi)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "new sub_slot_iters exceeds u64"))
}

// Full-node entry point; returns (sub_slot_iters, difficulty).
pub fn get_next_sub_slot_iters_and_difficulty(
    constants: &ConsensusConstants,
    is_first_in_sub_slot: bool,
    prev_b: Option<&BlockRecord>,
    blocks: &HashMap<Bytes32, BlockRecord>,
) -> Result<(u64, u64), Error> {
    let Some(prev_b) = prev_b else {
        return Ok((
            constants.sub_slot_iters_starting,
            constants.difficulty_starting,
        ));
    };

    let prev_difficulty = if prev_b.height != 0 {
        let prev_prev = blocks
            .get(&prev_b.prev_hash)
            .ok_or_else(|| missing(prev_b.prev_hash))?;
        u64::try_from(prev_b.weight - prev_prev.weight)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "previous difficulty exceeds u64"))?
    } else {
        u64::try_from(prev_b.weight)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "genesis weight exceeds u64"))?
    };

    if prev_b.sub_epoch_summary_included.is_some() {
        return Ok((prev_b.sub_slot_iters, prev_difficulty));
    }

    let sp_total_iters = prev_b.sp_total_iters(constants)?;

    let difficulty = get_next_difficulty(
        constants,
        blocks,
        prev_b.prev_hash,
        prev_b.height,
        prev_difficulty,
        prev_b.deficit,
        false,
        is_first_in_sub_slot,
        sp_total_iters,
    )?;

    let sub_slot_iters = get_next_sub_slot_iters(
        constants,
        blocks,
        prev_b.prev_hash,
        prev_b.height,
        prev_b.sub_slot_iters,
        prev_b.deficit,
        false,
        is_first_in_sub_slot,
        sp_total_iters,
    )?;

    Ok((sub_slot_iters, difficulty))
}

// How many ancestor records (inclusive of the anchor itself) the get_next_sub_slot_iters_and_
// difficulty walks can touch below a block at `prev_height`. A caller that materializes a
// bounded ancestor window must size it to the deepest walk for THIS anchor:
//   - can_finish_sub_and_full_epoch scans from the anchor down to the last sub-epoch boundary
//     (height % SUB_EPOCH_BLOCKS == 0) — up to SUB_EPOCH_BLOCKS - 1 records back;
//   - at an epoch turn (height_can_be_first_in_epoch(height + 1)) the retarget additionally
//     reaches the fetch floor just below height_prev_epoch_surpass
//     (height_prev_epoch_surpass - MAX_SUB_SLOT_BLOCKS - 1) — and clear down to genesis while
//     height_prev_epoch_surpass == 0;
//   - the residual scans (prev_prev, last_block_curr, the two-transaction-block walk) stay
//     within 4 * MAX_SUB_SLOT_BLOCKS of slack, mirrored here below the deepest anchor.
// The bounded record-window capacity that serves EVERY consensus walk without a miss,
// sized from the constants instead of a flat literal:
//   - deepest per-anchor `difficulty_record_depth` (epoch-turn regime, anchor offset
//     SUB_EPOCH_BLOCKS - 2 mod EPOCH_BLOCKS): EPOCH_BLOCKS + (SUB_EPOCH_BLOCKS - 2)
//     + 4 * MAX_SUB_SLOT_BLOCKS + 1 = 5,503 on mainnet;
//   - plus 2 * MAX_SUB_SLOT_BLOCKS slack for walk anchors that trail the newest cached head
//     (an unfinished block's parent; a stage window's base after a restart warm).
// Mainnet: 4608 + 384 + 6*128 = 5,760 records ≈ 6 MiB. A caller whose walks read ONLY the
// window must size it to the worst depth WITH margin, or the first-sub-epoch retarget walk
// falls off the edge (worst-case lookback 5,503).
#[must_use]
pub fn consensus_walk_window(constants: &ConsensusConstants) -> usize {
    (constants.epoch_blocks + constants.sub_epoch_blocks + 6 * constants.max_sub_slot_blocks)
        as usize
}

#[must_use]
pub fn difficulty_record_depth(constants: &ConsensusConstants, prev_height: u32) -> u32 {
    // The can_finish_sub_and_full_epoch walk floor: the last sub-epoch boundary at or below prev.
    let mut lowest = prev_height - (prev_height % constants.sub_epoch_blocks);
    // The epoch retarget only runs when the NEXT height can start an epoch — the same gate
    // can_finish_sub_and_full_epoch applies before get_next_difficulty takes the deep walk.
    if height_can_be_first_in_epoch(constants, prev_height.saturating_add(1)) {
        let height_in_next_epoch = prev_height
            + 2 * constants.max_sub_slot_blocks
            + u32::from(constants.min_blocks_per_challenge_block)
            + 5;
        let height_epoch_surpass =
            height_in_next_epoch - (height_in_next_epoch % constants.epoch_blocks);
        lowest = lowest.min(height_epoch_surpass.saturating_sub(constants.epoch_blocks));
    }
    let lowest = lowest.saturating_sub(4 * constants.max_sub_slot_blocks);
    (prev_height - lowest).saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockchain::unsized_bytes::UnsizedBytes;
    use crate::blockchain::vdf_output::VdfOutput;
    use crate::consensus::constants::MAINNET;
    use std::collections::HashMap;

    fn h32(n: u32) -> Bytes32 {
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(&n.to_be_bytes());
        Bytes32::from(b)
    }

    fn empty_vdf() -> VdfOutput {
        VdfOutput {
            data: UnsizedBytes::new(vec![]),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn block(
        height: u32,
        weight: u128,
        total_iters: u128,
        timestamp: Option<u64>,
        sub_slot_iters: u64,
        ses: Option<crate::blockchain::sub_epoch_summary::SubEpochSummary>,
    ) -> BlockRecord {
        BlockRecord {
            header_hash: h32(height),
            prev_hash: h32(height.wrapping_sub(1)),
            height,
            weight,
            total_iters,
            signage_point_index: 0,
            challenge_vdf_output: empty_vdf(),
            infused_challenge_vdf_output: None,
            reward_infusion_new_challenge: Bytes32::default(),
            challenge_block_info_hash: Bytes32::default(),
            sub_slot_iters,
            pool_puzzle_hash: Bytes32::default(),
            farmer_puzzle_hash: Bytes32::default(),
            required_iters: 1,
            deficit: 0,
            overflow: false,
            prev_transaction_block_height: height.wrapping_sub(1),
            timestamp,
            prev_transaction_block_hash: None,
            fees: None,
            reward_claims_incorporated: None,
            finished_challenge_slot_hashes: None,
            finished_infused_challenge_slot_hashes: None,
            finished_reward_slot_hashes: None,
            sub_epoch_summary_included: ses,
        }
    }

    #[test]
    fn truncate_and_count_match_chia_docstrings() {
        assert_eq!(truncate_to_significant_bits(0b0_1111_0101, 2), 0b1100_0000);
        assert_eq!(count_significant_bits(0b0001_1001_0000), 5);
        assert_eq!(count_significant_bits(0), 0);
        assert_eq!(truncate_to_significant_bits(0b101, 8), 0b101);
    }

    #[test]
    fn height_can_be_first_in_epoch_marks_epoch_starts() {
        assert!(height_can_be_first_in_epoch(&MAINNET, 0));
        assert!(height_can_be_first_in_epoch(&MAINNET, 4608));
        assert!(height_can_be_first_in_epoch(&MAINNET, 9216));
        assert!(!height_can_be_first_in_epoch(&MAINNET, 384));
        assert!(!height_can_be_first_in_epoch(&MAINNET, 4600));
    }

    #[test]
    fn genesis_returns_starting_values() {
        let blocks: HashMap<Bytes32, BlockRecord> = HashMap::new();
        let (ssi, diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, false, None, &blocks).unwrap();
        assert_eq!(ssi, MAINNET.sub_slot_iters_starting);
        assert_eq!(ssi, 2u64.pow(27));
        assert_eq!(diff, MAINNET.difficulty_starting);
        assert_eq!(diff, 7);
    }

    #[test]
    fn sub_epoch_summary_included_carries_values_forward() {
        let ssi = MAINNET.sub_slot_iters_starting;
        let parent = block(99, 7 * 99, 99 * 10, Some(1_000), ssi, None);
        let ses = crate::blockchain::sub_epoch_summary::SubEpochSummary {
            prev_subepoch_summary_hash: Bytes32::default(),
            reward_chain_hash: Bytes32::default(),
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        };
        let prev = block(100, 7 * 100, 100 * 10, Some(1_020), ssi, Some(ses));
        let mut blocks: HashMap<Bytes32, BlockRecord> = HashMap::new();
        blocks.insert(parent.header_hash, parent);
        let prev_ref = prev.clone();
        blocks.insert(prev.header_hash, prev);

        let (out_ssi, out_diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev_ref), &blocks)
                .unwrap();
        assert_eq!(out_ssi, ssi);
        assert_eq!(out_diff, 7);
    }

    fn linear_chain(
        top: u32,
        w_delta: u128,
        iter_delta: u128,
        t0: u64,
        t_delta: u64,
        ssi: u64,
    ) -> HashMap<Bytes32, BlockRecord> {
        let mut blocks = HashMap::new();
        for h in 0..=top {
            let b = block(
                h,
                w_delta * u128::from(h),
                iter_delta * u128::from(h),
                Some(t0 + t_delta * u64::from(h)),
                ssi,
                None,
            );
            blocks.insert(b.header_hash, b);
        }
        blocks
    }

    #[test]
    fn get_next_difficulty_and_ssi_clamp_at_epoch_boundary() {
        let ssi_start = MAINNET.sub_slot_iters_starting;
        let iter_delta: u128 = 10_000_000;
        let blocks = linear_chain(4607, 7, iter_delta, 1_000, 1, ssi_start);

        let sp_total_iters: u128 = iter_delta * 4606;

        let diff = get_next_difficulty(
            &MAINNET,
            &blocks,
            h32(4606),
            4607,
            7,
            0,
            false,
            true,
            sp_total_iters,
        )
        .unwrap();
        assert_eq!(diff, 21);

        let next_ssi = get_next_sub_slot_iters(
            &MAINNET,
            &blocks,
            h32(4606),
            4607,
            ssi_start,
            0,
            false,
            true,
            sp_total_iters,
        )
        .unwrap();
        assert_eq!(next_ssi, 402_653_184);
        assert_eq!(next_ssi, 3 * ssi_start);
        assert_eq!(next_ssi % u64::from(MAINNET.num_sps_sub_slot), 0);
        assert!(count_significant_bits(u128::from(next_ssi)) <= MAINNET.significant_bits);
    }

    #[test]
    fn get_next_sub_slot_iters_and_difficulty_end_to_end_at_boundary() {
        let ssi_start = MAINNET.sub_slot_iters_starting;
        let iter_delta: u128 = 10_000_000;
        let blocks = linear_chain(4607, 7, iter_delta, 1_000, 1, ssi_start);
        let prev_b = blocks.get(&h32(4607)).unwrap().clone();

        let sp = prev_b.sp_total_iters(&MAINNET).unwrap();
        assert!(iter_delta * 4606 <= sp && sp < iter_delta * 4607);

        let (out_ssi, out_diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev_b), &blocks).unwrap();
        assert_eq!(out_diff, 21);
        assert_eq!(out_ssi, 402_653_184);
    }

    #[test]
    fn mid_epoch_passes_difficulty_and_ssi_through() {
        let ssi_start = MAINNET.sub_slot_iters_starting;
        let mut blocks = linear_chain(5000, 7, 10_000_000, 1_000, 1, ssi_start);
        let ses = crate::blockchain::sub_epoch_summary::SubEpochSummary {
            prev_subepoch_summary_hash: Bytes32::default(),
            reward_chain_hash: Bytes32::default(),
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        };
        let ses_block = block(
            4608,
            7 * 4608,
            4608 * 10_000_000,
            Some(1_000 + 4608),
            ssi_start,
            Some(ses),
        );
        blocks.insert(ses_block.header_hash, ses_block);

        let sp_total_iters: u128 = 10_000_000 * 4699;
        let diff = get_next_difficulty(
            &MAINNET,
            &blocks,
            h32(4699),
            4700,
            13,
            0,
            false,
            true,
            sp_total_iters,
        )
        .unwrap();
        assert_eq!(diff, 13);
        let next_ssi = get_next_sub_slot_iters(
            &MAINNET,
            &blocks,
            h32(4699),
            4700,
            ssi_start,
            0,
            false,
            true,
            sp_total_iters,
        )
        .unwrap();
        assert_eq!(next_ssi, ssi_start);
    }

    // Chain slice holding exactly `depth` records: heights [top-depth+1 ..= top]. Every record is a
    // transaction block (timestamps set), no SES included — the can_finish walk's worst case, since
    // a SES between the sub-epoch boundary and the anchor would exit the walk early.
    fn chain_window(top: u32, depth: u32, ssi: u64) -> HashMap<Bytes32, BlockRecord> {
        let bottom = top.saturating_sub(depth.saturating_sub(1));
        let mut blocks = HashMap::new();
        for h in bottom..=top {
            let b = block(
                h,
                7 * u128::from(h),
                10_000_000 * u128::from(h),
                Some(1_000 + u64::from(h)),
                ssi,
                None,
            );
            blocks.insert(b.header_hash, b);
        }
        blocks
    }

    // The walk-window capacity is proven against the real depth function across every anchor
    // offset of two full epochs — with the trailing-anchor slack to spare. This is the engine
    // walk cache's no-miss guarantee (a flat 5,120 window loses to the 5,503-deep first-sub-epoch
    // retarget anchors).
    #[test]
    fn consensus_walk_window_covers_every_walk_depth_with_slack() {
        let cap = consensus_walk_window(&MAINNET);
        let slack = (2 * MAINNET.max_sub_slot_blocks) as usize;
        let base = 2000 * MAINNET.epoch_blocks;
        let mut max_depth = 0usize;
        for h in base..base + 2 * MAINNET.epoch_blocks {
            let depth = difficulty_record_depth(&MAINNET, h) as usize;
            max_depth = max_depth.max(depth);
            assert!(
                depth + slack <= cap,
                "offset {}: depth {depth} + slack {slack} exceeds capacity {cap}",
                h % MAINNET.epoch_blocks
            );
        }
        assert_eq!(
            max_depth, 5_503,
            "the epoch-turn regime is the deepest walk"
        );
    }

    #[test]
    fn difficulty_record_depth_matches_walk_floors() {
        // Mid-epoch: floor = last sub-epoch boundary minus the 4*MAX_SUB_SLOT_BLOCKS slack.
        assert_eq!(difficulty_record_depth(&MAINNET, 9_161_852), 380 + 512 + 1);
        // Exactly on a (non-epoch) sub-epoch boundary: minimal window.
        assert_eq!(difficulty_record_depth(&MAINNET, 9_161_472), 513);
        // Epoch turn: reaches the previous epoch surpass.
        assert_eq!(difficulty_record_depth(&MAINNET, 9_215), 5_120);
        // First epoch turn: height_prev_epoch_surpass == 0 walks clear to genesis.
        assert_eq!(difficulty_record_depth(&MAINNET, 4_607), 4_608);
    }

    // A fixed 256-record window is overrun by the can_finish_sub_and_full_epoch walk to
    // the sub-epoch boundary for anchors at peak % 384 in [259, 383].
    #[test]
    fn old_256_window_drops_late_sub_epoch_blocks_and_computed_depth_does_not() {
        let ssi = MAINNET.sub_slot_iters_starting;
        let prev_height = 9_161_852; // % 384 == 380, inside the observed failure band
        let depth = difficulty_record_depth(&MAINNET, prev_height);
        let blocks = chain_window(prev_height, depth, ssi);
        let prev = blocks.get(&h32(prev_height)).unwrap().clone();
        get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &blocks)
            .expect("computed depth must cover the sub-epoch walk");

        let shallow = chain_window(prev_height, 256, ssi);
        assert!(
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &shallow).is_err(),
            "the old fixed 256 window must reproduce the at-tip failure"
        );
    }

    // The at-tip guarantee: for EVERY offset within a sub-epoch, a window of exactly
    // difficulty_record_depth records suffices — the computation cannot miss a record whenever
    // the parent and its window ancestors are in the store. Offsets 256..=382 are precisely
    // the ones a fixed 256-record window cannot serve.
    #[test]
    fn computed_depth_suffices_for_every_sub_epoch_offset() {
        let ssi = MAINNET.sub_slot_iters_starting;
        let sub_epoch_start = 9_161_472; // % 384 == 0, % 4608 != 0: no epoch turn in range
        for offset in 0..MAINNET.sub_epoch_blocks {
            let prev_height = sub_epoch_start + offset;
            let depth = difficulty_record_depth(&MAINNET, prev_height);
            let blocks = chain_window(prev_height, depth, ssi);
            let prev = blocks.get(&h32(prev_height)).unwrap().clone();
            let ok = get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &blocks);
            assert!(ok.is_ok(), "offset {offset}: computed depth insufficient");

            let shallow = chain_window(prev_height, 256, ssi);
            let shallow_result =
                get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &shallow);
            if (256..=382).contains(&offset) {
                assert!(
                    shallow_result.is_err(),
                    "offset {offset}: expected old failure"
                );
            } else {
                assert!(
                    shallow_result.is_ok(),
                    "offset {offset}: old window regressed"
                );
            }
        }
    }

    // First epoch turn: the retarget walks to genesis (height_prev_epoch_surpass == 0), so the
    // computed depth spans the full chain and reproduces the established boundary values; a
    // fixed 512-record window must fail here.
    #[test]
    fn computed_depth_covers_the_genesis_epoch_turn() {
        let ssi_start = MAINNET.sub_slot_iters_starting;
        let prev_height = 4_607;
        let depth = difficulty_record_depth(&MAINNET, prev_height);
        assert_eq!(depth, 4_608);
        let blocks = chain_window(prev_height, depth, ssi_start);
        let prev = blocks.get(&h32(prev_height)).unwrap().clone();
        let (out_ssi, out_diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &blocks).unwrap();
        assert_eq!(out_diff, 21);
        assert_eq!(out_ssi, 402_653_184);

        let shallow = chain_window(prev_height, 512, ssi_start);
        assert!(
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &shallow).is_err()
        );
    }

    // Steady-state epoch turn (second epoch): the retarget reaches the previous epoch surpass at
    // 4608 and the two-transaction-block scan below it; depth == 5120 and the window computes
    // cleanly where fixed 256 and 512 windows fail.
    #[test]
    fn computed_depth_covers_a_steady_state_epoch_turn() {
        let ssi = MAINNET.sub_slot_iters_starting;
        let prev_height = 9_215;
        let depth = difficulty_record_depth(&MAINNET, prev_height);
        assert_eq!(depth, 5_120);
        let mut blocks = chain_window(prev_height, depth, ssi);
        // The previous epoch's sub-epoch summary, a few blocks past the surpass point — the
        // upward scan in get_second_to_last_transaction_block_in_previous_epoch anchors on it.
        let ses = crate::blockchain::sub_epoch_summary::SubEpochSummary {
            prev_subepoch_summary_hash: Bytes32::default(),
            reward_chain_hash: Bytes32::default(),
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        };
        let ses_block = block(
            4_610,
            7 * 4_610,
            10_000_000 * 4_610,
            Some(1_000 + 4_610),
            ssi,
            Some(ses),
        );
        blocks.insert(ses_block.header_hash, ses_block);
        let prev = blocks.get(&h32(prev_height)).unwrap().clone();
        let (out_ssi, out_diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &blocks).unwrap();
        assert!(out_diff > 0);
        assert!(out_ssi >= u64::from(MAINNET.num_sps_sub_slot));

        for shallow_depth in [256u32, 512] {
            let shallow = chain_window(prev_height, shallow_depth, ssi);
            assert!(
                get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev), &shallow)
                    .is_err(),
                "window of {shallow_depth} must not cover an epoch turn"
            );
        }
    }
}
