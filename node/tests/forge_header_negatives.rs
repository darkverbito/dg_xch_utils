// Tier-1 A-class header-mutation negatives, driven by corpus mutation rather than re-forging: we
// cannot farm plots or compute VDFs in a test, so each case starts from a REAL mainnet header (the
// committed recent-chain corpus, heights 9054524..=9054620 — the same fixture header_validation.rs
// validates clean), mutates ONE field, and drives it back through the node's promoted validator
// (`validate_finished_header`) against the real ancestor map. The block validated clean before the
// mutation, so a single-field flip this node catches is proof of the rejection seam.
//
// A-class = the mutation breaks a hash/quality/structural check the validator asserts INLINE, so
// the exact error name is recoverable. B-class mutations collapse under the deferred-VDF batch and
// live in forge_header_negatives_bclass.rs or are pinned by accepted-set. The target 9_054_611 is
// a clean non-genesis block with a signage point (sp_index != 0), so the sp/foliage signature and
// total-iters gates are all live.

use std::collections::HashMap;
use std::io::Cursor;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::sized_bytes::{Bytes32, Bytes96};
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_proof::VdfProof;
use dg_xch_core::blockchain::weight_proof::RecentChainData;
use dg_xch_core::consensus::block_header_validation::{
    ValidationState, validate_pospace_and_get_required_iters,
};
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::deficit::calculate_deficit;
use dg_xch_core::consensus::full_block_to_block_record::header_block_to_sub_block_record;
use dg_xch_core::consensus::get_block_challenge::pre_sp_tx_block_height;
use dg_xch_core::consensus::pot_iterations::is_overflow_block;
use dg_xch_node::{NativePrimitives, PrimitiveVerifier, validate_finished_header};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

// Golden ssi/difficulty for the sliced region (no sub-epoch boundary inside it) — the weight-proof
// recent-block validator's ground truth, identical to header_validation.rs.
const SSI: u64 = 574_619_648;
const DIFF: u64 = 2608;
// A clean, VALIDATED target inside the slice (header_validation.rs pins its required_iters).
const TARGET: u32 = 9_054_611;

fn load_chain() -> Vec<HeaderBlock> {
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054524_9054620.bin");
    RecentChainData::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("recent chain slice deserializes")
        .recent_chain_data
}

