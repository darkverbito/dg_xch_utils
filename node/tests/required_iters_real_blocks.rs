// Header/proof-of-space `required_iters` coverage: the path that the block-body harvest never
// exercised. The live node cleared the block-body wall and advanced its peak, then stalled one
// block later with:
//
//   Required iters 0 is not below the sp interval iters 2097152, 134217728 or not > 0.
//
// That verbatim message comes from `calculate_ip_iters` and fires ONLY when `required_iters == 0`. But
// `calculate_iterations_quality` clamps its result with `max(_, 1)` (chia pot_iterations.py), so a real
// proof of space can NEVER produce 0 — a `required_iters` of 0 is always a fabricated/stored value. The
// engine's follow path (`Engine::derive_required_iters`) used to store a fabricated 0 for a block whose
// deep ancestor context was not yet cached (a checkpoint/bootstrap entry point). chia never does this: every
// BlockRecord carries its true `required_iters`, and `get_next_sub_slot_iters_and_difficulty` reads it back
// through `prev_b.sp_total_iters()` -> `ip_iters()` -> `calculate_ip_iters()`
// (chia/consensus/difficulty_adjustment.py::get_next_sub_slot_iters_and_difficulty), which rejects 0.
//
// The chain here is the same real mainnet recent chain (heights 9054524..=9054620) the header-validation test uses,
// sliced from the committed weight proof. `GOLDEN` are the weight-proof recent-block validator's reference
// `required_iters` for two blocks — the independent oracle these tests check the block-sync path against.

