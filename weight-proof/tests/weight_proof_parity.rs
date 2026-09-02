// Parity and tamper tests using a committed mainnet weight proof.

mod common;

use common::{fixtures_dir, load_fixture};
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::utils::hash_256;
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use dg_xch_weight_proof::{WeightProofError, validate_weight_proof};

// --- Golden scalars, verified against chia's reference (wp_reference.py, mainnet genesis pinned). ---
const GOLDEN_SUB_EPOCHS: usize = 23_579;
const GOLDEN_SEGMENTS: usize = 236;
const GOLDEN_RECENT_BLOCKS: usize = 722;
const GOLDEN_FIRST_SES_HASH: &str =
    "6c6c5401c7b912fafbc7c99b7ef6d469ba0e349616766e006f0e588ae0d7057b";
const GOLDEN_LAST_SES_HASH: &str =
    "b70b1e645bd16bbde565dd595250f5c1cc944e9e75495ef80a25f09f5c554593";

/// The reference's `std_hash(ses)` is sha256 over the streamable bytes of the SubEpochSummary.
fn ses_hash_hex(ses: &SubEpochSummary) -> String {
    hex::encode(hash_256(
        ses.to_bytes(ChiaProtocolVersion::default())
            .expect("ses serializes"),
    ))
}

fn ses_hash_bytes(ses: &SubEpochSummary) -> Bytes32 {
    Bytes32::from(hash_256(
        ses.to_bytes(ChiaProtocolVersion::default())
            .expect("ses serializes"),
    ))
}

/// Independently reconstruct the ses-hash chain from the PUBLIC `sub_epochs` (mirroring the reference's
/// `_map_sub_epoch_summaries`), genesis-seeded. This is the verifier's own oracle-side computation — not
/// a call into the port's private phase-2 fn — so a bug in its reconstruction cannot hide a matching bug here.
fn reconstruct_hash_chain(wp: &WeightProof) -> Vec<String> {
    let mut prev = MAINNET.genesis_challenge;
    let mut out = Vec::with_capacity(wp.sub_epochs.len());
    for d in wp.sub_epochs.iter() {
        let ses = SubEpochSummary {
            prev_subepoch_summary_hash: prev,
            reward_chain_hash: d.reward_chain_hash,
            num_blocks_overflow: d.num_blocks_overflow,
            new_difficulty: d.new_difficulty,
            new_sub_slot_iters: d.new_sub_slot_iters,
        };
        out.push(ses_hash_hex(&ses));
        prev = ses_hash_bytes(&ses);
    }
    out
}

fn golden_hash_chain() -> Vec<String> {
    std::fs::read_to_string(fixtures_dir().join("weight_proof_mainnet_9054698.golden.hashes.txt"))
        .expect("golden hash chain present")
        .lines()
        .map(|l| l.trim().to_string())
        .collect()
}

/// RUNNABLE. Structural parity + the DoS-bound regression guard (23,579 sub-epochs must not trip the
/// MAX_SUB_EPOCHS bound the real vector forced up from 10_000 to 300_000).
#[test]
fn real_mainnet_proof_loads_with_reference_shape() {
    let wp = load_fixture();
    assert_eq!(wp.sub_epochs.len(), GOLDEN_SUB_EPOCHS, "sub_epochs count");
    assert_eq!(
        wp.sub_epoch_segments.len(),
        GOLDEN_SEGMENTS,
        "segments count"
    );
    assert_eq!(
        wp.recent_chain_data.len(),
        GOLDEN_RECENT_BLOCKS,
        "recent count"
    );
}

/// RUNNABLE. The committed golden is well-formed and matches the reference endpoints.
#[test]
fn golden_hash_chain_is_wellformed() {
    let chain = golden_hash_chain();
    assert_eq!(chain.len(), GOLDEN_SUB_EPOCHS, "golden chain length");
    assert_eq!(
        chain.first().unwrap(),
        GOLDEN_FIRST_SES_HASH,
        "first ses hash"
    );
    assert_eq!(chain.last().unwrap(), GOLDEN_LAST_SES_HASH, "last ses hash");
    for (i, h) in chain.iter().enumerate() {
        assert_eq!(h.len(), 64, "ses hash {i} is 32 bytes hex");
        assert!(hex::decode(h).is_ok(), "ses hash {i} valid hex");
    }
}