// Light-path (proof-of-space only, no VDF) required_iters — seeds ancestor records so the
// target's full validation reads a correct pb.ip_iters.
fn light_required_iters(
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

// Rebuild block records across the slice (light path), returning the ancestor map keyed by header hash.
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
            required_iters = light_required_iters(&ancestors, block, ch, pc, overflow);
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

fn vs() -> ValidationState {
    ValidationState {
        ssi: SSI,
        difficulty: DIFF,
    }
}

// The clean, validated target header — a fresh clone per mutation.
fn target(chain: &[HeaderBlock]) -> HeaderBlock {
    chain
        .iter()
        .find(|b| b.height() == TARGET)
        .expect("target in slice")
        .clone()
}

// Sanity: the UNMUTATED target validates clean, so every rejection below is attributable to the one
// mutated field (not a broken harness).
#[test]
fn unmutated_target_validates_clean() {
    let chain = load_chain();
    let ancestors = build_ancestors(&chain);
    let ok = validate_finished_header(
        &NativePrimitives,
        &MAINNET,
        &ancestors,
        &target(&chain),
        vs(),
        false,
    );
    assert!(ok.is_ok(), "unmutated target must validate: {ok:?}");
}

// Drive one mutated target through the validator and assert the rejection carries `expected`.
fn assert_rejects(mutate: impl FnOnce(&mut HeaderBlock), expected: &str) {
    let chain = load_chain();
    let ancestors = build_ancestors(&chain);
    let mut block = target(&chain);
    mutate(&mut block);
    let err =
        validate_finished_header(&NativePrimitives, &MAINNET, &ancestors, &block, vs(), false)
            .expect_err("mutated header must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains(expected),
        "expected rejection containing {expected:?}, got {msg:?}"
    );
}

// A broken proof-of-space field makes the quality string None -> INVALID_POSPACE.
#[test]
fn bad_proof_of_space_challenge_is_invalid_pospace() {
    assert_rejects(
        |b| b.reward_chain_block.proof_of_space.challenge = Bytes32::from([9u8; 32]),
        "INVALID_POSPACE",
    );
}

// total_iters off by one -> INVALID_TOTAL_ITERS.
#[test]
fn bad_total_iters_is_rejected() {
    assert_rejects(
        |b| b.reward_chain_block.total_iters += 1,
        "INVALID_TOTAL_ITERS",
    );
}

// A wrong foliage.reward_block_hash -> INVALID_REWARD_BLOCK_HASH (check 32, inline).
#[test]
fn bad_reward_block_hash_is_rejected() {
    assert_rejects(
        |b| b.foliage.reward_block_hash = Bytes32::from([7u8; 32]),
        "INVALID_REWARD_BLOCK_HASH",
    );
}

// Re-cohere the foliage's unfinished-reward-block hash (check 18, INVALID_URSB_HASH) to the mutated
// reward-chain block. The signage-point signatures are part of the reward-chain block hash, so a naive
// sig flip ALSO breaks the inline URSB check, which fires BEFORE the deferred signature batch (our
// window pipeline defers header-sig verification, then drains it after the inline ladder — see
// node/src/header.rs). The pure-hash re-cohere restores the intended ordering — sig check before
// check 18 — and lets the deferred batch be the sole catch, reporting the exact sig tag via
// first_failing_sig.
fn recohere_reward_hashes(b: &mut HeaderBlock) {
    // The sp signatures live in the reward-chain block, so a sig flip perturbs BOTH reward-block
    // hash commitments the inline ladder checks before the deferred sig batch runs: check 18 hashes
    // the UNFINISHED view (get_unfinished, which carries the sp sigs) and check 32 the FINISHED
    // reward_chain_block hash. Re-cohere both (pure hashing) so the deferred batch is the sole catch.
    b.foliage.foliage_block_data.unfinished_reward_block_hash = b
        .reward_chain_block
        .get_unfinished()
        .hash()
        .expect("ursb hash");
    b.foliage.reward_block_hash = b.reward_chain_block.hash().expect("reward block hash");
}

// A bad reward-chain signage-point signature ->
// INVALID_RC_SIGNATURE. A zero G2 point fails closed in bls_verify (no panic).
#[test]
fn bad_rc_sp_signature_is_rejected() {
    assert_rejects(
        |b| {
            b.reward_chain_block.reward_chain_sp_signature = Bytes96::from([0u8; 96]);
            recohere_reward_hashes(b);
        },
        "INVALID_RC_SIGNATURE",
    );
}

// A bad challenge-chain signage-point signature ->
// INVALID_CC_SIGNATURE.
#[test]
fn bad_cc_sp_signature_is_rejected() {
    assert_rejects(
        |b| {
            b.reward_chain_block.challenge_chain_sp_signature = Bytes96::from([0u8; 96]);
            recohere_reward_hashes(b);
        },
        "INVALID_CC_SIGNATURE",
    );
}

// A bad foliage-block-data signature ->
// INVALID_PLOT_SIGNATURE (block data).
#[test]
fn bad_foliage_block_data_signature_is_rejected() {
    assert_rejects(
        |b| b.foliage.foliage_block_data_signature = Bytes96::from([0u8; 96]),
        "INVALID_PLOT_SIGNATURE",
    );
}

// An out-of-range signage-point index (== NUM_SPS_SUB_SLOT) reaches check 6 ->
// INVALID_SP_INDEX.
#[test]
fn signage_point_index_at_bound_is_invalid_sp_index() {
    let bound = u8::try_from(MAINNET.num_sps_sub_slot).expect("num_sps fits u8");
    assert_rejects(
        |b| b.reward_chain_block.signage_point_index = bound,
        "INVALID_SP_INDEX",
    );
}

fn garbage_proof() -> VdfProof {
    // witness_type 0, a non-empty non-witness, not normalized-to-identity: takes the standard
    // validate_vdf branch, which the deferred batch then fails.
    VdfProof {
        witness_type: 0,
        witness: UnsizedBytes::new(vec![0u8; 100]),
        normalized_to_identity: false,
    }
}

// Proof arm: a bad challenge-chain infusion-point
// VDF proof → INVALID_CC_IP_VDF. Coarse: INVALID_VDF (deferred batch) until C5.
#[test]
fn bad_cc_ip_proof_is_invalid_vdf() {
    assert_rejects(
        |b| b.challenge_chain_ip_proof = garbage_proof(),
        "INVALID_VDF",
    );
}

// Proof arm: a bad reward-chain infusion-point VDF
// proof → INVALID_RC_IP_VDF. Coarse: INVALID_VDF (deferred batch) until C5.
#[test]
fn bad_rc_ip_proof_is_invalid_vdf() {
    assert_rejects(|b| b.reward_chain_ip_proof = garbage_proof(), "INVALID_VDF");
}

// Output arm: a wrong challenge-chain IP VDF
// output. The output lives in the reward_chain_block, so check 29's DATA comparison still holds (both
// sides read the mutated output) but the finished reward-block hash (check 32) breaks — re-cohere it
// so the deferred proof check is the sole catch.
#[test]
fn bad_cc_ip_output_is_invalid_vdf() {
    assert_rejects(
        |b| {
            b.reward_chain_block.challenge_chain_ip_vdf.output =
                ClassgroupElement::get_default_element();
            recohere_reward_hashes(b);
        },
        "INVALID_VDF",
    );
}

// Output arm: a wrong reward-chain IP VDF output
// (target-form check 30) → INVALID_RC_IP_VDF, coarse INVALID_VDF. Same reward-block-hash re-cohere.
#[test]
fn bad_rc_ip_output_is_invalid_vdf() {
    assert_rejects(
        |b| {
            b.reward_chain_block.reward_chain_ip_vdf.output =
                ClassgroupElement::get_default_element();
            recohere_reward_hashes(b);
        },
        "INVALID_VDF",
    );
}
