// Sub-epoch-summary construction.
// Ports chia/consensus/make_sub_epoch_summary.py (no chia_rs port exists).

use crate::blockchain::block_record::BlockRecord;
use crate::blockchain::sized_bytes::Bytes32;
use crate::blockchain::sub_epoch_summary::SubEpochSummary;
use crate::consensus::constants::ConsensusConstants;
use crate::consensus::missing;
use std::collections::HashMap;
use std::io::{Error, ErrorKind};

// dg_xch's SubEpochSummary is the mainnet-active 5-field form (no challenge_merkle_root); do not add the
// 6th field until it activates.
pub fn make_sub_epoch_summary(
    constants: &ConsensusConstants,
    blocks: &HashMap<Bytes32, BlockRecord>,
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
    // First sub-epoch: no previous summary to link, so it is genesis-anchored.
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
    let mut curr = prev_prev_block;
    while curr.sub_epoch_summary_included.is_none() {
        curr = blocks
            .get(&curr.prev_hash)
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
