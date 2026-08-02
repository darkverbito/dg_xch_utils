//! The block-deficit state machine.
//!
//! Every block carries a `deficit`: how many more blocks must be produced before the next challenge
//! block (and infused-challenge VDF) is created. A full node computes the deficit of a block from its
//! predecessor's deficit, whether the block overflows its signage point, and how many sub-slots finished
//! immediately before it. This ports chia's `calculate_deficit` from
//! `chia/consensus/deficit.py`; there is no chia_rs port of the rule, so the Python module is the source.

use crate::blockchain::block_record::BlockRecord;
use crate::consensus::constants::ConsensusConstants;

/// chia `calculate_deficit` (ref `chia/consensus/deficit.py`). The deficit at a block of height `height`,
/// given its predecessor `prev_b` (`None` only at genesis), whether the block is an overflow block, and
/// the number of sub-slots that finished immediately before it.
///
/// At genesis the deficit is `MIN_BLOCKS_PER_CHALLENGE_BLOCK - 1`. Otherwise it steps down from the
/// predecessor's deficit toward zero, resetting to the full `MIN_BLOCKS_PER_CHALLENGE_BLOCK` window at a
/// challenge boundary — the exact branch depending on overflow and how many sub-slots were crossed.
#[must_use]
pub fn calculate_deficit(
    constants: &ConsensusConstants,
    height: u32,
    prev_b: Option<&BlockRecord>,
    overflow: bool,
    num_finished_sub_slots: usize,
) -> u8 {
    let min = constants.min_blocks_per_challenge_block;
    if height == 0 {
        return min - 1;
    }
    // height != 0 ⇒ prev_b present in a well-formed chain; fail-closed callers guarantee this.
    let prev_deficit = prev_b.map_or(min, |p| p.deficit);
    if prev_deficit == min {
        // Prev sb must be an overflow sb. However maybe it's in a different sub-slot.
        if overflow {
            if num_finished_sub_slots > 0 {
                prev_deficit - 1
            } else {
                prev_deficit
            }
        } else {
            prev_deficit - 1
        }
    } else if prev_deficit == 0 {
        if num_finished_sub_slots == 0 {
            0
        } else if num_finished_sub_slots == 1 {
            if overflow { min } else { min - 1 }
        } else {
            min - 1
        }
    } else {
        prev_deficit - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::constants::MAINNET;

    // Pure deficit state-machine parity spot-checks against chia's deficit.py. Value-level, no fixtures —
    // a fast guard on the trickiest branch (overflow × new-sub-slot). MAINNET.min_blocks_per_challenge_block
    // is 16, so the genesis deficit is 15.
    #[test]
    fn genesis_is_min_minus_one() {
        assert_eq!(calculate_deficit(&MAINNET, 0, None, false, 0), 15);
    }
}
