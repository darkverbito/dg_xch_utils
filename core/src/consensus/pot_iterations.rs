use crate::blockchain::sized_bytes::Bytes32;
use crate::consensus::constants::ConsensusConstants;
use crate::constants::TWO_POW_256;
use crate::utils::hash_256;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use std::cmp::max;
use std::io::{Error, ErrorKind};
use std::ops::Mul;

pub fn is_overflow_block(
    constants: &ConsensusConstants,
    signage_point_index: u8,
) -> Result<bool, Error> {
    if u32::from(signage_point_index) >= constants.num_sps_sub_slot {
        Err(Error::new(ErrorKind::InvalidData, "SP index too high"))
    } else {
        Ok(u64::from(signage_point_index)
            >= u64::from(constants.num_sps_sub_slot) - constants.num_sp_intervals_extra)
    }
}

pub fn calculate_sp_interval_iters(
    constants: &ConsensusConstants,
    sub_slot_iters: u64,
) -> Result<u64, Error> {
    if !sub_slot_iters.is_multiple_of(u64::from(constants.num_sps_sub_slot)) {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!("Invalid SubSlot Iterations: {sub_slot_iters}"),
        ))
    } else {
        Ok(sub_slot_iters / u64::from(constants.num_sps_sub_slot))
    }
}

pub fn calculate_sp_iters(
    constants: &ConsensusConstants,
    sub_slot_iters: u64,
    signage_point_index: u8,
) -> Result<u64, Error> {
    if u32::from(signage_point_index) >= constants.num_sps_sub_slot {
        Err(Error::new(ErrorKind::InvalidData, "SP index too high"))
    } else {
        Ok(
            calculate_sp_interval_iters(constants, sub_slot_iters)?
                * u64::from(signage_point_index),
        )
    }
}

pub fn calculate_ip_iters(
    constants: &ConsensusConstants,
    sub_slot_iters: u64,
    signage_point_index: u8,
    required_iters: u64,
) -> Result<u64, Error> {
    let sp_iters = calculate_sp_iters(constants, sub_slot_iters, signage_point_index)?;
    let sp_interval_iters = calculate_sp_interval_iters(constants, sub_slot_iters)?;
    if sp_iters % sp_interval_iters != 0 || sp_iters >= sub_slot_iters {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!("Invalid sp iters {sp_iters} for this ssi {sub_slot_iters}"),
        ))
    } else if required_iters >= sp_interval_iters || required_iters == 0 {
        Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "Required iters {required_iters} is not below the sp interval iters {sp_interval_iters}, {sub_slot_iters} or not > 0."
            ),
        ))
    } else {
        Ok(
            (sp_iters + constants.num_sp_intervals_extra * sp_interval_iters + required_iters)
                % sub_slot_iters,
        )
    }
}

#[must_use]
pub fn expected_plot_size(k: u8) -> u64 {
    ((2 * u64::from(k)) + 1) * 2u64.pow(u32::from(k) - 1)
}

