// Mainnet hash-exact cross-check for make_sub_epoch_summary against the real on-chain SES sequence.

mod common;

use std::collections::HashMap;

use common::load_fixture;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::make_sub_epoch_summary::make_sub_epoch_summary;

fn empty_vdf() -> ClassgroupElement {
    ClassgroupElement::get_default_element()
}

fn ses_block(
    height: u32,
    included: SubEpochSummary,
    reward_slot_hashes: Vec<Bytes32>,
) -> BlockRecord {
    BlockRecord {
        header_hash: Bytes32::default(),
        prev_hash: Bytes32::default(),
        height,
        weight: 0,
        total_iters: 0,
        signage_point_index: 0,
        challenge_vdf_output: empty_vdf(),
        infused_challenge_vdf_output: None,
        reward_infusion_new_challenge: Bytes32::default(),
        challenge_block_info_hash: Bytes32::default(),
        sub_slot_iters: MAINNET.sub_slot_iters_starting,
        pool_puzzle_hash: Bytes32::default(),
        farmer_puzzle_hash: Bytes32::default(),
        required_iters: 1,
        deficit: 0,
        overflow: false,
        prev_transaction_block_height: 0,
        timestamp: None,
        prev_transaction_block_hash: None,
        fees: None,
        reward_claims_incorporated: None,
        finished_challenge_slot_hashes: None,
        finished_infused_challenge_slot_hashes: None,
        finished_reward_slot_hashes: Some(reward_slot_hashes),
        sub_epoch_summary_included: Some(included),
    }
}

// Genesis-anchored reconstruction of the on-chain SES sequence from wp.sub_epochs (chia _map_sub_epoch_summaries).
fn real_sub_epoch_summaries(wp: &WeightProof) -> Vec<SubEpochSummary> {
    let mut prev = MAINNET.genesis_challenge;
    let mut out = Vec::with_capacity(wp.sub_epochs.len());
    for d in &wp.sub_epochs {
        let ses = SubEpochSummary {
            prev_subepoch_summary_hash: prev,
            reward_chain_hash: d.reward_chain_hash,
            num_blocks_overflow: d.num_blocks_overflow,
            new_difficulty: d.new_difficulty,
            new_sub_slot_iters: d.new_sub_slot_iters,
        };
        prev = ses.hash().expect("ses hashes");
        out.push(ses);
    }
    out
}

#[test]
fn make_sub_epoch_summary_matches_mainnet_ses_hashes() {
    let wp = load_fixture();
    assert_eq!(
        wp.sub_epochs.len(),
        23_579,
        "expected the full mainnet SES chain"
    );
    let real = real_sub_epoch_summaries(&wp);

    // No block records are traversed: the prev-summary block IS `prev_prev_block` (it already carries the
    // included summary), so the walk-back loop never queries the provider. An empty map suffices.
    let blocks: HashMap<Bytes32, BlockRecord> = HashMap::new();
    let sub_epoch_blocks = MAINNET.sub_epoch_blocks; // 384

    let mut matched = 0usize;
    let mut first_match: Option<(usize, String)> = None;
    let mut last_match: Option<(usize, String)> = None;

    for k in 1..real.len() {
        let prev_ses = real[k - 1];
        let expected = &real[k];

        // Place the previous-summary block at a large height whose residue mod SUB_EPOCH_BLOCKS equals the
        // real `num_blocks_overflow`, so `make_sub_epoch_summary` re-derives that field from the height.
        let overflow = u32::from(expected.num_blocks_overflow);
        let prev_ses_height = sub_epoch_blocks * 1_000 + overflow;
        let blocks_included_height = prev_ses_height + 2; // prev_prev_block sits two below

        let prev_prev = ses_block(prev_ses_height, prev_ses, vec![expected.reward_chain_hash]);

        let reconstructed = make_sub_epoch_summary(
            &MAINNET,
            &blocks,
            blocks_included_height,
            &prev_prev,
            expected.new_difficulty,
            expected.new_sub_slot_iters,
        )
        .expect("make_sub_epoch_summary reconstructs");

        // Field-exact, then hash-exact against the real on-chain summary.
        assert_eq!(&reconstructed, expected, "SES {k} field mismatch");
        let got = reconstructed.hash().expect("hash");
        let want = expected.hash().expect("hash");
        assert_eq!(got, want, "SES {k} hash mismatch");

        matched += 1;
        let hex = hex::encode(AsRef::<[u8]>::as_ref(&got));
        if first_match.is_none() {
            first_match = Some((k, hex.clone()));
        }
        last_match = Some((k, hex));
    }

    assert!(matched > 20_000, "expected thousands of real SES matches");
    let (fk, fh) = first_match.unwrap();
    let (lk, lh) = last_match.unwrap();
    eprintln!("make_sub_epoch_summary: {matched} mainnet SES hashes reconstructed hash-exact");
    eprintln!("  first match: SES #{fk} -> {fh}");
    eprintln!("  last  match: SES #{lk} -> {lh}");
}

#[test]
fn make_sub_epoch_summary_returns_genesis_anchored_first_summary() {
    let blocks: HashMap<Bytes32, BlockRecord> = HashMap::new();
    // (blocks_included_height + MAX_SUB_SLOT_BLOCKS) / SUB_EPOCH_BLOCKS <= 1 selects the first-epoch branch.
    let blocks_included_height = 2u32;
    let prev_prev = ses_block(
        blocks_included_height - 2,
        SubEpochSummary {
            prev_subepoch_summary_hash: Bytes32::default(),
            reward_chain_hash: Bytes32::default(),
            num_blocks_overflow: 0,
            new_difficulty: None,
            new_sub_slot_iters: None,
        },
        vec![Bytes32::default()],
    );
    let ses = make_sub_epoch_summary(
        &MAINNET,
        &blocks,
        blocks_included_height,
        &prev_prev,
        None,
        None,
    )
    .expect("first-sub-epoch summary");
    assert_eq!(ses.prev_subepoch_summary_hash, MAINNET.genesis_challenge);
    assert_eq!(ses.reward_chain_hash, MAINNET.genesis_challenge);
    assert_eq!(ses.num_blocks_overflow, 0);
    assert_eq!(ses.new_difficulty, None);
    assert_eq!(ses.new_sub_slot_iters, None);
}