use std::collections::HashMap;
use std::io::Cursor;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::weight_proof::RecentChainData;
use dg_xch_core::consensus::block_header_validation::{
    ValidationState, validate_pospace_and_get_required_iters,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::deficit::calculate_deficit;
use dg_xch_core::consensus::difficulty_adjustment::get_next_sub_slot_iters_and_difficulty;
use dg_xch_core::consensus::full_block_to_block_record::header_block_to_sub_block_record;
use dg_xch_core::consensus::get_block_challenge::pre_sp_tx_block_height;
use dg_xch_core::consensus::pot_iterations::{calculate_sp_interval_iters, is_overflow_block};
use dg_xch_node::{NativePrimitives, PrimitiveVerifier, validate_finished_header};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

const SSI: u64 = 574_619_648;
const DIFF: u64 = 2608;
const GOLDEN: &[(u32, u64)] = &[(9_054_611, 2_155_695), (9_054_612, 7_701_062)];

fn load_chain() -> Vec<HeaderBlock> {
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054524_9054620.bin");
    RecentChainData::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("recent chain slice deserializes")
        .recent_chain_data
}

// The seeding light path (used only to BUILD the ancestor records so the full validator has a chain to walk).
// It intentionally mirrors weight_proof.py's `_validate_pospace_recent_chain`.
fn seed_required_iters(
    ancestors: &HashMap<Bytes32, BlockRecord>,
    block: &HeaderBlock,
    challenge: Bytes32,
    prev_challenge: Bytes32,
    overflow: bool,
) -> u64 {
    let rcb = &block.reward_chain_block;
    let cc_sp_hash = match &rcb.challenge_chain_sp_vdf {
        None => challenge,
        Some(v) => v.output.hash().expect("cc sp hash"),
    };
    let pre = pre_sp_tx_block_height(
        &MAINNET,
        ancestors,
        block.prev_header_hash(),
        rcb.signage_point_index,
        block.finished_sub_slots.len(),
    )
    .expect("pre_sp_tx_block_height");
    validate_pospace_and_get_required_iters(
        &PrimitiveVerifier(&NativePrimitives),
        &MAINNET,
        &rcb.proof_of_space,
        if overflow { prev_challenge } else { challenge },
        cc_sp_hash,
        block.height(),
        DIFF,
        pre,
    )
    .expect("pospace")
    .expect("valid pospace")
}

// EXACTLY the computation `Engine::pospace_required_iters` (the required_iters fix) performs: the block's own
// declared `pos_ss_cc_challenge_hash` as the pospace challenge, difficulty = weight increment over prev, and
// prev_transaction_block_height = 0 (unused by dg_xch's pospace quality). No deep ancestor walk, no VDF.
fn fix_pospace_required_iters(block: &HeaderBlock, prev_weight: u128) -> u64 {
    let rcb = &block.reward_chain_block;
    let challenge = rcb.pos_ss_cc_challenge_hash;
    let cc_sp_hash = match &rcb.challenge_chain_sp_vdf {
        None => challenge,
        Some(v) => v.output.hash().expect("cc sp hash"),
    };
    let difficulty = u64::try_from(block.weight() - prev_weight).expect("difficulty fits u64");
    validate_pospace_and_get_required_iters(
        &PrimitiveVerifier(&NativePrimitives),
        &MAINNET,
        &rcb.proof_of_space,
        challenge,
        cc_sp_hash,
        block.height(),
        difficulty,
        0,
    )
    .expect("pospace")
    .expect("valid pospace")
}

fn build_ancestors(chain: &[HeaderBlock]) -> HashMap<Bytes32, BlockRecord> {
    let c = &MAINNET;
    let mut ancestors: HashMap<Bytes32, BlockRecord> = HashMap::new();
    let mut challenge = Some(chain[0].reward_chain_block.pos_ss_cc_challenge_hash);
    let mut prev_challenge: Option<Bytes32> = None;
    let mut prev_rec: Option<BlockRecord> = None;
    let mut deficit: u8 = 0;
    let mut tx_blocks: u32 = 0;

    for block in chain {
        let rcb = &block.reward_chain_block;
        let h = block.height();
        let mut overflow = false;
        for ss in &block.finished_sub_slots {
            prev_challenge = Some(ss.challenge_chain.challenge_chain_end_of_slot_vdf.challenge);
            challenge = Some(ss.challenge_chain.hash().expect("cc hash"));
            deficit = ss.reward_chain.deficit;
        }
        let mut required_iters = 0u64;
        if let (Some(ch), Some(pc)) = (challenge, prev_challenge)
            && tx_blocks > 2
        {
            overflow = is_overflow_block(c, rcb.signage_point_index).expect("overflow");
            deficit = calculate_deficit(
                c,
                h,
                prev_rec.as_ref(),
                overflow,
                block.finished_sub_slots.len(),
            );
            required_iters = seed_required_iters(&ancestors, block, ch, pc, overflow);
        }
        let rec = header_block_to_sub_block_record(
            c,
            required_iters,
            block,
            SSI,
            overflow,
            deficit,
            h,
            None,
        )
        .expect("record");
        ancestors.insert(rec.header_hash, rec.clone());
        if rcb.is_transaction_block {
            tx_blocks += 1;
        }
        prev_rec = Some(rec);
    }
    ancestors
}

// THE FIX. For real mainnet blocks the ancestor-independent pospace computation (what the engine now stores
// when the deep VDF context is not yet cached) must (a) land in the open interval (0, sp_interval_iters) —
// never 0, the value that crashes the difficulty retarget — and (b) equal BOTH the full PoW/VDF validator's
// required_iters AND the weight-proof reference for the same block. If (b) held but the engine still stored 0,
// the zero-seed crash would recur; if the fix computed a DIFFERENT nonzero value, it would silently corrupt the retarget.
#[test]
fn pospace_required_iters_matches_full_validation_and_is_in_range() {
    let chain = load_chain();
    assert!(chain.len() > 80, "slice present");
    let ancestors = build_ancestors(&chain);
    let by_height: HashMap<u32, &HeaderBlock> = chain.iter().map(|b| (b.height(), b)).collect();
    let rec_by_height: HashMap<u32, &BlockRecord> =
        ancestors.values().map(|r| (r.height, r)).collect();

    let sp_interval = calculate_sp_interval_iters(&MAINNET, SSI).expect("sp interval");
    let vs = ValidationState {
        ssi: SSI,
        difficulty: DIFF,
    };

    for &(height, expected) in GOLDEN {
        let block = by_height.get(&height).expect("target in slice");
        let prev = rec_by_height
            .get(&(height - 1))
            .expect("prev record in slice");

        // The difficulty the fix derives (weight increment over prev) is the epoch difficulty chia enforces
        // with the INVALID_WEIGHT check (block.weight == prev.weight + difficulty).
        assert_eq!(
            u64::try_from(block.weight() - prev.weight).unwrap(),
            DIFF,
            "height {height}: weight-delta difficulty equals the epoch difficulty",
        );

        let fixed = fix_pospace_required_iters(block, prev.weight);
        assert!(
            fixed > 0 && fixed < sp_interval,
            "height {height}: required_iters {fixed} must be in (0, {sp_interval}) — 0 is the crash value",
        );
        assert_eq!(
            fixed, expected,
            "height {height}: matches weight-proof reference"
        );

        // The full PoW/VDF validator's required_iters for the same block (the value it would store when the
        // deep context IS cached) must equal the ancestor-independent fix — the two paths cannot diverge.
        let full =
            validate_finished_header(&NativePrimitives, &MAINNET, &ancestors, block, vs, false)
                .unwrap_or_else(|e| panic!("full header validation at {height}: {e}"));
        assert_eq!(
            full, fixed,
            "height {height}: full path and pospace fix agree"
        );
    }
}

// The POISON, characterized end to end + the guard that a genuinely-zero required_iters stays fatal (the fix
// is "never store 0", NOT "tolerate 0"). Take a real prev record, blank its required_iters to 0, and drive
// the exact live call — get_next_sub_slot_iters_and_difficulty for the NEXT block — which reads
// prev_b.sp_total_iters(). It must surface the verbatim ip-iters rejection; with the real required_iters the
// same read succeeds.
#[test]
fn stored_zero_required_iters_poisons_the_difficulty_retarget() {
    let chain = load_chain();
    let mut ancestors = build_ancestors(&chain);
    let (height, _) = GOLDEN[1]; // a block with a real, nonzero required_iters

    // Sanity: with the real required_iters, the retarget read succeeds.
    let prev = ancestors
        .values()
        .find(|r| r.height == height)
        .cloned()
        .expect("prev record in slice");
    assert!(
        prev.required_iters > 0,
        "fixture prev carries a real required_iters"
    );
    get_next_sub_slot_iters_and_difficulty(&MAINNET, false, Some(&prev), &ancestors)
        .expect("real required_iters retargets cleanly");

    // Poison it exactly as the old fabricated-0 path did, then re-run the identical read.
    let mut poisoned = prev.clone();
    poisoned.required_iters = 0;
    ancestors.insert(poisoned.header_hash, poisoned.clone());
    let err = get_next_sub_slot_iters_and_difficulty(&MAINNET, false, Some(&poisoned), &ancestors)
        .expect_err("required_iters == 0 must be rejected, not silently retargeted");
    let msg = err.to_string();
    assert!(
        msg.contains("Required iters 0") && msg.contains("not > 0"),
        "the poison surfaces the exact live ip-iters rejection, got: {msg}",
    );
}