/// Reconstructed summary hashes match the reference fixture.
#[test]
fn phase2_full_chain_ses_hash_parity() {
    let wp = load_fixture();
    let got = reconstruct_hash_chain(&wp);
    let want = golden_hash_chain();
    assert_eq!(got.len(), want.len(), "chain length parity");
    let mut first_divergence = None;
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        if g != w {
            first_divergence = Some((i, g.clone(), w.clone()));
            break;
        }
    }
    assert!(
        first_divergence.is_none(),
        "ses-hash divergence vs reference at {first_divergence:?} \
         — dg_xch 5-field hashing differs from the reference format"
    );
}

/// A modified summary hash is rejected.
#[test]
fn tamper_phase2_corrupt_reward_chain_hash_rejects() {
    let mut wp = load_fixture();
    let k = GOLDEN_SUB_EPOCHS / 2;
    // Replace with a clearly-different hash.
    wp.sub_epochs[k].reward_chain_hash = Bytes32::from([0xABu8; 32]);
    match validate_weight_proof(&wp, &MAINNET) {
        Err(WeightProofError::Rejected(_)) => { /* correctly caught by the anchor check */ }
        Err(WeightProofError::PhaseUnimplemented("sub_epoch_sampling")) => panic!(
            "phase 2 accepted a proof with a corrupted reward_chain_hash \
             (it advanced to phase 1 instead of rejecting)"
        ),
        other => panic!("unexpected verdict on tampered proof: {other:?}"),
    }
}

/// PHASE 2 — TAMPER REJECT: a proof with fewer than two sub-epoch summaries must not validate
/// (ref: chia's `test_weight_proof_rejects_fewer_than_two_summaries`).
#[test]
fn tamper_phase2_fewer_than_two_summaries_rejects() {
    let mut wp = load_fixture();
    wp.sub_epochs.truncate(1);
    assert!(
        validate_weight_proof(&wp, &MAINNET).is_err(),
        "a proof with a single sub-epoch summary must be rejected"
    );
}

// ================================ PHASE 1 — SAMPLING (determinism) ================================
//
// The security property is that the sampled sub-epoch INDEX SET is fixed by the RNG (seed =
// summaries[-2].get_hash()), so a prover cannot cherry-pick a convenient subset. chia's reference
// (validate_sub_epoch_sampling) independently computes this exact set on the real fixture; it equals the
// set of sub-epochs the prover actually backed with segments. Golden (wp_reference.py, 995 RNG draws,
// MAX_SAMPLES=20): the 20 indices below, with sampled_equals_provided == true.
const GOLDEN_SAMPLED_SUB_EPOCHS: [u32; 20] = [
    1069, 2054, 2513, 3090, 3188, 4153, 4188, 4563, 5192, 5974, 6013, 6088, 7155, 7438, 7767, 8092,
    9518, 9594, 9701, 10591,
];

