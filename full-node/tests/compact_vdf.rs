//! Red-first coverage for the compact-VDF (bluebox) pipeline against a REAL stored mainnet block
//! (height 5,000,000, `full_block_5000000.json`). That block carries a mix of proof forms:
//! its CC_IP proof is already compact (`witness_type == 0`, `normalized_to_identity == true`),
//! its CC_SP proof is still bulky (`witness_type == 5`) — so one fixture exercises both the
//! serve-a-compact-proof arm and the stay-silent-on-a-bulky-proof arm.
//!
//! Chia oracle: `chia/full_node/full_node.py` `request_compact_vdf` (serve),
//! `_needs_compact_proof`, `_can_accept_compact_proof`, `_replace_proof`.

mod common;

use dg_xch_core::blockchain::unsized_bytes::UnsizedBytes;
use dg_xch_core::blockchain::vdf_proof::VdfProof;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::protocols::timelord::RequestCompactProofOfTime;
use dg_xch_node::compact_vdf::{
    SolicitLedger, can_accept_compact_proof, needs_compact_proof, plan_block_solicitations,
    replace_proof, serve_compact, uncompact_fields, validate_compact_proof,
};
use std::time::{Duration, Instant};

const CC_SP: u8 = 3; // CompressibleVDFField::CC_SP_VDF — bulky in this fixture
const CC_IP: u8 = 4; // CompressibleVDFField::CC_IP_VDF — already compact in this fixture

fn cc_ip_vdf(
    b: &dg_xch_core::blockchain::full_block::FullBlock,
) -> dg_xch_core::blockchain::vdf_info::VdfInfo {
    b.reward_chain_block.challenge_chain_ip_vdf
}
fn cc_sp_vdf(
    b: &dg_xch_core::blockchain::full_block::FullBlock,
) -> dg_xch_core::blockchain::vdf_info::VdfInfo {
    b.reward_chain_block
        .challenge_chain_sp_vdf
        .expect("fixture block has a CC_SP vdf")
}

// ---- SERVE arm (chia request_compact_vdf) ---------------------------------------------------

#[test]
fn serve_returns_our_proof_when_stored_field_is_already_compact() {
    let b = common::full_block();
    let served = serve_compact(&b, CC_IP, &cc_ip_vdf(&b));
    assert_eq!(
        served.as_ref(),
        Some(&b.challenge_chain_ip_proof),
        "a peer asking for our CC_IP proof, which we hold compact, must get exactly that proof"
    );
}

#[test]
fn serve_stays_silent_when_our_stored_field_is_still_bulky() {
    let b = common::full_block();
    assert!(
        serve_compact(&b, CC_SP, &cc_sp_vdf(&b)).is_none(),
        "we have not compressed our CC_SP proof ourselves; we must answer nothing"
    );
}

#[test]
fn serve_stays_silent_on_a_field_vdf_info_mismatch() {
    let b = common::full_block();
    // Right field, wrong VdfInfo (the CC_SP one) — no match, so nothing is served.
    assert!(serve_compact(&b, CC_IP, &cc_sp_vdf(&b)).is_none());
    // Unknown field byte is silently ignored, never a panic.
    assert!(serve_compact(&b, 9, &cc_ip_vdf(&b)).is_none());
}

// ---- needs_compact_proof (chia _needs_compact_proof) ----------------------------------------

#[test]
fn needs_compact_proof_true_for_bulky_field_false_for_compact_field() {
    let b = common::full_block();
    assert!(
        needs_compact_proof(&b, CC_SP, &cc_sp_vdf(&b)),
        "CC_SP is bulky — a compact replacement is wanted"
    );
    assert!(
        !needs_compact_proof(&b, CC_IP, &cc_ip_vdf(&b)),
        "CC_IP is already compact — no replacement wanted"
    );
    assert!(
        !needs_compact_proof(&b, CC_SP, &cc_ip_vdf(&b)),
        "VdfInfo mismatch — nothing to replace"
    );
}

// ---- validate_compact_proof (chia validate_vdf from the default element) --------------------

