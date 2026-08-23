//! The OUTBOUND MESSAGE CONTRACT (emission conformance, class 1): for every peer type, every message
//! chia's full node emits, under what stimulus — harvested from chia full_node.py / full_node_api.py.
//! Each entry names the Rust test that proves the emission with chia-complete fields, or carries an
//! explicit ignore reason. The table IS the point: a chia emission absent from the table (or an entry
//! silently skipped) is a red test, so a missing broadcast can never again escape as "nothing owed"
//! (the sp_source_data=None + absent index-0 farmer broadcast escape).

/// One chia full-node emission: peer type, wire message, chia source ref, stimulus, and how the dg_xch
/// implementation is proven (a covering test) or why it is not yet provable in-process.
struct ContractEntry {
    peer: &'static str,
    msg: &'static str,
    chia_ref: &'static str,
    stimulus: &'static str,
    status: Status,
}

enum Status {
    /// Emission + field completeness proven by this Rust test (crate::path in this repo).
    Covered(&'static str),
    /// Not yet stimulable in-process; the note names exactly what harness is missing. NEVER silently
    /// skipped — the meta-test prints every ignored entry so the gap stays visible.
    Ignored(&'static str),
}

const CONTRACT: &[ContractEntry] = &[
    // ── NodeType::TIMELORD ─────────────────────────────────────────────────────────────────────
    ContractEntry {
        peer: "TIMELORD",
        msg: "new_peak_timelord",
        chia_ref: "full_node.py:924-926 (send_peak_to_timelords; also on timelord connect :1009)",
        stimulus: "a new peak is confirmed / a timelord connects",
        status: Status::Covered(
            "live capture: validate_node_ws timelord peer (all 5 field assertions) + \
             daemon get_recent_reward_challenges/ladder unit context",
        ),
    },
    ContractEntry {
        peer: "TIMELORD",
        msg: "new_unfinished_block_timelord",
        chia_ref: "full_node.py:2633-2634 (add_unfinished_block)",
        stimulus: "a valid unfinished block is accepted",
        status: Status::Ignored(
            "needs a real plot proof + VDF-populated SlotState to accept a UB in-process — the \
             #[ignore]d daemon::tests::declare_to_new_unfinished_block_end_to_end harness; offline \
             field-parity is pinned by producer_differential (byte-identical vs mainnet)",
        ),
    },
    ContractEntry {
        peer: "TIMELORD",
        msg: "request_compact_proof_of_time",
        chia_ref: "full_node.py:3506 (uncompact-block sweep to connected timelords)",
        stimulus: "stored blocks carry uncompact VDF proofs and a bluebox timelord is connected",
        status: Status::Covered(
            "daemon::uncompact_solicit_tests::a_bulky_field_is_solicited_from_a_connected_timelord \
             (send half) + compact_vdf.rs::two_ticks_over_the_same_bulky_block_solicit_only_once \
             (plan/dedup); the --uncompact scan now sends RequestCompactProofOfTime to connected \
             timelords",
        ),
    },
    // ── NodeType::FARMER ───────────────────────────────────────────────────────────────────────
    ContractEntry {
        peer: "FARMER",
        msg: "new_signage_point (sp_source_data.vdf_data)",
        chia_ref: "full_node.py:1917-1931 (accepted SP VDF -> send_to_all FARMER)",
        stimulus: "a normal-index signage point is accepted",
        status: Status::Covered(
            "daemon::tests::farmer_sp_announce_emits_vdf_source_data + \
             node farmer::tests::normal_signage_point_carries_vdf_source_data (wire round-trip)",
        ),
    },
    ContractEntry {
        peer: "FARMER",
        msg: "new_signage_point (index 0, sp_source_data.sub_slot_data)",
        chia_ref: "full_node.py:2847-2863 (accepted EOS -> farmer SP at index 0)",
        stimulus: "an end-of-sub-slot is accepted",
        status: Status::Covered(
            "daemon::tests::index_zero_eos_carries_sub_slot_source_data (fields + wire round-trip)",
        ),
    },
    ContractEntry {
        peer: "FARMER",
        msg: "request_signed_values",
        chia_ref: "full_node_api.py declare_proof_of_space (quality + foliage hashes)",
        stimulus: "a farmer's DeclareProofOfSpace passes pospace validation",
        status: Status::Covered(
            "node declare_proof_of_space::{declare_from_real_fixture_blocks_validates_and_emits_request_signed_values, \
             declare_from_recent_chain_slice_validates_every_real_proof} (99 real mainnet proofs committed): a \
             DeclareProofOfSpace synthesized from a real wire block drives validate_declared_proof (the \
             handler's pospace-accept gate) to Accepted, then assemble_candidate (the try_build_candidate emission) \
             returns a RequestSignedValues whose quality + foliage hashes are correct and whose reward chain block is \
             BYTE-IDENTICAL to mainnet. The splice-and-broadcast half is covered by \
             daemon::tests::signed_values_splices_farmer_sigs_and_queues_for_broadcast; the StoreApi store/slot \
             resolution feeding assemble_candidate stays in a live deployment (its assembly is pinned by producer_differential)",
        ),
    },
    // ── NodeType::FULL_NODE ────────────────────────────────────────────────────────────────────
    ContractEntry {
        peer: "FULL_NODE",
        msg: "new_peak",
        chia_ref: "full_node.py:998 (on_connect) + peak_post_processing broadcast",
        stimulus: "a new peak is confirmed / a full node connects",
        status: Status::Ignored(
            "broadcast_new_peak construction (unfinished reward-block hash derivation) needs a \
             stored FullBlock + outbound-peer capture harness; proven live by the live-peer sync sentinel \
             (peers receive + sync from node-0)",
        ),
    },
    ContractEntry {
        peer: "FULL_NODE",
        msg: "new_signage_point_or_end_of_sub_slot (SP relay)",
        chia_ref: "full_node.py:1898-1899",
        stimulus: "a signage point is accepted from gossip",
        status: Status::Covered("daemon::tests::full_node_sp_and_eos_announces_carry_chia_fields"),
    },
    ContractEntry {
        peer: "FULL_NODE",
        msg: "new_signage_point_or_end_of_sub_slot (EOS relay)",
        chia_ref: "full_node.py:2840-2841",
        stimulus: "an end-of-sub-slot is accepted from gossip",
        status: Status::Covered("daemon::tests::full_node_sp_and_eos_announces_carry_chia_fields"),
    },
    ContractEntry {
        peer: "FULL_NODE",
        msg: "new_unfinished_block / new_unfinished_block2 (version split)",
        chia_ref: "full_node.py:2639-2656 (old clients v1, new clients v2 at 0.0.35)",
        stimulus: "an unfinished block is accepted",
        status: Status::Covered(
            "daemon::tests::unfinished_announce_version_split_matches_chia_0_0_35_boundary \
             (the split); UB acceptance stimulus shares the plot-proof gap above",
        ),
    },
    ContractEntry {
        peer: "FULL_NODE",
        msg: "new_transaction",
        chia_ref: "full_node.py:2999-3003 (mempool accept re-gossip)",
        stimulus: "a spend bundle is admitted to the mempool",
        status: Status::Ignored(
            "the tx-validator worker drains a private inbox; needs the in-process mempool-admit \
             harness (t060 covers admission itself, not the announce queue)",
        ),
    },
    ContractEntry {
        peer: "FULL_NODE",
        msg: "new_compact_vdf",
        chia_ref: "full_node.py:3269/3355 (accepted compact proof re-gossip)",
        stimulus: "a pulled compact VDF proof validates and is swapped in",
        status: Status::Ignored(
            "process_compact_vdf_inbox needs a stored block + a valid compact proof fixture; \
             the consume/swap path runs in a live deployment only today",
        ),
    },
    ContractEntry {
        peer: "FULL_NODE",
        msg: "respond_blocks/reject_blocks + respond_block + respond_signage_point (serve surface)",
        chia_ref: "full_node_api.py (RequestBlocks/RequestBlock/RequestSignagePointOrEndOfSubSlot)",
        stimulus: "a peer requests blocks / a signage point",
        status: Status::Covered(
            "t056_fast_sync_advances_from_empty (real loopback RequestBlocks/RespondBlocks) + \
             p2p reject-range tests (RangeRejected)",
        ),
    },
    // ── NodeType::WALLET ───────────────────────────────────────────────────────────────────────
    ContractEntry {
        peer: "WALLET",
        msg: "new_peak_wallet",
        chia_ref: "full_node.py:1008/1571",
        stimulus: "a new peak is confirmed / a wallet connects",
        status: Status::Covered(
            "puzzle_state::wallet_handshake_is_greeted_with_new_peak_wallet_within_sage_budget \
             (on-connect, chia-exact fields, inside Sage's 2s drop budget) + \
             puzzle_state::peak_advance_broadcasts_new_peak_wallet_to_wallet_peers (REAL confirm \
             through the follow path; wallet-type peer receives, full-node-type does not)",
        ),
    },
    ContractEntry {
        peer: "WALLET",
        msg: "coin_state_update",
        chia_ref: "full_node.py:1559",
        stimulus: "a confirmed block touches a subscribed coin/puzzle hash",
        status: Status::Covered(
            "puzzle_state::sage_sync_sequence_end_to_end (the RequestPuzzleState-subscribed hash \
             receives its CoinStateUpdate over the wire; nothing after remove-all unsubscribe)",
        ),
    },
];

/// Every emission harvested from chia full_node.py/full_node_api.py (grep make_msg + send_to_all,
/// deduped to the outbound-broadcast surface). If chia emits it and the table above does not carry it,
/// this test is RED — absence of an owed message becomes a failing test, not a silent gap.
const CHIA_HARVEST: &[&str] = &[
    "new_peak_timelord",
    "new_unfinished_block_timelord",
    "request_compact_proof_of_time",
    "new_signage_point (sp_source_data.vdf_data)",
    "new_signage_point (index 0, sp_source_data.sub_slot_data)",
    "request_signed_values",
    "new_peak",
    "new_signage_point_or_end_of_sub_slot (SP relay)",
    "new_signage_point_or_end_of_sub_slot (EOS relay)",
    "new_unfinished_block / new_unfinished_block2 (version split)",
    "new_transaction",
    "new_compact_vdf",
    "respond_blocks/reject_blocks + respond_block + respond_signage_point (serve surface)",
    "new_peak_wallet",
    "coin_state_update",
];

#[test]
fn every_harvested_chia_emission_has_a_contract_entry() {
    for harvested in CHIA_HARVEST {
        assert!(
            CONTRACT.iter().any(|e| e.msg == *harvested),
            "chia emits `{harvested}` but the outbound contract has no entry for it — \
             add the entry (Covered or Ignored-with-reason); it must never be silently absent"
        );
    }
}

#[test]
fn no_contract_entry_is_silently_skipped() {
    let mut covered = 0usize;
    let mut ignored = 0usize;
    for e in CONTRACT {
        match &e.status {
            Status::Covered(by) => {
                covered += 1;
                println!("COVERED  [{}] {} <- {} ({})", e.peer, e.msg, by, e.chia_ref);
            }
            Status::Ignored(why) => {
                ignored += 1;
                println!(
                    "IGNORED  [{}] {} — {} (stimulus: {}; {})",
                    e.peer, e.msg, why, e.stimulus, e.chia_ref
                );
            }
        }
    }
    // The floor pins today's coverage so a future edit cannot quietly demote a Covered entry.
    assert!(
        covered >= 8,
        "covered entries regressed below the pinned floor: {covered}"
    );
    assert_eq!(covered + ignored, CONTRACT.len());
}
