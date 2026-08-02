//! Epoch-boundary difficulty and sub-slot-iteration retargeting.
//!
//! A full node validating blocks across an epoch boundary must be able to derive, from the chain that
//! precedes a block, the `difficulty` and `sub_slot_iters` that block is required to use. Chia retargets
//! both parameters once per epoch (`EPOCH_BLOCKS` blocks) so that a sub-slot lasts `SUB_SLOT_TIME_TARGET`
//! seconds and a challenge yields `SLOT_BLOCKS_TARGET` blocks on average.
//!
//! This module ports the rules from chia's `chia/consensus/difficulty_adjustment.py`
//! (`get_next_sub_slot_iters_and_difficulty`, `_get_next_difficulty`, `_get_next_sub_slot_iters`,
//! `can_finish_sub_and_full_epoch`, `height_can_be_first_in_epoch`,
//! `_get_second_to_last_transaction_block_in_previous_epoch`). The block-record iteration accessors it
//! relies on (`sp_total_iters`, ...) mirror chia_rs `crates/chia-protocol/src/block_record.rs`. There is
//! no chia_rs port of the retarget functions themselves, so the Python module is the rule source.
//!
//! Everything here is a pure function of previously-validated [`BlockRecord`]s, their timestamps and
//! weights, and the [`ConsensusConstants`]. It has no dependency on a coin store or block-validation
//! engine, which is why it stands alone.

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::sized_bytes::Bytes32;
use crate::consensus::constants::ConsensusConstants;
use std::io::{Error, ErrorKind};

/// Read-only access to previously-validated [`BlockRecord`]s, keyed by header hash.
///
/// Mirrors the subset of chia's `BlockRecordsProtocol` that the retarget rules need: every lookup here
/// resolves an ancestor of the block whose next difficulty is being computed, reached by following
/// `prev_hash` links. A full node backs this with its block store; tests back it with a map.
pub trait BlockRecordProvider {
    /// Returns the record for `header_hash`, or `None` when this provider has never seen it.
    fn block_record(&self, header_hash: Bytes32) -> Option<&BlockRecord>;
}

impl BlockRecordProvider for std::collections::HashMap<Bytes32, BlockRecord> {
    fn block_record(&self, header_hash: Bytes32) -> Option<&BlockRecord> {
        self.get(&header_hash)
    }
}

impl<T: BlockRecordProvider + ?Sized> BlockRecordProvider for &T {
    fn block_record(&self, header_hash: Bytes32) -> Option<&BlockRecord> {
        (*self).block_record(header_hash)
    }
}

fn missing(hash: Bytes32) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("block record not found: {hash}"),
    )
}

/// chia `truncate_to_significant_bits` (ref `chia/util/significant_bits.py`). Zeroes every bit below the
/// top `num_significant_bits` set bits, so difficulty/ssi carry only `SIGNIFICANT_BITS` of precision.
#[must_use]
pub fn truncate_to_significant_bits(value: u128, num_significant_bits: u64) -> u128 {
    let bit_length = u64::from(128 - value.leading_zeros());
    if num_significant_bits >= bit_length {
        return value;
    }
    let lower = bit_length - num_significant_bits;
    (value >> lower) << lower
}

/// chia `count_significant_bits` (ref `chia/util/significant_bits.py`). The number of bits between the
/// most- and least-significant set bit, inclusive; `0` for zero.
#[must_use]
pub fn count_significant_bits(value: u128) -> u64 {
    if value == 0 {
        return 0;
    }
    u64::from(128 - value.leading_zeros() - value.trailing_zeros())
}

/// chia `height_can_be_first_in_epoch` (ref `difficulty_adjustment.py`). True when a block at `height`
/// could be the first block of an epoch.
#[must_use]
pub fn height_can_be_first_in_epoch(constants: &ConsensusConstants, height: u32) -> bool {
    (height - (height % constants.sub_epoch_blocks)).is_multiple_of(constants.epoch_blocks)
}

