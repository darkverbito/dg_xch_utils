//! Sub-epoch-summary construction.
//!
//! At a sub-epoch boundary a full node builds the [`SubEpochSummary`] the next block must commit to. The
//! summary pins the sub-epoch's `num_blocks_overflow`, links the previous summary by hash, carries the
//! previous sub-epoch-summary block's final reward-chain slot hash, and — at a full-epoch boundary —
//! commits the retargeted `new_difficulty` / `new_sub_slot_iters` (as produced by
//! [`crate::consensus::difficulty_adjustment`]). This ports chia's `make_sub_epoch_summary` from
//! `chia/consensus/make_sub_epoch_summary.py`; there is no chia_rs port of the rule, so the Python module
//! is the source.
//!
//! Everything here is a pure function of previously-validated [`BlockRecord`]s reached through a
//! [`BlockRecordProvider`] and the [`ConsensusConstants`]. It reuses the existing [`SubEpochSummary`]
//! type — no parallel model — and its inherent [`SubEpochSummary::hash`] for the prev-summary linkage.

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::sub_epoch_summary::SubEpochSummary;
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::difficulty_adjustment::BlockRecordProvider;
use std::io::{Error, ErrorKind};

fn missing(hash: Bytes32) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("block record not found: {hash}"),
    )
}

/// chia `make_sub_epoch_summary` (ref `chia/consensus/make_sub_epoch_summary.py`). Reconstructs the
/// [`SubEpochSummary`] expected at `blocks_included_height` — the height of the block that will *include*
/// the summary — from `prev_prev_block` (the record two heights below it) walking back through `blocks`
/// to the previous sub-epoch-summary block.
///
/// `new_difficulty` / `new_sub_slot_iters` are the retargeted values a full-epoch boundary commits (from
/// [`crate::consensus::difficulty_adjustment::get_next_sub_slot_iters_and_difficulty`]), or `None` at a
/// sub-epoch boundary that does not also close an epoch.
///
/// dg_xch's [`SubEpochSummary`] is the mainnet-active 5-field form (no `challenge_merkle_root`), matching
/// mainnet's on-chain summary hashes; the 6th field must not be added until it activates.
///
/// # Errors
/// Returns an error if `prev_prev_block` is not two heights below `blocks_included_height`, if the
/// ancestor walk references a header hash `blocks` does not contain, if the previous sub-epoch-summary
/// block is missing its included summary or its finished reward-slot hashes, or if a derived value does
/// not fit its integer type.
pub fn make_sub_epoch_summary(
    constants: &ConsensusConstants,
    blocks: &impl BlockRecordProvider,
    blocks_included_height: u32,
    prev_prev_block: &BlockRecord,
    new_difficulty: Option<u64>,
    new_sub_slot_iters: Option<u64>,
) -> Result<SubEpochSummary, Error> {
    if prev_prev_block.height != blocks_included_height.wrapping_sub(2) {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "prev_prev_block is not two heights below blocks_included_height",
        ));
    }
    // First sub-epoch: there is no previous summary to link, so it is genesis-anchored.
    if (u64::from(blocks_included_height) + u64::from(constants.max_sub_slot_blocks))
        / u64::from(constants.sub_epoch_blocks)
        <= 1
    {
        return Ok(SubEpochSummary {
            prev_subepoch_summary_hash: constants.genesis_challenge,
            reward_chain_hash: constants.genesis_challenge,
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        });
    }
    // Walk back to the block that included the previous sub-epoch summary.
    let mut curr = prev_prev_block;
    while curr.sub_epoch_summary_included.is_none() {
        curr = blocks
            .block_record(curr.prev_hash)
            .ok_or_else(|| missing(curr.prev_hash))?;
    }
    let prev_ses_block = curr;
    let included = prev_ses_block
        .sub_epoch_summary_included
        .as_ref()
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous sub-epoch-summary block has no included summary",
            )
        })?;
    let reward_slot_hashes = prev_ses_block
        .finished_reward_slot_hashes
        .as_ref()
        .ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous sub-epoch-summary block has no finished reward-slot hashes",
            )
        })?;
    Ok(SubEpochSummary {
        prev_subepoch_summary_hash: included.hash()?,
        reward_chain_hash: *reward_slot_hashes.last().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidData,
                "previous sub-epoch-summary block has empty finished reward-slot hashes",
            )
        })?,
        num_blocks_overflow: u8::try_from(prev_ses_block.height % constants.sub_epoch_blocks)
            .map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    "sub-epoch overflow block count exceeds u8",
                )
            })?,
        new_difficulty,
        new_sub_slot_iters,
    })
}