/// Distinct `sub_epoch_n` the prover backed with segments in the fixture.
fn provided_segment_sub_epochs(wp: &WeightProof) -> Vec<u32> {
    let mut v: Vec<u32> = wp
        .sub_epoch_segments
        .iter()
        .map(|s| s.sub_epoch_n)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// PHASE 1 — REFERENCE PARITY on the sampled INDEX SET. chia's RNG-computed sampled set equals the
/// fixture's provided-segment set element-for-element (both == GOLDEN_SAMPLED_SUB_EPOCHS). This pins the
/// determinism against the reference without depending on the port's private RNG.
#[test]
fn phase1_sampled_set_matches_reference() {
    let wp = load_fixture();
    assert_eq!(
        provided_segment_sub_epochs(&wp),
        GOLDEN_SAMPLED_SUB_EPOCHS.to_vec(),
        "provided-segment sub-epochs must equal chia's RNG-sampled set"
    );
}

/// PHASE 1 — EXACT-SET TAMPER (the strong gate). For EACH of the 20 sampled sub-epochs, stripping its
/// segments must make phase 1 REJECT (that sampled sub-epoch is now uncovered). Because the port accepts the
/// untampered proof (his set ⊆ provided) AND rejects when any one sampled sub-epoch is stripped (his set
/// ⊇ each), his sampled set equals the reference set exactly — a full bidirectional proof through the
/// public API, with no dependence on his private MT19937. A validator that under-sampled (missed an index)
/// would fail here: stripping that index's segments would NOT trigger a rejection.
#[test]
fn phase1_tamper_strip_each_sampled_subepoch_rejects() {
    for &target in GOLDEN_SAMPLED_SUB_EPOCHS.iter() {
        let mut wp = load_fixture();
        let before = wp.sub_epoch_segments.len();
        wp.sub_epoch_segments.retain(|s| s.sub_epoch_n != target);
        assert!(
            wp.sub_epoch_segments.len() < before,
            "sub_epoch_n {target} should have had segments to strip"
        );
        match validate_weight_proof(&wp, &MAINNET) {
            Err(WeightProofError::Rejected(_)) => { /* correctly rejected: uncovered sampled sub-epoch */
            }
            Err(WeightProofError::PhaseUnimplemented("summaries_weight")) => panic!(
                "UNDER-SAMPLING / FAIL-OPEN: stripping segments of sampled sub-epoch {target} did not \
                 cause phase 1 to reject — the validator did not require coverage of a sampled index"
            ),
            other => panic!("unexpected verdict for stripped sub-epoch {target}: {other:?}"),
        }
    }
}

// ============================== PHASE 3 — SUMMARIES WEIGHT ==============================
//
// The accumulated summary weight (total_weight, from phase 2) must equal the on-chain weight at the
// sub-epoch boundary block (height = (n_summaries-1)*SUB_EPOCH_BLOCKS + last_overflow - 1). Golden
// (wp_reference.py): total_weight and the boundary block weight agree at 55,604,764,512, height 9,053,977.
const GOLDEN_TOTAL_WEIGHT: u128 = 55_604_764_512;
const GOLDEN_SES_END_HEIGHT: u32 = 9_053_977;

fn boundary_block_idx(wp: &WeightProof) -> usize {
    wp.recent_chain_data
        .iter()
        .rposition(|b| b.reward_chain_block.height == GOLDEN_SES_END_HEIGHT)
        .expect("recent chain contains the sub-epoch boundary block")
}

/// PHASE 3 — REFERENCE PARITY. The on-chain weight at the sub-epoch boundary equals the reference's
/// accumulated summary weight (== the port's `total_weight`, which it threads from phase 2 and asserts here).
#[test]
fn phase3_boundary_weight_matches_reference() {
    let wp = load_fixture();
    let idx = boundary_block_idx(&wp);
    assert_eq!(
        wp.recent_chain_data[idx].reward_chain_block.weight, GOLDEN_TOTAL_WEIGHT,
        "boundary-block weight must equal the reference accumulated summary weight"
    );
}

/// PHASE 3 — INFLATE-A-WEIGHT TAMPER. Inflating the boundary block's on-chain weight by 1 must be caught
/// by the weight-equality check. A +1 is below the granularity that could shift phase-1 sampling (delta
/// changes by ~1e-11), so the rejection originates at phase 3 — asserted via its message. A validator that
/// skipped the equality check would fail-open and advance to phase 4.
#[test]
fn phase3_tamper_inflate_boundary_weight_rejects() {
    let mut wp = load_fixture();
    let idx = boundary_block_idx(&wp);
    wp.recent_chain_data[idx].reward_chain_block.weight += 1;
    match validate_weight_proof(&wp, &MAINNET) {
        Err(WeightProofError::Rejected(msg)) => assert!(
            msg.contains("summaries weight"),
            "expected the phase-3 weight-mismatch rejection, got: {msg}"
        ),
        Err(WeightProofError::PhaseUnimplemented("sub_epoch_segments")) => {
            panic!("FAIL-OPEN: phase 3 accepted a proof whose boundary weight was inflated by 1")
        }
        other => panic!("unexpected verdict on weight-inflated proof: {other:?}"),
    }
}

/// ENCODING-CONTRACT REGRESSION GUARD. Current mainnet's on-chain ses hash is over the 5-field summary,
/// which serializes to exactly 67 bytes. The newer 6th field `challenge_merkle_root` is not active
/// on mainnet and emits 0 bytes for None there; adding it to dg_xch as a standard `Option` (None -> a
/// trailing 0x00, 68 bytes) shifts every ses hash and false-rejects the real proof. This guard goes RED
/// the moment the type grows past 67 bytes for an all-None summary — re-add the field only gated on the
/// activating hard-fork height (see the note in sub_epoch_summary.rs).
#[test]
fn five_field_summary_is_67_bytes() {
    let z = Bytes32::from([0u8; 32]);
    let ses = SubEpochSummary {
        prev_subepoch_summary_hash: z,
        reward_chain_hash: z,
        num_blocks_overflow: 0,
        new_difficulty: None,
        new_sub_slot_iters: None,
    };
    let b = ses.to_bytes(ChiaProtocolVersion::default()).unwrap();
    assert_eq!(
        b.len(),
        67,
        "all-None summary must be 67 bytes to match mainnet's on-chain ses hash; got {} \
         (a 6th serialized field would break the phase-2 anchor check)",
        b.len()
    );
}