#[test]
fn validate_accepts_the_real_compact_proof_and_rejects_a_tampered_one() {
    let b = common::full_block();
    let vdf = cc_ip_vdf(&b);
    let good = b.challenge_chain_ip_proof.clone();
    assert!(
        validate_compact_proof(&MAINNET, &vdf, &good),
        "the block's genuine compact CC_IP proof must validate from the default element"
    );

    // Flip one witness byte: the Wesolowski verify must reject, never panic.
    let mut witness = good.witness.bytes.clone();
    assert!(!witness.is_empty(), "compact proof still carries a witness");
    witness[0] ^= 0xFF;
    let tampered = VdfProof {
        witness: UnsizedBytes { bytes: witness },
        ..good
    };
    assert!(
        !validate_compact_proof(&MAINNET, &vdf, &tampered),
        "a tampered compact proof must not validate"
    );
}

#[test]
fn validate_rejects_a_non_compact_proof_without_running_the_vdf() {
    let b = common::full_block();
    // The CC_SP proof is bulky; validate short-circuits on the compactness predicate.
    assert!(!validate_compact_proof(
        &MAINNET,
        &cc_sp_vdf(&b),
        b.challenge_chain_sp_proof.as_ref().expect("cc_sp proof")
    ));
}

// ---- can_accept_compact_proof guards (chia _can_accept_compact_proof) ------------------------

#[test]
fn can_accept_rejects_a_too_recent_block() {
    let b = common::full_block();
    let h = b.height();
    // Peak only 2 above the block — chia will not compactify a block within 5 of the peak.
    assert!(!can_accept_compact_proof(
        &MAINNET,
        &b,
        CC_IP,
        &cc_ip_vdf(&b),
        &b.challenge_chain_ip_proof,
        h + 2,
        h,
    ));
}

#[test]
fn can_accept_rejects_a_non_compact_offered_proof() {
    let b = common::full_block();
    let h = b.height();
    let bulky = b.challenge_chain_sp_proof.clone().expect("cc_sp proof");
    assert!(!can_accept_compact_proof(
        &MAINNET,
        &b,
        CC_SP,
        &cc_sp_vdf(&b),
        &bulky,
        h + 100,
        h,
    ));
}

#[test]
fn can_accept_rejects_a_duplicate_when_the_field_is_already_compact() {
    let b = common::full_block();
    let h = b.height();
    // Offer the block's own already-compact CC_IP proof back: it validates, but the field is not
    // bulky, so needs_compact_proof is false and admission is refused (chia "Duplicate compact proof").
    assert!(!can_accept_compact_proof(
        &MAINNET,
        &b,
        CC_IP,
        &cc_ip_vdf(&b),
        &b.challenge_chain_ip_proof,
        h + 100,
        h,
    ));
}

// ---- replace_proof mechanics (chia _replace_proof) ------------------------------------------
// The ACCEPT-AND-REPLACE happy path (can_accept => true, then replace) cannot be exercised
// offline: it needs a genuine normalized-to-identity CC_SP proof that both validates AND matches
// the stored VdfInfo, which is only produced by a live bluebox timelord. That end-to-end path is
// gated live (see report). Here we prove the pure swap mechanics, which the live path relies on.

#[test]
fn replace_swaps_only_the_named_field_and_preserves_the_header_hash() {
    let b = common::full_block();
    let sentinel = VdfProof {
        witness_type: 0,
        witness: UnsizedBytes {
            bytes: vec![0xAB; 8],
        },
        normalized_to_identity: true,
    };
    let nb = replace_proof(&b, CC_SP, &cc_sp_vdf(&b), &sentinel).expect("CC_SP field matches");
    assert_eq!(
        nb.challenge_chain_sp_proof.as_ref(),
        Some(&sentinel),
        "the CC_SP proof is swapped"
    );
    assert_eq!(
        nb.challenge_chain_ip_proof, b.challenge_chain_ip_proof,
        "no other proof field is touched"
    );
    assert_eq!(
        nb.header_hash().expect("hash"),
        b.header_hash().expect("hash"),
        "swapping a witness leaves the block identity unchanged"
    );
}

// ---- uncompact_fields (chia broadcast_uncompact_blocks enumeration) -------------------------

#[test]
fn uncompact_fields_lists_only_the_bulky_cc_sp_field_of_this_block() {
    let b = common::full_block();
    let fields = uncompact_fields(&b);
    // Block 5,000,000 has no finished sub slots; CC_IP is compact, CC_SP is bulky.
    assert_eq!(fields.len(), 1, "exactly one bulky field");
    assert_eq!(fields[0].0, CC_SP, "the bulky field is CC_SP");
    assert_eq!(
        fields[0].1,
        cc_sp_vdf(&b),
        "keyed on the CC_SP start VdfInfo"
    );
}