/// chia `can_finish_sub_and_full_epoch` (ref `difficulty_adjustment.py`). Given the block at `height`
/// (with `prev_header_hash`, `deficit`, and whether it itself included a sub-epoch summary), decides
/// whether the *next* block can finish a sub-epoch and/or a full epoch. Returns
/// `(can_finish_sub_epoch, can_finish_full_epoch)`.
///
/// This is the general-node port: chia's optional `prev_ses_block` fast path is not taken, so the
/// walk-back branch is always used (matching the `prev_ses_block=None` call sites).
///
/// # Errors
/// Returns an error if the ancestor walk references a header hash `blocks` does not contain.
pub fn can_finish_sub_and_full_epoch(
    constants: &ConsensusConstants,
    blocks: &impl BlockRecordProvider,
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
            .block_record(prev_header_hash)
            .ok_or_else(|| missing(prev_header_hash))?;
        while curr.height % constants.sub_epoch_blocks > 0 {
            if curr.sub_epoch_summary_included.is_some() {
                return Ok((false, false));
            }
            curr = blocks
                .block_record(curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        if curr.sub_epoch_summary_included.is_some() {
            return Ok((false, false));
        }
    }
    Ok((true, height_can_be_first_in_epoch(constants, height + 1)))
}

/// chia `_get_second_to_last_transaction_block_in_previous_epoch` (ref `difficulty_adjustment.py`).
/// Finds the block whose weight/timestamp anchors the *previous* epoch when retargeting at the boundary
/// after `last_b`: the second-to-last transaction block before the previous epoch's sub-epoch-summary
/// block.
///
/// # Errors
/// Returns an error if the ancestor walk references a header hash `blocks` does not contain, or if the
/// walk cannot reach the required heights.
pub fn get_second_to_last_transaction_block_in_previous_epoch<'a, P: BlockRecordProvider>(
    constants: &ConsensusConstants,
    blocks: &'a P,
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

    // Collect the ancestor chain of `last_b` down to just below the previous epoch surpass, indexed by
    // height. chia's `_get_blocks_at_height` fetches this same window; the retarget then indexes into it.
    if height_prev_epoch_surpass == 0 {
        // The previous "epoch" is genesis: return the block at height 0.
        let mut curr = last_b;
        while curr.height > 0 {
            curr = blocks
                .block_record(curr.prev_hash)
                .ok_or_else(|| missing(curr.prev_hash))?;
        }
        return Ok(curr);
    }

    // Walk down from `last_b`, keeping every ancestor at or above `height_prev_epoch_surpass - 1`.
    let mut by_height: std::collections::HashMap<u32, &BlockRecord> =
        std::collections::HashMap::new();
    let mut curr = last_b;
    loop {
        by_height.insert(curr.height, curr);
        if curr.height < height_prev_epoch_surpass {
            break;
        }
        curr = blocks
            .block_record(curr.prev_hash)
            .ok_or_else(|| missing(curr.prev_hash))?;
    }

    // chia starts `next_b` at `height_prev_epoch_surpass` and advances until it finds the block that
    // included a sub-epoch summary; `curr_b` trails one height behind it.
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

    // Walk back to the second transaction block, inclusive of `curr_b` itself.
    let mut found_tx_block = u8::from(curr_b.is_transaction_block());
    while found_tx_block < 2 {
        curr_b = blocks
            .block_record(curr_b.prev_hash)
            .ok_or_else(|| missing(curr_b.prev_hash))?;
        if curr_b.is_transaction_block() {
            found_tx_block += 1;
        }
    }
    Ok(curr_b)
}

