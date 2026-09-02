mod common;

use std::collections::HashMap;

use common::load_fixture;
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::full_block_to_block_record::header_block_to_sub_block_record;
use dg_xch_core::consensus::get_block_challenge::get_block_challenge;
use dg_xch_core::consensus::pot_iterations::is_overflow_block;

#[test]
fn get_block_challenge_matches_mainnet_committed_challenges() {
    let wp = load_fixture();
    // The promoted `get_block_challenge` only walks ancestors when the block has NO finished sub-slots; the
    // finished-sub-slot branch is self-contained, so an empty provider suffices for the blocks checked here.
    let empty: HashMap<Bytes32, BlockRecord> = HashMap::new();

    let mut matched = 0usize;
    let mut checked_overflow_eos = 0usize;
    for block in &wp.recent_chain_data {
        if block.finished_sub_slots.is_empty() {
            continue; // ancestor-walk branch; needs the populated recent-chain cache (covered by phase 5)
        }
        let sp_index = block.reward_chain_block.signage_point_index;
        let overflow = is_overflow_block(&MAINNET, sp_index).expect("overflow determination");
        // All recent-chain blocks are non-genesis (tip ~9.05M).
        let challenge = get_block_challenge(
            &MAINNET,
            &block.finished_sub_slots,
            block.prev_header_hash(),
            &empty,
            false,
            overflow,
            false,
        )
        .expect("get_block_challenge on a finished-sub-slot block");
        assert_eq!(
            challenge,
            block.reward_chain_block.pos_ss_cc_challenge_hash,
            "get_block_challenge must reproduce the on-chain committed cc challenge at height {}",
            block.height()
        );
        matched += 1;
        if overflow {
            checked_overflow_eos += 1;
        }
    }

    // Most recent blocks share their predecessor's sub-slot (no finished sub-slots) and take the
    // ancestor-walk branch, covered by the phase-5 recent-chain validator; the self-contained
    // finished-sub-slot branch is exercised by the ~20 blocks that open a new sub-slot.
    assert!(
        matched > 15,
        "expected the finished-sub-slot blocks to cross-check, got {matched}"
    );
    eprintln!(
        "get_block_challenge: {matched} mainnet committed challenges reproduced hash-exact \
         ({checked_overflow_eos} via the overflow end-of-slot-VDF branch)"
    );
}

// Tampering the last challenge-chain sub-slot must change the derived challenge.
#[test]
fn get_block_challenge_rejects_tampered_challenge_sub_slot() {
    let wp = load_fixture();
    let empty: HashMap<Bytes32, BlockRecord> = HashMap::new();

    // Find a non-overflow finished-sub-slot block (its challenge is the plain hash of the last cc sub-slot).
    let mut tampered = false;
    for block in &wp.recent_chain_data {
        if block.finished_sub_slots.is_empty() {
            continue;
        }
        let sp_index = block.reward_chain_block.signage_point_index;
        if is_overflow_block(&MAINNET, sp_index).expect("overflow") {
            continue;
        }
        let mut bad = block.clone();
        let last = bad.finished_sub_slots.last_mut().unwrap();
        // Flip a committed field of the challenge-chain sub-slot; its hash — hence the challenge — changes.
        last.challenge_chain.new_difficulty =
            Some(last.challenge_chain.new_difficulty.unwrap_or(0) ^ 0xDEAD_BEEF);
        let challenge = get_block_challenge(
            &MAINNET,
            &bad.finished_sub_slots,
            bad.prev_header_hash(),
            &empty,
            false,
            false,
            false,
        )
        .expect("get_block_challenge on tampered block");
        assert_ne!(
            challenge, block.reward_chain_block.pos_ss_cc_challenge_hash,
            "tampering the last challenge-chain sub-slot must change the derived challenge"
        );
        tampered = true;
        break;
    }
    assert!(
        tampered,
        "expected at least one non-overflow finished-sub-slot block to tamper"
    );
}

// header_block_to_sub_block_record must reproduce the on-chain hash linkage across the real recent chain.
#[test]
fn header_block_to_sub_block_record_links_mainnet_recent_chain() {
    let wp = load_fixture();
    let chain = &wp.recent_chain_data;
    assert!(chain.len() > 100, "fixture recent chain present");

    // Build a record for each recent-chain block. The fields asserted below (hashes, height, weight,
    // total_iters, linkage) do not depend on required_iters/deficit/overflow/ssi/ses, so plausible
    // placeholders are used for those; the record-linkage and hash derivations are what is under test.
    let records: Vec<BlockRecord> = chain
        .iter()
        .map(|block| {
            header_block_to_sub_block_record(
                &MAINNET,
                0,
                block,
                MAINNET.sub_slot_iters_starting,
                false,
                0,
                block.height(),
                None,
            )
            .expect("header_block_to_sub_block_record")
        })
        .collect();

    // Per-block hash/field consistency against the source HeaderBlock.
    for (block, rec) in chain.iter().zip(records.iter()) {
        assert_eq!(rec.header_hash, block.header_hash().expect("header hash"));
        assert_eq!(rec.prev_hash, block.prev_header_hash());
        assert_eq!(rec.height, block.height());
        assert_eq!(rec.weight, block.weight());
        assert_eq!(rec.total_iters, block.total_iters());
        // The two nontrivial derived hashes reproduce the inherent type hashes.
        assert_eq!(
            rec.reward_infusion_new_challenge,
            block.reward_chain_block.hash().expect("rcb hash")
        );
        // A finished-sub-slot block records one challenge-slot hash per finished sub-slot.
        if !block.finished_sub_slots.is_empty() {
            let fcsh = rec
                .finished_challenge_slot_hashes
                .as_ref()
                .expect("finished challenge slot hashes present");
            assert_eq!(fcsh.len(), block.finished_sub_slots.len());
            for (h, ss) in fcsh.iter().zip(block.finished_sub_slots.iter()) {
                assert_eq!(*h, ss.challenge_chain.hash().expect("cc hash"));
            }
        }
    }

    // Hash-linkage across the real chain: consecutive blocks (block[i+1] links block[i]) must have
    // record[i].header_hash == record[i+1].prev_hash.
    let mut links = 0usize;
    for i in 0..chain.len() - 1 {
        if chain[i + 1].prev_header_hash() == chain[i].header_hash().expect("header hash") {
            assert_eq!(
                records[i].header_hash,
                records[i + 1].prev_hash,
                "record linkage must follow the on-chain header-hash link at height {}",
                chain[i].height()
            );
            links += 1;
        }
    }
    assert!(
        links > 100,
        "expected a long contiguously-linked run in the recent chain, got {links}"
    );
    eprintln!(
        "header_block_to_sub_block_record: {} records built, {links} on-chain hash links reproduced",
        records.len()
    );
}
