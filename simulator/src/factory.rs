use crate::error::SimError;
use crate::pos2::PlotSet;
use crate::timelord::prove_vdf;
use dg_xch_core::blockchain::class_group_element::ClassgroupElement;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::pool_target::PoolTarget;
use dg_xch_core::blockchain::proof_of_space::{
    ProofBytes, ProofOfSpace, calculate_pos_challenge, calculate_prefix_bits_v2, passes_plot_filter,
};
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::unfinished_block::UnfinishedBlock;
use dg_xch_core::consensus::constants::ConsensusConstants;
use dg_xch_core::consensus::pot_iterations::{
    calculate_ip_iters, calculate_iterations_quality_for_proof, calculate_sp_interval_iters,
    calculate_sp_iters, is_overflow_block,
};
use dg_xch_core::consensus::producer::{
    FarmerSignatures, calculate_infusion_point_total_iters, create_unfinished_block_with_sigs,
    g2_infinity, unfinished_block_to_full_block,
};

/// The iters a farmed proof resolves to, and the total-iters anchors a block is finished against.
#[derive(Debug, Clone, Copy)]
pub struct FarmedIters {
    pub required_iters: u64,
    pub sp_iters: u64,
    pub ip_iters: u64,
    pub infusion_point_total_iters: u128,
}

/// One farmed proof of space against a fixed challenge: the winning plot, its proof, and the iters
/// it resolves to under the given constants.
#[derive(Debug, Clone)]
pub struct FarmedProof {
    pub plot_index: usize,
    pub proof_of_space: ProofOfSpace,
    pub iters: FarmedIters,
}

/// Farm the genesis challenge from a plot set, returning the first plot whose proof passes the plot
/// filter and resolves to a required-iters inside the infusion range.
///
/// Genesis fixes the challenge, so the only freedom is which plot wins: unlike a running chain there
/// is no signage point to vary. A plot set therefore has to be large enough, or its constants
/// loosened enough, that some plot lands a proof in range against the one genesis challenge.
pub fn farm_genesis(
    constants: &ConsensusConstants,
    plots: &PlotSet,
    difficulty: u64,
    sub_slot_iters: u64,
) -> Result<FarmedProof, SimError> {
    let challenge_hash = constants.genesis_challenge;
    // At the first signage point the signage point is the sub-slot challenge itself, which at
    // genesis is the genesis challenge.
    let signage_point = constants.genesis_challenge;
    let signage_point_index: u8 = 0;
    let prefix_bits = calculate_prefix_bits_v2(constants, 0);
    let sp_interval_iters =
        calculate_sp_interval_iters(constants, sub_slot_iters).map_err(SimError::Io)?;

    for (plot_index, plot) in plots.plots.iter().enumerate() {
        if !passes_plot_filter(prefix_bits, plot.plot_id, challenge_hash, signage_point) {
            continue;
        }
        let pos_challenge = calculate_pos_challenge(plot.plot_id, challenge_hash, signage_point);
        for (found_index, chain) in plots
            .qualities_for_challenge(pos_challenge)?
            .into_iter()
            .filter(|(i, _)| *i == plot_index)
        {
            let proof_bytes = plots.solve(found_index, &chain);
            if proof_bytes.is_empty() {
                continue;
            }
            let pos = ProofOfSpace::v2(
                pos_challenge,
                Some(plot.keys.pool_public_key),
                None,
                plot.keys.plot_public_key,
                u16::try_from(plot_index).unwrap_or(u16::MAX),
                0,
                plots.strength,
                ProofBytes::from(proof_bytes),
            );
            let Some(quality) = dg_xch_pos::verify_and_get_quality_string(
                &pos,
                constants,
                challenge_hash,
                signage_point,
                0,
            ) else {
                continue;
            };
            let required_iters = calculate_iterations_quality_for_proof(
                constants,
                &pos,
                quality,
                difficulty,
                signage_point,
            );
            if required_iters == 0 || required_iters >= sp_interval_iters {
                continue;
            }
            let sp_iters = calculate_sp_iters(constants, sub_slot_iters, signage_point_index)
                .map_err(SimError::Io)?;
            let ip_iters = calculate_ip_iters(
                constants,
                sub_slot_iters,
                signage_point_index,
                required_iters,
            )
            .map_err(SimError::Io)?;
            let infusion_point_total_iters =
                calculate_infusion_point_total_iters(0, sp_iters, ip_iters, sub_slot_iters);
            return Ok(FarmedProof {
                plot_index,
                proof_of_space: pos,
                iters: FarmedIters {
                    required_iters,
                    sp_iters,
                    ip_iters,
                    infusion_point_total_iters,
                },
            });
        }
    }
    Err(SimError::Invariant(
        "no plot produced a genesis proof in range; grow the plot set or loosen the constants"
            .to_string(),
    ))
}

