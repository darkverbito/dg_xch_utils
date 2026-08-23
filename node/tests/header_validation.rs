// Full single-block PoW/VDF header validation outside the weight-proof path. A real mainnet recent
// chain (heights 9054524..=9054620, sliced from the committed weight proof) is rebuilt into block records,
// then target blocks are validated through the node's PrimitiveVerifier — the derived required_iters must
// equal the reference the weight-proof path produces for the same block.

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
use dg_xch_core::consensus::full_block_to_block_record::header_block_to_sub_block_record;
use dg_xch_core::consensus::get_block_challenge::pre_sp_tx_block_height;
use dg_xch_core::consensus::pot_iterations::is_overflow_block;
use dg_xch_node::{NativePrimitives, PrimitiveVerifier, validate_finished_header};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

// Golden ssi/difficulty for the sliced region (no sub-epoch boundary inside it), and per-height reference
// required_iters — both produced by the weight-proof recent-block validator over the full committed proof.
const SSI: u64 = 574_619_648;
const DIFF: u64 = 2608;
const GOLDEN: &[(u32, u64)] = &[(9_054_611, 2_155_695), (9_054_612, 7_701_062)];

fn load_chain() -> Vec<HeaderBlock> {
    let bytes = include_bytes!("fixtures/recent_chain_mainnet_9054524_9054620.bin");
    RecentChainData::from_bytes(&mut Cursor::new(&bytes[..]), ChiaProtocolVersion::default())
        .expect("recent chain slice deserializes")
        .recent_chain_data
}

// Light-path (proof-of-space only, no VDF) required_iters — used to seed ancestor records so a target's
// full validation reads a correct pb.ip_iters. Mirrors weight_proof.py's _validate_pospace_recent_chain.
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

#[test]
fn validates_mainnet_header_outside_weight_proof_with_reference_required_iters() {
    let chain = load_chain();
    assert!(chain.len() > 80, "slice present");
    let ancestors = build_ancestors(&chain);

    let by_height: HashMap<u32, &HeaderBlock> = chain.iter().map(|b| (b.height(), b)).collect();
    let vs = ValidationState {
        ssi: SSI,
        difficulty: DIFF,
    };

    for &(height, expected) in GOLDEN {
        let block = by_height.get(&height).expect("target in slice");
        // The node's promoted validator, driven by the ConsensusPrimitives-backed verifier — not the
        // weight-proof loop. check_sub_epoch_summary=false: the slice carries no sub-epoch boundary.
        let required_iters =
            validate_finished_header(&NativePrimitives, &MAINNET, &ancestors, block, vs, false)
                .unwrap_or_else(|e| panic!("validate header at {height}: {e}"));
        assert_eq!(
            required_iters, expected,
            "required_iters at {height} matches the weight-proof reference"
        );
    }
}
