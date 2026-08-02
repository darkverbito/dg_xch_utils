//! Mainnet hash-exact cross-check for the promoted `make_sub_epoch_summary` in `dg_xch_core`.
//!
//! A real mainnet weight proof (tip height 9,054,698) carries one [`SubEpochData`] per sub-epoch. Chained
//! genesis-anchored (each summary links the previous by hash), these reconstruct the *actual on-chain*
//! [`SubEpochSummary`] sequence — the same reconstruction the weight-proof verifier proves terminates in
//! the sub-epoch-summary hash mainnet committed in its recent chain (see `weight_proof_parity.rs`, which
//! pins the first/last SES hashes as golden scalars).
//!
//! This test then drives the *promoted* [`make_sub_epoch_summary`] over block records carrying those real
//! previous summaries and reward-chain hashes, and asserts the summary it reconstructs hashes to mainnet's
//! real on-chain SES hash. Because `make_sub_epoch_summary` derives `prev_subepoch_summary_hash` itself by
//! hashing the previous summary (it is not fed in), the match proves the promoted function reproduces
//! chia's exact field assembly and streamable hashing — a hash-exact proof, thousands of times over.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_output::VdfOutput;
use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::make_sub_epoch_summary::make_sub_epoch_summary;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};

fn load_fixture() -> WeightProof {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/weight_proof_mainnet_9054698.bin");
    let data = std::fs::read(path).expect("mainnet weight-proof fixture present");
    let mut cur = Cursor::new(data.as_slice());
    WeightProof::from_bytes(&mut cur, ChiaProtocolVersion::default())
        .expect("real mainnet weight proof deserializes")
}

fn empty_vdf() -> VdfOutput {
    VdfOutput {
        data: UnsizedBytes::new(vec![]),
    }
}

/// A block record carrying the fields `make_sub_epoch_summary` reads: `height`, `prev_hash`, the included
/// previous summary, and the finished reward-slot hashes. Every other field is a valid placeholder the
/// reconstruction never inspects.
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

/// Genesis-anchored reconstruction of the real on-chain sub-epoch-summary sequence from the proof's
/// public `sub_epochs`, mirroring chia's `_map_sub_epoch_summaries`. This is the verifier's oracle side —
/// it is exactly the chain `weight_proof_parity.rs` proves ends in mainnet's committed SES hash.
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

/// Every real sub-epoch summary (past the genesis-anchored first) is reconstructed hash-exact by the
/// promoted `make_sub_epoch_summary`. The previous summary is supplied as the included summary on the
/// prev-sub-epoch-summary block; `make_sub_epoch_summary` re-derives `prev_subepoch_summary_hash` by
/// hashing it, `reward_chain_hash` from the block's last finished reward-slot hash, and
/// `num_blocks_overflow` from that block's height mod `SUB_EPOCH_BLOCKS` — so a match proves the full
/// field-assembly-and-hash path against mainnet.
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

/// The genesis-anchored first-sub-epoch branch: when `blocks_included_height` still falls inside the first
/// sub-epoch, the summary is fixed to the genesis-challenge anchors with no retarget, per chia.
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
