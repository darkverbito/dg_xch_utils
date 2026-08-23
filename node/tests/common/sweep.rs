//! Test-support for the geometric-boundary sweeps (test class 3): mainnet-geometry record chains
//! synthesized the way `anchor_epoch_gap` builds them, plus the boundary-offset classes
//! {B-1, B, B+1, mid-band} around B ∈ {SUB_EPOCH_BLOCKS = 384, EPOCH_BLOCKS = 4608} — the offsets
//! where the era-anchor and ssi-window bugs both died.

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_output::VdfOutput;
use dg_xch_core::consensus::constants::MAINNET;
use std::collections::HashMap;

/// The live wall's epoch boundary (993 * `EPOCH_BLOCKS` = 4,575,744) — real mainnet geometry.
pub const EPOCH_BOUNDARY: u32 = 4_575_744;
/// A mid-epoch sub-epoch boundary three sub-epochs past it (not an epoch multiple).
pub const SUB_EPOCH_BOUNDARY: u32 = EPOCH_BOUNDARY + 3 * 384;
/// Where the chain includes a sub-epoch summary: a couple of blocks past the boundary (the first
/// new-slot block of the new sub-epoch; `anchor_epoch_gap` uses the same convention).
pub const SES_INCLUSION_OFFSET: u32 = 2;

#[must_use]
pub fn h32(n: u32) -> Bytes32 {
    let mut b = [0u8; 32];
    b[..4].copy_from_slice(&n.to_be_bytes());
    Bytes32::from(b)
}

/// The boundary-offset classes for boundary `b0` of period `b`: one below, on it, one above, and a
/// mid-band offset chosen OFF the sub-epoch grid (so a mid-band position is never itself a boundary).
#[must_use]
pub fn offset_classes(b0: u32, b: u32) -> [u32; 4] {
    [b0 - 1, b0, b0 + 1, b0 + b / 2 + 3]
}

/// The pending epoch boundary for a resume/anchor position `at`: the smallest epoch multiple whose
/// retarget-trigger window can still be ahead of `at` (mirrors `epoch_backfill_low`'s rounding).
#[must_use]
pub fn pending_epoch_boundary(at: u32) -> u32 {
    (at.saturating_sub(MAINNET.sub_epoch_blocks) / MAINNET.epoch_blocks + 1) * MAINNET.epoch_blocks
}

#[must_use]
pub fn plain_ses() -> SubEpochSummary {
    SubEpochSummary {
        prev_subepoch_summary_hash: Bytes32::default(),
        reward_chain_hash: Bytes32::default(),
        num_blocks_overflow: 0,
        new_difficulty: None,
        new_sub_slot_iters: None,
    }
}

/// A mainnet-geometry record: linear weight/iters/timestamps, every block a transaction block, the
/// starting sub-slot iters — `anchor_epoch_gap`'s `record`, shared. An SES record also carries the
/// finished reward-slot hashes `make_sub_epoch_summary` reads from the previous summary block.
#[must_use]
pub fn record(height: u32, ses: Option<SubEpochSummary>) -> BlockRecord {
    let is_ses = ses.is_some();
    BlockRecord {
        header_hash: h32(height),
        prev_hash: h32(height.wrapping_sub(1)),
        height,
        weight: 7 * u128::from(height),
        total_iters: 10_000_000 * u128::from(height),
        signage_point_index: 0,
        challenge_vdf_output: VdfOutput {
            data: UnsizedBytes::new(vec![]),
        },
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
        finished_reward_slot_hashes: is_ses.then(|| vec![Bytes32::default()]),
        sub_epoch_summary_included: ses,
    }
}

/// A linear record chain over `[low, high]` with realistic SES placement: every sub-epoch boundary
/// strictly below `ses_below` carries its summary at boundary + [`SES_INCLUSION_OFFSET`] (the
/// headers-first backfill stores exactly these from the weight proof's summary chain); boundaries at
/// or above `ses_below` are PENDING — their summary is not yet included, so the sweeps' walks take
/// their worst-case depth there.
#[must_use]
pub fn chain(low: u32, high: u32, ses_below: u32) -> HashMap<Bytes32, BlockRecord> {
    let mut blocks = HashMap::new();
    for h in low..=high {
        let ses = (h >= SES_INCLUSION_OFFSET
            && (h - SES_INCLUSION_OFFSET).is_multiple_of(MAINNET.sub_epoch_blocks)
            && h - SES_INCLUSION_OFFSET < ses_below)
            .then(plain_ses);
        let b = record(h, ses);
        blocks.insert(b.header_hash, b);
    }
    blocks
}