#[test]
fn replace_returns_none_on_a_vdf_info_mismatch() {
    let b = common::full_block();
    let sentinel = VdfProof {
        witness_type: 0,
        witness: UnsizedBytes::default(),
        normalized_to_identity: true,
    };
    // CC_SP field but the CC_IP VdfInfo: no match, no swap.
    assert!(replace_proof(&b, CC_SP, &cc_ip_vdf(&b), &sentinel).is_none());
}

// ---- plan_block_solicitations + SolicitLedger (chia broadcast_uncompact_blocks + our re-solicit
// ---- suppression) --------------------------------------------------------------------------

#[test]
fn a_bulky_block_plans_exactly_its_uncompact_field_as_a_request() {
    let b = common::full_block();
    let hh = b.header_hash().expect("hash");
    let mut ledger = SolicitLedger::new(1024, Duration::from_secs(3600));
    let now = Instant::now();

    let reqs = plan_block_solicitations(&b, hh, b.height(), &mut ledger, now);
    assert_eq!(reqs.len(), 1, "one bulky field ⇒ one request");
    let req = &reqs[0];
    assert_eq!(req.field_vdf, CC_SP, "the bulky field is CC_SP");
    assert_eq!(req.header_hash, hh);
    assert_eq!(req.height, b.height());
    assert_eq!(
        req.new_proof_of_time,
        cc_sp_vdf(&b),
        "keyed on the CC_SP start VdfInfo — exactly what a bluebox needs"
    );
}

// TEST 2 — the dedup: two scan ticks over the SAME bulky block, same ledger, within the ttl, plan
// the request ONCE. Without the ledger a fixed-window scan would re-solicit the same field every
// tick and spam a connected bluebox with duplicate work.
#[test]
fn two_ticks_over_the_same_bulky_block_solicit_only_once() {
    let b = common::full_block();
    let hh = b.header_hash().expect("hash");
    let ttl = Duration::from_secs(3600);
    let mut ledger = SolicitLedger::new(1024, ttl);
    let t0 = Instant::now();

    let first = plan_block_solicitations(&b, hh, b.height(), &mut ledger, t0);
    assert_eq!(first.len(), 1, "first tick solicits the bulky field");

    // Second tick a minute later (still within ttl): suppressed.
    let second = plan_block_solicitations(
        &b,
        hh,
        b.height(),
        &mut ledger,
        t0 + Duration::from_secs(60),
    );
    assert!(
        second.is_empty(),
        "the same field within ttl is not re-solicited"
    );
    assert_eq!(ledger.len(), 1, "ledger holds the one solicited field");

    // After the ttl elapses the field — still bulky, never compacted (no bluebox answered) — is
    // retried, not abandoned forever.
    let later = plan_block_solicitations(
        &b,
        hh,
        b.height(),
        &mut ledger,
        t0 + ttl + Duration::from_secs(1),
    );
    assert_eq!(
        later.len(),
        1,
        "past ttl the still-bulky field is re-solicited"
    );
}

#[test]
fn the_ledger_is_capacity_bounded_and_never_panics() {
    // A tiny cap forces eviction; admit must stay bounded and keep working (an evicted stale key
    // simply becomes solicitable again — safe).
    let mut ledger = SolicitLedger::new(4, Duration::from_secs(3600));
    let now = Instant::now();
    for tag in 0u8..50 {
        let req = RequestCompactProofOfTime {
            new_proof_of_time: dg_xch_core::blockchain::vdf_info::VdfInfo {
                challenge: dg_xch_core::blockchain::sized_bytes::Bytes32::from([tag; 32]),
                number_of_iterations: u64::from(tag),
                output: dg_xch_core::blockchain::class_group_element::ClassgroupElement::get_default_element(),
            },
            header_hash: dg_xch_core::blockchain::sized_bytes::Bytes32::from([tag ^ 0x5A; 32]),
            height: 100 + u32::from(tag),
            field_vdf: 3,
        };
        assert!(ledger.admit(&req, now), "each distinct field admits once");
    }
    assert!(ledger.len() <= 4, "capacity bound holds: {}", ledger.len());
}
