use crate::error::ConfigError;
use dg_xch_core::consensus::constants::{ChiaNetwork, ConsensusConstants};
use dg_xch_core::consensus::overrides::{ConsensusOverrides, apply_overrides};

/// `create_discriminant_int` rejects a size that is zero, over 1024 bits, or not a multiple of 8.
const MAX_DISCRIMINANT_SIZE_BITS: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HarnessConfig {
    pub campaign_seed: u64,
    pub n_runs: u32,
    pub horizon_blocks: u32,
}

/// A run's consensus parameters, expressed as overrides folded onto a stock network.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SimConfig {
    pub network: ChiaNetwork,
    #[serde(default)]
    pub consensus: ConsensusOverrides,
    pub harness: HarnessConfig,
}

impl SimConfig {
    /// The effective constants for a run, or the first field that fails validation. Ranges are
    /// checked before cross-field invariants so the narrower diagnosis wins.
    pub fn constants(&self) -> Result<ConsensusConstants, ConfigError> {
        let c = apply_overrides(ConsensusConstants::from(self.network), &self.consensus);
        check_ranges(&c)?;
        check_invariants(&c)?;
        self.check_harness()?;
        Ok(c)
    }

    fn check_harness(&self) -> Result<(), ConfigError> {
        if self.harness.n_runs == 0 {
            return Err(ConfigError::range("harness.n_runs", "must be at least 1"));
        }
        if self.harness.horizon_blocks == 0 {
            return Err(ConfigError::range(
                "harness.horizon_blocks",
                "must be at least 1",
            ));
        }
        Ok(())
    }
}

fn check_ranges(c: &ConsensusConstants) -> Result<(), ConfigError> {
    if c.num_sps_sub_slot == 0 {
        return Err(ConfigError::range(
            "consensus.num_sps_sub_slot",
            "must be at least 1; it divides sub_slot_iters",
        ));
    }
    if c.discriminant_size_bits == 0
        || c.discriminant_size_bits > MAX_DISCRIMINANT_SIZE_BITS
        || !c.discriminant_size_bits.is_multiple_of(8)
    {
        return Err(ConfigError::range(
            "consensus.discriminant_size_bits",
            format!(
                "must be positive, at most {MAX_DISCRIMINANT_SIZE_BITS}, and a multiple of 8, got {}",
                c.discriminant_size_bits
            ),
        ));
    }
    if c.difficulty_starting == 0 {
        return Err(ConfigError::range(
            "consensus.difficulty_starting",
            "must be at least 1",
        ));
    }
    if c.sub_slot_iters_starting == 0 {
        return Err(ConfigError::range(
            "consensus.sub_slot_iters_starting",
            "must be at least 1",
        ));
    }
    if c.sub_epoch_blocks == 0 {
        return Err(ConfigError::range(
            "consensus.sub_epoch_blocks",
            "must be at least 1",
        ));
    }
    Ok(())
}