/// Assemble the genesis unfinished block from a farmed proof.
///
/// The farmer signatures are `g2_infinity` placeholders, as the genesis reference does: the
/// producer embeds them without checking, and a simulated chain does not gate on them. The signage
/// point VDFs are `None` because the first signage point coincides with the sub-slot start.
pub fn build_genesis_unfinished(
    constants: &ConsensusConstants,
    farmed: &FarmedProof,
    farmer_reward_puzzle_hash: Bytes32,
    timestamp: u64,
) -> Result<UnfinishedBlock, SimError> {
    let placeholder = FarmerSignatures {
        challenge_chain_sp_signature: g2_infinity(),
        reward_chain_sp_signature: g2_infinity(),
        foliage_block_data_signature: g2_infinity(),
        foliage_transaction_block_signature: g2_infinity(),
    };
    let pool_target = PoolTarget {
        puzzle_hash: constants.genesis_pre_farm_pool_puzzle_hash,
        max_height: 0,
    };
    create_unfinished_block_with_sigs(
        constants,
        farmed.iters.infusion_point_total_iters,
        0,
        farmed.proof_of_space.clone(),
        constants.genesis_challenge,
        None,
        None,
        None,
        None,
        Vec::new(),
        0,
        true,
        &[],
        None,
        constants.genesis_challenge,
        constants.genesis_challenge,
        pool_target,
        None,
        farmer_reward_puzzle_hash,
        timestamp,
        b"dg_xch_simulator/genesis",
        placeholder,
    )
    .map_err(SimError::Producer)
}

/// Finish the genesis unfinished block into a full block by running its two infusion-point VDFs.
///
/// At genesis both the challenge-chain and reward-chain infusion VDFs start from the identity
/// element, carry the genesis challenge, and run for `ip_iters`. They are proved at the network's
/// `discriminant_size_bits`, which a simulated chain sets small so the real prover and the real
/// validator both finish in microseconds without changing what they compute.
pub fn build_genesis_full(
    constants: &ConsensusConstants,
    unfinished: &UnfinishedBlock,
    farmed: &FarmedProof,
) -> Result<FullBlock, SimError> {
    let identity = ClassgroupElement::get_default_element();
    let ip_iters = farmed.iters.ip_iters;
    let disc = constants.discriminant_size_bits;
    let (cc_ip_vdf, cc_ip_proof) =
        prove_vdf(constants.genesis_challenge, &identity, ip_iters, disc)?;
    let (rc_ip_vdf, rc_ip_proof) =
        prove_vdf(constants.genesis_challenge, &identity, ip_iters, disc)?;
    unfinished_block_to_full_block(
        unfinished,
        cc_ip_vdf,
        cc_ip_proof,
        rc_ip_vdf,
        rc_ip_proof,
        None,
        None,
        Vec::new(),
        None,
        true,
        constants.difficulty_starting,
    )
    .map_err(SimError::Producer)
}

