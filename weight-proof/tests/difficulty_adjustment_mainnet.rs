// Mainnet cross-check for the difficulty-adjustment retarget math: every real on-chain retarget must
// satisfy the significant-bit truncation, NUM_SPS_SUB_SLOT rounding, and DIFFICULTY_CHANGE_MAX_FACTOR clamp.

mod common;

use common::load_fixture;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_core::consensus::difficulty_adjustment::{
    count_significant_bits, truncate_to_significant_bits,
};

#[test]
fn mainnet_retargets_match_significant_bits_and_rounding() {
    let wp = load_fixture();
    assert_eq!(
        wp.sub_epochs.len(),
        23_579,
        "expected the full mainnet SES chain"
    );

    let sig_bits = MAINNET.significant_bits;
    let num_sps = u128::from(MAINNET.num_sps_sub_slot);

    let mut diff_count = 0usize;
    let mut ssi_count = 0usize;

    // Consecutive on-chain difficulties / ssi (in epoch order) to check the per-epoch clamp factor.
    let mut prev_difficulty: Option<u64> = None;
    let mut prev_ssi: Option<u64> = None;
    let factor = u128::from(MAINNET.difficulty_change_max_factor);

    for se in &wp.sub_epochs {
        if let Some(d) = se.new_difficulty {
            diff_count += 1;
            let d128 = u128::from(d);
            // Every real difficulty carries at most SIGNIFICANT_BITS of precision, so truncating it is a
            // no-op — exactly the invariant `_get_next_difficulty` guarantees via truncate_to_significant_bits.
            assert!(
                count_significant_bits(d128) <= sig_bits,
                "difficulty {d} over sig bits"
            );
            assert_eq!(
                truncate_to_significant_bits(d128, sig_bits),
                d128,
                "difficulty {d} not already significant-bit truncated"
            );
            if let Some(p) = prev_difficulty {
                // Per-epoch change is clamped to DIFFICULTY_CHANGE_MAX_FACTOR in each direction.
                let (a, b) = (u128::from(p), d128);
                assert!(
                    b <= a * factor && a <= b * factor,
                    "difficulty {p} -> {d} exceeds {factor}x"
                );
            }
            prev_difficulty = Some(d);
        }
        if let Some(s) = se.new_sub_slot_iters {
            ssi_count += 1;
            let s128 = u128::from(s);
            assert!(
                count_significant_bits(s128) <= sig_bits,
                "ssi {s} over sig bits"
            );
            assert_eq!(
                truncate_to_significant_bits(s128, sig_bits),
                s128,
                "ssi {s} not truncated"
            );
            // `_get_next_sub_slot_iters` rounds down to a multiple of NUM_SPS_SUB_SLOT.
            assert_eq!(
                s128 % num_sps,
                0,
                "ssi {s} not a multiple of NUM_SPS_SUB_SLOT"
            );
            if let Some(p) = prev_ssi {
                let (a, b) = (u128::from(p), s128);
                assert!(
                    b <= a * factor && a <= b * factor,
                    "ssi {p} -> {s} exceeds {factor}x"
                );
            }
            prev_ssi = Some(s);
        }
    }

    // The fixture must actually contain retargets, otherwise the checks above are vacuous.
    assert!(
        diff_count > 0,
        "no on-chain difficulty retargets in fixture"
    );
    assert!(ssi_count > 0, "no on-chain ssi retargets in fixture");

    let diffs: Vec<u64> = wp
        .sub_epochs
        .iter()
        .filter_map(|se| se.new_difficulty)
        .collect();
    let ssis: Vec<u64> = wp
        .sub_epochs
        .iter()
        .filter_map(|se| se.new_sub_slot_iters)
        .collect();
    eprintln!(
        "mainnet SES: {} sub-epochs, {diff_count} difficulty retargets, {ssi_count} ssi retargets",
        wp.sub_epochs.len()
    );
    eprintln!(
        "difficulty range [{}..{}], first {}, last {}",
        diffs.iter().min().unwrap(),
        diffs.iter().max().unwrap(),
        diffs.first().unwrap(),
        diffs.last().unwrap()
    );
    eprintln!(
        "ssi range [{}..{}], first {}, last {}",
        ssis.iter().min().unwrap(),
        ssis.iter().max().unwrap(),
        ssis.first().unwrap(),
        ssis.last().unwrap()
    );
}