#[must_use]
pub fn calculate_iterations_quality(
    difficulty_constant_factor: u128,
    quality_string: Bytes32,
    size: u8,
    difficulty: u64,
    cc_sp_output_hash: Bytes32,
) -> u64 {
    let mut to_hash: Vec<u8> = Vec::new();
    to_hash.extend(quality_string);
    to_hash.extend(cc_sp_output_hash);
    let hashed = hash_256(to_hash);
    let quality_int = BigUint::from_bytes_be(hashed.as_slice());
    let difficulty_int = BigUint::from(difficulty);
    let difficulty_constant_factor_int = BigUint::from(difficulty_constant_factor);
    let top: BigUint = difficulty_int * difficulty_constant_factor_int * quality_int;
    let bottom: BigUint = (*TWO_POW_256).clone().mul(expected_plot_size(size));
    let bigint: BigUint = top / bottom;
    if bigint.gt(&u64::MAX.into()) {
        return u64::MAX;
    }
    max(1, bigint.to_u64().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    //! Harvested from `chia/_tests/core/consensus/test_pot_iterations.py` (the "harvest Chia's tests,
    //! not libs" discipline). chia's `test_constants` overrides `NUM_SPS_SUB_SLOT=32`
    //! (`SUB_SLOT_TIME_TARGET=300` and `HARD_FORK2_HEIGHT` are irrelevant to these functions), so the
    //! overflow boundary is `32 - NUM_SP_INTERVALS_EXTRA(3) = 29`. Every vector below is chia's, verbatim.
    //!
    //! DIVERGENCES (flagged, not bugs):
    //!   * chia's `calculate_iterations_quality` takes a `PlotParam` (v1 or v2); dg_xch takes the v1
    //!     `size: u8`. All v1 vectors port faithfully; the v2 plot model (`PlotParam::make_v2`,
    //!     `PLOT_SIZE_V2`, `_expected_plot_size` v2) does not exist in dg_xch, so the v2-only tests are
    //!     `#[ignore]`d with that reason.
    //!   * on an astronomically-large quotient chia's `uint64(...)` would raise; dg_xch CLAMPS to
    //!     `u64::MAX` (see `clamps_to_u64_max_on_overflow`). Both reject the impossible proof downstream
    //!     (`calculate_ip_iters` then errors on `required_iters >= sp_interval_iters`) — a defensive
    //!     clamp vs a raise, not a consensus difference.

    use super::*;
    use crate::consensus::constants::MAINNET;
    use crate::traits::SizedBytes; // Bytes32::new (quality-hash construction in the harvested vectors)

    // chia: DEFAULT_CONSTANTS.replace(NUM_SPS_SUB_SLOT=32, ...). ConsensusConstants is Copy.
    fn test_constants() -> ConsensusConstants {
        ConsensusConstants {
            num_sps_sub_slot: 32,
            ..MAINNET
        }
    }

    // chia test_pot_iterations.py::TestPotIterations::test_is_overflow_block
    #[test]
    fn test_is_overflow_block() {
        let c = test_constants();
        assert!(!is_overflow_block(&c, 27).unwrap());
        assert!(!is_overflow_block(&c, 28).unwrap());
        assert!(is_overflow_block(&c, 29).unwrap());
        assert!(is_overflow_block(&c, 30).unwrap());
        assert!(is_overflow_block(&c, 31).unwrap());
        // chia: raises ValueError("SP index too high").
        let err = is_overflow_block(&c, 32).unwrap_err();
        assert!(err.to_string().contains("SP index too high"));
    }

    // chia test_pot_iterations.py::TestPotIterations::test_calculate_sp_iters
    #[test]
    fn test_calculate_sp_iters() {
        let c = test_constants();
        let ssi: u64 = 100_001 * 64 * 4;
        // chia: raises ValueError("SP index too high") for index == NUM_SPS_SUB_SLOT.
        let err = calculate_sp_iters(&c, ssi, 32).unwrap_err();
        assert!(err.to_string().contains("SP index too high"));
        // The last valid index (31) does not error.
        assert!(calculate_sp_iters(&c, ssi, 31).is_ok());
    }

    // chia test_pot_iterations.py::TestPotIterations::test_calculate_ip_iters
    #[test]
    fn test_calculate_ip_iters() {
        let c = test_constants();
        let ssi: u64 = 100_001 * 64 * 4;
        let sp_interval_iters = ssi / u64::from(c.num_sps_sub_slot);
        let extra = c.num_sp_intervals_extra;

        // chia: invalid signage point index -> "SP index too high".
        let err = calculate_ip_iters(&c, ssi, 123, 100_000).unwrap_err();
        assert!(err.to_string().contains("SP index too high"));

        let sp_iters = sp_interval_iters * 13;

        // chia: required_iters too high (== and > sp_interval_iters) -> "Required iters ...".
        let err = calculate_ip_iters(&c, ssi, 0, sp_interval_iters).unwrap_err();
        assert!(err.to_string().contains("Required iters"));
        let err = calculate_ip_iters(&c, ssi, 0, sp_interval_iters * 12).unwrap_err();
        assert!(err.to_string().contains("Required iters"));

        // chia: required_iters too low (0) -> same message ("... or not > 0.").
        let err = calculate_ip_iters(&c, ssi, 0, 0).unwrap_err();
        assert!(err.to_string().contains("Required iters"));

        // Non-overflow: ip_iters == sp_iters + extra*sp_interval + required (the % ssi is a no-op).
        let required_iters = sp_interval_iters - 1;
        let ip_iters = calculate_ip_iters(&c, ssi, 13, required_iters).unwrap();
        assert_eq!(
            ip_iters,
            sp_iters + extra * sp_interval_iters + required_iters
        );

        let required_iters = 1;
        let ip_iters = calculate_ip_iters(&c, ssi, 13, required_iters).unwrap();
        assert_eq!(
            ip_iters,
            sp_iters + extra * sp_interval_iters + required_iters
        );

        // chia: required_iters = uint64(ssi * 4 / 300) (Python float-div then truncate == integer div).
        let required_iters = (ssi * 4) / 300;
        let ip_iters = calculate_ip_iters(&c, ssi, 13, required_iters).unwrap();
        assert_eq!(
            ip_iters,
            sp_iters + extra * sp_interval_iters + required_iters
        );
        assert!(sp_iters < ip_iters);

        // Overflow (the candidate's make-or-break vector): index NUM_SPS_SUB_SLOT-1, sp_iters > ip_iters,
        // ip_iters == (sp_iters + extra*sp_interval + required) % ssi.
        let sp_iters = sp_interval_iters * u64::from(c.num_sps_sub_slot - 1);
        let ip_iters =
            calculate_ip_iters(&c, ssi, (c.num_sps_sub_slot - 1) as u8, required_iters).unwrap();
        assert_eq!(
            ip_iters,
            (sp_iters + extra * sp_interval_iters + required_iters) % ssi
        );
        assert!(sp_iters > ip_iters);
    }

    // The quality -> required_iters path the candidate's `resolve_candidate_iters` stands on. chia has no
    // standalone unit vector for `calculate_iterations_quality` (it is exercised inside test_win_percentage,
    // which needs the v2 plot model — see the ignored port below). These deterministic invariants lock the
    // v1 path without a hand-computed sha256+bigint: floored at 1, linear in difficulty, inverse in plot
    // size. Anchor: chia/consensus/pot_iterations.py::calculate_iterations_quality.
    #[test]
    fn calculate_iterations_quality_v1_invariants() {
        let dcf = MAINNET.difficulty_constant_factor;
        let q = Bytes32::from([7u8; 32]);
        let sp = Bytes32::from([9u8; 32]);
        // Always >= 1 (chia max(iters, 1)).
        assert!(calculate_iterations_quality(dcf, q, 32, 1, sp) >= 1);
        // Linear in difficulty => monotonic non-decreasing.
        let low = calculate_iterations_quality(dcf, q, 32, 1, sp);
        let high = calculate_iterations_quality(dcf, q, 32, 1_000, sp);
        assert!(high >= low, "required_iters grows with difficulty");
        // Inverse in expected_plot_size => a larger k yields fewer-or-equal iters.
        let small_k = calculate_iterations_quality(dcf, q, 32, 1_000_000, sp);
        let large_k = calculate_iterations_quality(dcf, q, 40, 1_000_000, sp);
        assert!(
            large_k <= small_k,
            "a bigger plot wins more often (fewer iters)"
        );
    }

    // DIVERGENCE lock: dg_xch clamps to u64::MAX where chia's uint64() would raise. Anchor:
    // chia/consensus/pot_iterations.py::calculate_iterations_quality (the uint64(...) cast).
    #[test]
    fn clamps_to_u64_max_on_overflow() {
        let got = calculate_iterations_quality(
            u128::MAX,
            Bytes32::from([0xFFu8; 32]),
            18,
            u64::MAX,
            Bytes32::from([0xFFu8; 32]),
        );
        assert_eq!(got, u64::MAX);
    }

    // chia test_pot_iterations.py::test_expected_plot_size_v1
    #[test]
    fn test_expected_plot_size_v1() {
        let mut last_size = 2_400_000u64;
        for k in 18u8..50 {
            let plot_size = expected_plot_size(k);
            assert!(plot_size > last_size * 2, "k={k} not > 2x previous");
            last_size = plot_size;
        }
    }

    // chia test_pot_iterations.py::TestPotIterations::test_win_percentage — PORTED v1-only, #[ignore]d.
    // REASON (not faked): chia's fixture mixes v1 and v2 farmers; dg_xch has no v2 plot model
    // (`PlotParam::make_v2`, `PLOT_SIZE_V2`, `_expected_plot_size` v2), so the v2 farmers are omitted and
    // this is a v1-only reduction of chia's vector — the proportionality property still holds among v1
    // farmers. It is also a ~400k-iteration probabilistic vector (1% tolerance) that was NOT run locally
    // (no-cargo constraint), so it is ignored until confirmed on the cluster rather than shipped green.
    #[test]
    #[ignore = "v1-only reduction of chia test_win_percentage (no v2 plot model in dg_xch); heavy \
                probabilistic vector, confirm on cluster before un-ignoring"]
    fn test_win_percentage_v1_only() {
        use std::collections::BTreeMap;

        let constants = ConsensusConstants {
            num_sps_sub_slot: 32,
            difficulty_constant_factor: 2u128.pow(25),
            ..MAINNET
        };
        // chia farmer_ks — v1 only (v2 entries dropped: no dg_xch model).
        let farmer_ks: [(u8, u64); 5] = [(32, 100), (33, 100), (34, 100), (35, 100), (36, 100)];
        let farmer_space: BTreeMap<u8, u128> = farmer_ks
            .iter()
            .map(|&(k, count)| (k, u128::from(expected_plot_size(k)) * u128::from(count)))
            .collect();
        let mut wins: BTreeMap<u8, u64> = farmer_ks.iter().map(|&(k, _)| (k, 0)).collect();

        let total_slots = 50u32;
        let num_sps = 16u32;
        let sub_slot_iters: u64 = 100_000_000;
        let sp_interval_iters = calculate_sp_interval_iters(&constants, sub_slot_iters).unwrap();
        let difficulty: u64 = 500_000_000_000;

        for slot_index in 0..total_slots {
            for sp_index in 0..num_sps {
                let mut sp_in = Vec::new();
                sp_in.extend_from_slice(&slot_index.to_be_bytes());
                sp_in.extend_from_slice(&sp_index.to_be_bytes());
                let sp_hash = Bytes32::new(hash_256(sp_in));
                for &(k, count) in &farmer_ks {
                    for farmer_index in 0..count {
                        // chia: std_hash(slot_be4 + k_1byte + bytes(farmer_index)) — bytes(n) is n zeros.
                        let mut q_in = Vec::new();
                        q_in.extend_from_slice(&slot_index.to_be_bytes());
                        q_in.push(k);
                        let base = q_in.len();
                        q_in.resize(base + farmer_index as usize, 0u8);
                        let quality = Bytes32::new(hash_256(q_in));
                        let required_iters = calculate_iterations_quality(
                            constants.difficulty_constant_factor,
                            quality,
                            k,
                            difficulty,
                            sp_hash,
                        );
                        if required_iters < sp_interval_iters {
                            *wins.get_mut(&k).unwrap() += 1;
                        }
                    }
                }
            }
        }

        let total_space: u128 = farmer_space.values().sum();
        let total_wins: u64 = wins.values().sum();
        for &(k, _) in &farmer_ks {
            let percentage_space = farmer_space[&k] as f64 / total_space as f64;
            let win_percentage = wins[&k] as f64 / total_wins as f64;
            assert!(
                (win_percentage - percentage_space).abs() < 0.01,
                "k={k}: win {win_percentage} vs space {percentage_space}"
            );
        }
    }

    // chia test_pot_iterations.py::test_expected_plot_size_v2 — NOT PORTED.
    // REASON (not faked): requires the v2 plot-size model — `_expected_plot_size(PlotParam::make_v2(...))`
    // asserting the constant 988_513_566 — which dg_xch does not implement (v1 `expected_plot_size(k)`
    // only). No faithful port exists until dg_xch grows a v2 plot model.
    #[test]
    #[ignore = "no v2 plot-size model in dg_xch (_expected_plot_size v2 / PLOT_SIZE_V2 absent)"]
    fn test_expected_plot_size_v2() {}
}