/// chia `_get_next_difficulty` (ref `difficulty_adjustment.py`). Computes the difficulty the block after
/// `prev_header_hash`/`height` must use. Off an epoch boundary this is `current_difficulty` unchanged; at
/// a boundary it is the epoch-time-adjusted, clamped, significant-bit-truncated new difficulty.
///
/// `signage_point_total_iters` is the previous block's `sp_total_iters`; `new_slot` is whether the block
/// being computed is the first in a sub-slot.
///
/// # Errors
/// Returns an error if a referenced ancestor is missing, a required timestamp is absent, the epoch time
/// is zero, or an intermediate value does not fit its integer type.
#[allow(clippy::too_many_arguments)]
pub fn get_next_difficulty(
    constants: &ConsensusConstants,
    blocks: &impl BlockRecordProvider,
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
        // We are in the first epoch.
        return Ok(constants.difficulty_starting);
    }

    let prev_b = blocks
        .block_record(prev_header_hash)
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
        // Not at the start of an epoch: difficulty is unchanged.
        return Ok(current_difficulty);
    }

    let last_block_prev =
        get_second_to_last_transaction_block_in_previous_epoch(constants, blocks, prev_b)?;

    let mut last_block_curr = prev_b;
    while last_block_curr.total_iters > signage_point_total_iters
        || !last_block_curr.is_transaction_block()
    {
        last_block_curr = blocks
            .block_record(last_block_curr.prev_hash)
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
        .block_record(prev_b.prev_hash)
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

/// chia `_get_next_sub_slot_iters` (ref `difficulty_adjustment.py`). Computes the `sub_slot_iters` the
/// block after `prev_header_hash`/`height` must use. Off an epoch boundary this is `curr_sub_slot_iters`
/// unchanged; at a boundary it is the time-adjusted, clamped, significant-bit-truncated new value,
/// rounded down to a multiple of `NUM_SPS_SUB_SLOT`.
///
/// # Errors
/// Returns an error if a referenced ancestor is missing, a required timestamp is absent, the block time
/// is zero, or an intermediate value does not fit its integer type.
#[allow(clippy::too_many_arguments)]
pub fn get_next_sub_slot_iters(
    constants: &ConsensusConstants,
    blocks: &impl BlockRecordProvider,
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
        // We are in the first epoch.
        return Ok(constants.sub_slot_iters_starting);
    }

    blocks
        .block_record(prev_header_hash)
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
        .block_record(prev_header_hash)
        .ok_or_else(|| missing(prev_header_hash))?;
    let last_block_prev =
        get_second_to_last_transaction_block_in_previous_epoch(constants, blocks, prev_b)?;

    let mut last_block_curr = prev_b;
    while last_block_curr.total_iters > signage_point_total_iters
        || !last_block_curr.is_transaction_block()
    {
        last_block_curr = blocks
            .block_record(last_block_curr.prev_hash)
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

/// chia `get_next_sub_slot_iters_and_difficulty` (ref `difficulty_adjustment.py`). The full-node entry
/// point: given the previous block `prev_b` (or `None` at genesis) and whether the block being computed
/// is the first in its sub-slot, returns `(sub_slot_iters, difficulty)` for that block.
///
/// # Errors
/// Returns an error if a referenced ancestor is missing or the retarget helpers fail.
pub fn get_next_sub_slot_iters_and_difficulty(
    constants: &ConsensusConstants,
    is_first_in_sub_slot: bool,
    prev_b: Option<&BlockRecord>,
    blocks: &impl BlockRecordProvider,
) -> Result<(u64, u64), Error> {
    let Some(prev_b) = prev_b else {
        return Ok((
            constants.sub_slot_iters_starting,
            constants.difficulty_starting,
        ));
    };

    let prev_difficulty = if prev_b.height != 0 {
        let prev_prev = blocks
            .block_record(prev_b.prev_hash)
            .ok_or_else(|| missing(prev_b.prev_hash))?;
        u64::try_from(prev_b.weight - prev_prev.weight)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "previous difficulty exceeds u64"))?
    } else {
        u64::try_from(prev_b.weight)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "genesis weight exceeds u64"))?
    };

    if prev_b.sub_epoch_summary_included.is_some() {
        // The retarget already happened at the sub-epoch-summary block; carry its values forward.
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

    /// Build a synthetic block. Only the fields the retarget rules read are meaningful; the rest are
    /// filled with valid placeholders. `signage_point_index = 0` / `required_iters = 1` / non-overflow
    /// give a well-formed `sp_total_iters` under `MAINNET`.
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

    // chia significant_bits.py docstring examples.
    #[test]
    fn truncate_and_count_match_chia_docstrings() {
        // truncate_to_significant_bits(0b011110101, 2) == 0b11000000
        assert_eq!(truncate_to_significant_bits(0b0_1111_0101, 2), 0b1100_0000);
        // count_significant_bits(0b000110010000) == 5
        assert_eq!(count_significant_bits(0b0001_1001_0000), 5);
        assert_eq!(count_significant_bits(0), 0);
        // Truncating a value with <= n significant bits is a no-op.
        assert_eq!(truncate_to_significant_bits(0b101, 8), 0b101);
    }

    #[test]
    fn height_can_be_first_in_epoch_marks_epoch_starts() {
        // Mainnet EPOCH_BLOCKS = 4608, SUB_EPOCH_BLOCKS = 384.
        assert!(height_can_be_first_in_epoch(&MAINNET, 0));
        assert!(height_can_be_first_in_epoch(&MAINNET, 4608));
        assert!(height_can_be_first_in_epoch(&MAINNET, 9216));
        assert!(!height_can_be_first_in_epoch(&MAINNET, 384));
        assert!(!height_can_be_first_in_epoch(&MAINNET, 4600));
    }

    #[test]
    fn genesis_returns_starting_values() {
        let blocks: std::collections::HashMap<Bytes32, BlockRecord> =
            std::collections::HashMap::new();
        let (ssi, diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, false, None, &blocks).unwrap();
        assert_eq!(ssi, MAINNET.sub_slot_iters_starting);
        assert_eq!(ssi, 2u64.pow(27));
        assert_eq!(diff, MAINNET.difficulty_starting);
        assert_eq!(diff, 7);
    }

    #[test]
    fn sub_epoch_summary_included_carries_values_forward() {
        // When prev_b itself included a sub-epoch summary, the retarget already happened there: the
        // next block reuses prev_b.sub_slot_iters and prev_b's own difficulty (weight - parent.weight).
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
        assert_eq!(out_diff, 7); // 700 - 693
    }

    /// Build a linear chain `0..=top` where each block adds `w_delta` weight, `iter_delta` total_iters,
    /// and `t_delta` seconds, every block a transaction block, constant `ssi`. Returns the map.
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

    // Reproduces the exact retarget math of chia's `_get_next_difficulty` / `_get_next_sub_slot_iters`
    // at the epoch 0 -> 1 boundary (prev height 4607, next height 4608 = EPOCH_BLOCKS). The
    // second-to-last-tx-block of the previous epoch resolves to genesis (height 0), and the walk for the
    // last tx block before the signage point resolves to height 4606. With a heavily over-target epoch
    // the raw retarget saturates, so both parameters clamp to the DIFFICULTY_CHANGE_MAX_FACTOR (3x)
    // ceiling, then truncate to SIGNIFICANT_BITS and (for ssi) round to a multiple of NUM_SPS_SUB_SLOT.
    #[test]
    fn get_next_difficulty_and_ssi_clamp_at_epoch_boundary() {
        let ssi_start = MAINNET.sub_slot_iters_starting; // 2^27
        let iter_delta: u128 = 10_000_000;
        let blocks = linear_chain(4607, 7, iter_delta, 1_000, 1, ssi_start);

        // prev_header_hash/height are prev_b.prev_hash / prev_b.height, per chia's call convention.
        // The signage point of block 4607 lands exactly on block 4606's total_iters.
        let sp_total_iters: u128 = iter_delta * 4606;

        let diff = get_next_difficulty(
            &MAINNET,
            &blocks,
            h32(4606), // prev_b.prev_hash
            4607,      // prev_b.height
            7,         // current difficulty
            0,         // deficit
            false,
            true, // new_slot
            sp_total_iters,
        )
        .unwrap();
        // precise = (7*4606 * 600) / (32 * 4606) = 131; >= 7 so min(131, 3*7=21) = 21; truncate(21,8)=21.
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
        // precise = 600 * (4606*1e7) / 4606 = 6e9; >= ssi so min(6e9, 3*2^27=402653184) = 402653184;
        // truncate(402653184,8)=402653184; 402653184 % 64 == 0.
        assert_eq!(next_ssi, 402_653_184);
        assert_eq!(next_ssi, 3 * ssi_start);
        assert_eq!(next_ssi % u64::from(MAINNET.num_sps_sub_slot), 0);
        assert!(count_significant_bits(u128::from(next_ssi)) <= MAINNET.significant_bits);
    }

    // Same boundary, but driven through the full `get_next_sub_slot_iters_and_difficulty` entry point,
    // which derives the signage-point total iters from prev_b itself. Block 4607 uses
    // signage_point_index 0 / required_iters 1, so its sp_total_iters lands just below block 4606's
    // total_iters and the last-tx-block walk stops at 4606 — reproducing the clamp result end to end.
    #[test]
    fn get_next_sub_slot_iters_and_difficulty_end_to_end_at_boundary() {
        let ssi_start = MAINNET.sub_slot_iters_starting;
        let iter_delta: u128 = 10_000_000;
        let blocks = linear_chain(4607, 7, iter_delta, 1_000, 1, ssi_start);
        let prev_b = blocks.block_record(h32(4607)).unwrap().clone();

        // The last-tx-block walk stops at the highest block whose total_iters <= sp_total_iters, so
        // block 4606's total_iters must be <= sp < block 4607's total_iters.
        let sp = prev_b.sp_total_iters(&MAINNET).unwrap();
        assert!(iter_delta * 4606 <= sp && sp < iter_delta * 4607);

        let (out_ssi, out_diff) =
            get_next_sub_slot_iters_and_difficulty(&MAINNET, true, Some(&prev_b), &blocks).unwrap();
        assert_eq!(out_diff, 21);
        assert_eq!(out_ssi, 402_653_184);
    }

    // Mid sub-epoch, a sub-epoch summary has already been included earlier in this sub-epoch, so
    // `can_finish_sub_and_full_epoch` reports the epoch cannot finish here and both parameters pass
    // through unchanged even though new_slot is true.
    #[test]
    fn mid_epoch_passes_difficulty_and_ssi_through() {
        let ssi_start = MAINNET.sub_slot_iters_starting;
        let mut blocks = linear_chain(5000, 7, 10_000_000, 1_000, 1, ssi_start);
        // The sub-epoch that contains height 4700 starts at 4608 (a multiple of SUB_EPOCH_BLOCKS=384).
        // Real chains include a sub-epoch summary in that first block; add one so the walk-back finds it.
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
}