fn check_invariants(c: &ConsensusConstants) -> Result<(), ConfigError> {
    // is_overflow_block computes num_sps_sub_slot - num_sp_intervals_extra.
    if c.num_sp_intervals_extra >= u64::from(c.num_sps_sub_slot) {
        return Err(ConfigError::cross_field(
            "consensus.num_sp_intervals_extra/consensus.num_sps_sub_slot",
            format!(
                "num_sp_intervals_extra ({}) must be below num_sps_sub_slot ({})",
                c.num_sp_intervals_extra, c.num_sps_sub_slot
            ),
        ));
    }
    // calculate_sp_interval_iters rejects a sub_slot_iters that is not a whole multiple.
    if !c
        .sub_slot_iters_starting
        .is_multiple_of(u64::from(c.num_sps_sub_slot))
    {
        return Err(ConfigError::cross_field(
            "consensus.sub_slot_iters_starting/consensus.num_sps_sub_slot",
            format!(
                "sub_slot_iters_starting ({}) must be a whole multiple of num_sps_sub_slot ({})",
                c.sub_slot_iters_starting, c.num_sps_sub_slot
            ),
        ));
    }
    if !c.epoch_blocks.is_multiple_of(c.sub_epoch_blocks) {
        return Err(ConfigError::cross_field(
            "consensus.epoch_blocks/consensus.sub_epoch_blocks",
            format!(
                "epoch_blocks ({}) must be a whole multiple of sub_epoch_blocks ({})",
                c.epoch_blocks, c.sub_epoch_blocks
            ),
        ));
    }
    if c.max_sub_slot_blocks >= c.sub_epoch_blocks / 2 {
        return Err(ConfigError::cross_field(
            "consensus.max_sub_slot_blocks/consensus.sub_epoch_blocks",
            format!(
                "max_sub_slot_blocks ({}) must be below sub_epoch_blocks/2 ({})",
                c.max_sub_slot_blocks,
                c.sub_epoch_blocks / 2
            ),
        ));
    }
    if c.max_sub_slot_blocks <= c.slot_blocks_target {
        return Err(ConfigError::cross_field(
            "consensus.max_sub_slot_blocks/consensus.slot_blocks_target",
            format!(
                "max_sub_slot_blocks ({}) must exceed slot_blocks_target ({})",
                c.max_sub_slot_blocks, c.slot_blocks_target
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ValidationTier;
    use num_bigint::BigInt;

    fn config(network: ChiaNetwork, consensus: ConsensusOverrides) -> SimConfig {
        SimConfig {
            network,
            consensus,
            harness: HarnessConfig {
                campaign_seed: 1,
                n_runs: 1,
                horizon_blocks: 1,
            },
        }
    }

    #[test]
    fn the_stock_networks_validate() {
        for network in [ChiaNetwork::Mainnet, ChiaNetwork::Simulator] {
            config(network, ConsensusOverrides::default())
                .constants()
                .unwrap_or_else(|e| panic!("{network:?} rejected by its own rules: {e}"));
        }
    }

    #[test]
    fn a_tiny_discriminant_is_accepted() {
        let c = config(
            ChiaNetwork::Simulator,
            ConsensusOverrides {
                discriminant_size_bits: Some(BigInt::from(16)),
                ..Default::default()
            },
        )
        .constants()
        .expect("16 bits is positive, under 1024, and a multiple of 8");
        assert_eq!(c.discriminant_size_bits, 16);
    }

    #[test]
    fn a_discriminant_off_the_byte_boundary_is_a_range_error() {
        let e = config(
            ChiaNetwork::Mainnet,
            ConsensusOverrides {
                discriminant_size_bits: Some(BigInt::from(20)),
                ..Default::default()
            },
        )
        .constants()
        .expect_err("20 is not a multiple of 8");
        assert_eq!(e.tier, ValidationTier::Range);
        assert_eq!(e.field, "consensus.discriminant_size_bits");
    }

    #[test]
    fn sub_slot_iters_must_divide_into_signage_points() {
        let e = config(
            ChiaNetwork::Mainnet,
            ConsensusOverrides {
                sub_slot_iters_starting: Some(1_000),
                num_sps_sub_slot: Some(64),
                ..Default::default()
            },
        )
        .constants()
        .expect_err("1000 is not a multiple of 64");
        assert_eq!(e.tier, ValidationTier::CrossField);
        assert!(e.field.contains("sub_slot_iters_starting"), "{}", e.field);
    }

    #[test]
    fn epoch_blocks_must_be_a_multiple_of_sub_epoch_blocks() {
        let e = config(
            ChiaNetwork::Mainnet,
            ConsensusOverrides {
                epoch_blocks: Some(4_609),
                ..Default::default()
            },
        )
        .constants()
        .expect_err("4609 is not a multiple of 384");
        assert_eq!(e.tier, ValidationTier::CrossField);
        assert!(e.field.contains("epoch_blocks"), "{}", e.field);
    }

    #[test]
    fn max_sub_slot_blocks_is_bounded_on_both_sides() {
        let low = config(
            ChiaNetwork::Mainnet,
            ConsensusOverrides {
                max_sub_slot_blocks: Some(16),
                ..Default::default()
            },
        )
        .constants()
        .expect_err("16 does not exceed slot_blocks_target 32");
        assert!(low.field.contains("slot_blocks_target"), "{}", low.field);

        let high = config(
            ChiaNetwork::Mainnet,
            ConsensusOverrides {
                max_sub_slot_blocks: Some(192),
                ..Default::default()
            },
        )
        .constants()
        .expect_err("192 is not below sub_epoch_blocks/2 of 192");
        assert!(high.field.contains("sub_epoch_blocks"), "{}", high.field);
    }

    #[test]
    fn an_empty_campaign_is_a_range_error() {
        let mut c = config(ChiaNetwork::Simulator, ConsensusOverrides::default());
        c.harness.n_runs = 0;
        let e = c.constants().expect_err("zero runs");
        assert_eq!(e.tier, ValidationTier::Range);
        assert_eq!(e.field, "harness.n_runs");
    }

    #[test]
    fn a_network_round_trips_through_serde_by_its_lowercase_name() {
        let c = config(ChiaNetwork::Simulator, ConsensusOverrides::default());
        let json = serde_json::to_string(&c).expect("serialize");
        assert!(json.contains("\"simulator\""), "{json}");
        let back: SimConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, c);
    }
}