/// Whether a farmed proof is a signage-point overflow, which the finishing step needs to know.
#[must_use]
pub fn farmed_is_overflow(constants: &ConsensusConstants, signage_point_index: u8) -> bool {
    is_overflow_block(constants, signage_point_index).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::consensus::constants::SIMULATOR;
    use dg_xch_core::consensus::overrides::{ConsensusOverrides, apply_overrides};

    const K: u8 = 18;
    const STRENGTH: u8 = 2;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dgxch_sim_factory_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// SIMULATOR with the v2 plot size at k18, the v2 plot filter disabled so every plot is a
    /// candidate, and a low difficulty so a farmed proof lands in the infusion range.
    fn constants() -> ConsensusConstants {
        apply_overrides(
            SIMULATOR,
            &ConsensusOverrides {
                plot_size_v2: Some(K),
                number_zero_bits_plot_filter_v2: Some(0),
                // chia's test constant. With the mainnet 2^67 factor a k18 proof's required-iters
                // sits far above the infusion range and never wins; 2^25 brings the lottery into a
                // range where a small plot set finds a genesis proof.
                difficulty_constant_factor: Some(2u128.pow(25)),
                difficulty_starting: Some(7),
                // A tiny discriminant makes the real VDF finish in microseconds and validate
                // unchanged; a small sub-slot keeps ip_iters low so the proof is a handful of
                // squarings. 65536 = 64 * 1024, so it still divides the signage points evenly.
                discriminant_size_bits: Some(num_bigint::BigInt::from(16)),
                sub_slot_iters_starting: Some(65_536),
                ..Default::default()
            },
        )
    }

    #[test]
    fn a_genesis_proof_is_farmed_and_assembled_into_an_unfinished_block() {
        let c = constants();
        let dir = scratch("genesis");
        let plots = PlotSet::setup(&dir, 3, 4, K, STRENGTH, false).expect("plots");

        let farmed = farm_genesis(&c, &plots, c.difficulty_starting, c.sub_slot_iters_starting)
            .expect("a genesis proof must be farmable");
        assert_eq!(farmed.proof_of_space.version, 1, "genesis proof is v2");
        assert!(farmed.iters.required_iters >= 1);

        let ub = build_genesis_unfinished(&c, &farmed, Bytes32::from([0xAB; 32]), 1_700_000_000)
            .expect("the producer must accept a farmed v2 proof");

        // The assembled block carries the farmed proof, and that proof still verifies against the
        // genesis challenge — the pos2 farming path and the producer agree.
        assert_eq!(
            ub.reward_chain_block.proof_of_space, farmed.proof_of_space,
            "the unfinished block did not carry the farmed proof"
        );
        assert!(
            dg_xch_pos::verify_and_get_quality_string(
                &ub.reward_chain_block.proof_of_space,
                &c,
                c.genesis_challenge,
                c.genesis_challenge,
                0,
            )
            .is_some(),
            "the embedded proof no longer verifies"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_farmed_genesis_block_is_accepted_by_the_engine() {
        use dg_xch_node::engine::{AddBlockOutcome, Engine};
        use dg_xch_node::primitives::NativePrimitives;
        use dg_xch_stores::SqliteStore;

        let c = constants();
        let dir = scratch("engine");
        let plots = PlotSet::setup(&dir, 5, 4, K, STRENGTH, false).expect("plots");

        let farmed = farm_genesis(&c, &plots, c.difficulty_starting, c.sub_slot_iters_starting)
            .expect("farm genesis");
        let ub = build_genesis_unfinished(&c, &farmed, Bytes32::from([0xAB; 32]), 1_700_000_000)
            .expect("assemble");
        let full = build_genesis_full(&c, &ub, &farmed).expect("finish");
        assert_eq!(full.reward_chain_block.height, 0, "genesis is height 0");

        // The whole point: a block farmed and finished from scratch clears live consensus and
        // becomes the peak. The tiny-discriminant VDFs are real, so add_block's VDF gates pass.
        let db = tempfile::tempdir().expect("tempdir");
        let store = SqliteStore::open(&db.path().join("sim.sqlite"))
            .await
            .expect("open store");
        let mut engine = Engine::new(store, NativePrimitives, c);
        let outcome = engine.add_block(&full).await.expect("add_block");
        assert!(
            matches!(outcome, AddBlockOutcome::NewPeak { height: 0 }),
            "genesis did not become the peak: {outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
