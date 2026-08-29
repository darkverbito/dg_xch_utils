use crate::pos2::constants::CHAIN_SET_BITS;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use std::io::{Error, ErrorKind};

/// An inclusive range of proof fragment values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: u64,
    pub end: u64,
}

impl Range {
    #[must_use]
    pub fn contains(&self, value: u64) -> bool {
        value >= self.start && value <= self.end
    }
}

/// The parameters a plot is proved and verified under.
///
/// `match_info` is `k` bits laid out as `[section | match_key | match_target]`, and the widths of
/// the first two fields are what `strength` tunes: a stronger plot spends more match key bits,
/// which costs the plotter time without costing the verifier anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProofParams {
    plot_id: Bytes32,
    k: u8,
    strength: u8,
    testnet: bool,
}

impl ProofParams {
    pub fn new(plot_id: Bytes32, k: u8, strength: u8, testnet: bool) -> Result<Self, Error> {
        if strength < 2 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("strength {strength} must be at least 2"),
            ));
        }
        if strength > 63 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("strength {strength} must be below 64"),
            ));
        }
        let params = Self {
            plot_id,
            k,
            strength,
            testnet,
        };
        let ceiling = u32::from(k)
            .checked_sub(params.num_section_bits() + 1)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("k {k} is too small for a plot"),
                )
            })?;
        if u32::from(strength) > ceiling {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("strength {strength} must not exceed k - section_bits - 1 ({ceiling})"),
            ));
        }
        Ok(params)
    }

    #[must_use]
    pub fn plot_id(&self) -> Bytes32 {
        self.plot_id
    }

    #[must_use]
    pub fn k(&self) -> u8 {
        self.k
    }

    #[must_use]
    pub fn strength(&self) -> u8 {
        self.strength
    }

    #[must_use]
    pub fn is_testnet(&self) -> bool {
        self.testnet
    }

    #[must_use]
    pub fn num_section_bits(&self) -> u32 {
        if self.k < 28 {
            2
        } else {
            u32::from(self.k) - 26
        }
    }

    #[must_use]
    pub fn num_sections(&self) -> u32 {
        1u32 << self.num_section_bits()
    }

    /// Table 1 always spends two match key bits; later tables spend `strength`.
    #[must_use]
    pub fn num_match_key_bits(&self, table_id: usize) -> u32 {
        assert!(
            (1..=3).contains(&table_id),
            "table_id {table_id} out of range"
        );
        if table_id == 1 {
            2
        } else {
            u32::from(self.strength)
        }
    }

    #[must_use]
    pub fn num_match_keys(&self, table_id: usize) -> u64 {
        1u64 << self.num_match_key_bits(table_id)
    }

    #[must_use]
    pub fn num_match_target_bits(&self, table_id: usize) -> u32 {
        u32::from(self.k) - self.num_section_bits() - self.num_match_key_bits(table_id)
    }

    /// Table 1 carries one x worth of metadata; later tables carry a pair.
    #[must_use]
    pub fn num_meta_bits(&self, table_id: usize) -> u32 {
        if table_id == 1 {
            u32::from(self.k)
        } else {
            u32::from(self.k) * 2
        }
    }

    #[must_use]
    pub fn num_pairing_meta_bits(&self) -> u32 {
        2 * u32::from(self.k)
    }

    #[must_use]
    pub fn extract_section(&self, match_info: u32) -> u32 {
        match_info >> (u32::from(self.k) - self.num_section_bits())
    }

    #[must_use]
    pub fn extract_match_key(&self, table_id: usize, match_info: u32) -> u32 {
        let match_bits = self.num_match_key_bits(table_id);
        let shift = u32::from(self.k) - self.num_section_bits() - match_bits;
        (match_info >> shift) & ((1u32 << match_bits) - 1)
    }

    #[must_use]
    pub fn extract_match_target(&self, table_id: usize, match_info: u64) -> u32 {
        let bits = self.num_match_target_bits(table_id);
        (match_info & ((1u64 << bits) - 1)) as u32
    }

    #[must_use]
    pub fn chaining_set_bits(&self) -> u32 {
        CHAIN_SET_BITS
    }

    #[must_use]
    pub fn chaining_set_size(&self) -> u32 {
        1u32 << self.chaining_set_bits()
    }

    #[must_use]
    pub fn num_chaining_sets_bits(&self) -> u32 {
        u32::from(self.k) - self.chaining_set_bits()
    }

    #[must_use]
    pub fn num_chaining_sets(&self) -> u32 {
        1u32 << self.num_chaining_sets_bits()
    }

    #[must_use]
    pub fn chaining_set_range(&self, chaining_set_index: u64) -> Range {
        let width = 1u64 << (u32::from(self.k) + self.chaining_set_bits());
        let start = chaining_set_index * width;
        Range {
            start,
            end: start + width - 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(k: u8, strength: u8) -> ProofParams {
        ProofParams::new(Bytes32::from([1u8; 32]), k, strength, false).expect("valid params")
    }

    #[test]
    fn section_bits_follow_k() {
        assert_eq!(params(28, 2).num_section_bits(), 2);
        assert_eq!(params(28, 2).num_sections(), 4);
        assert_eq!(params(30, 2).num_section_bits(), 4);
        assert_eq!(params(32, 2).num_section_bits(), 6);
        assert_eq!(params(32, 2).num_sections(), 64);
        // Below k28 the width is pinned rather than going negative.
        assert_eq!(params(20, 2).num_section_bits(), 2);
    }

    #[test]
    fn match_key_bits_are_two_for_table_one_and_strength_after() {
        let p = params(28, 5);
        assert_eq!(p.num_match_key_bits(1), 2);
        assert_eq!(p.num_match_key_bits(2), 5);
        assert_eq!(p.num_match_key_bits(3), 5);
    }

    #[test]
    fn the_three_match_info_fields_fill_exactly_k_bits() {
        for k in [28u8, 30, 32] {
            for strength in [2u8, 5, 16] {
                let p = params(k, strength);
                for table in 1..=3 {
                    assert_eq!(
                        p.num_section_bits()
                            + p.num_match_key_bits(table)
                            + p.num_match_target_bits(table),
                        u32::from(k),
                        "k {k} strength {strength} table {table}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_match_info_round_trips_through_its_three_extractors() {
        let p = params(28, 5);
        for table in 1..=3usize {
            let section = p.num_sections() - 1;
            let key = (1u32 << p.num_match_key_bits(table)) - 2;
            let target = (1u32 << p.num_match_target_bits(table)) - 3;
            let match_info = (section << (28 - p.num_section_bits()))
                | (key << p.num_match_target_bits(table))
                | target;
            assert_eq!(p.extract_section(match_info), section, "table {table}");
            assert_eq!(p.extract_match_key(table, match_info), key, "table {table}");
            assert_eq!(
                p.extract_match_target(table, u64::from(match_info)),
                target,
                "table {table}"
            );
        }
    }

    #[test]
    fn meta_widens_after_table_one() {
        let p = params(28, 2);
        assert_eq!(p.num_meta_bits(1), 28);
        assert_eq!(p.num_meta_bits(2), 56);
        assert_eq!(p.num_pairing_meta_bits(), 56);
    }

    #[test]
    fn chaining_sets_tile_the_fragment_space() {
        let p = params(28, 2);
        assert_eq!(p.chaining_set_size(), 64);
        assert_eq!(p.num_chaining_sets_bits(), 22);
        let first = p.chaining_set_range(0);
        let second = p.chaining_set_range(1);
        assert_eq!(first.start, 0);
        assert_eq!(second.start, first.end + 1);
        assert!(first.contains(first.end));
        assert!(!first.contains(second.start));
    }

    #[test]
    fn strength_is_bounded_on_both_sides() {
        let id = Bytes32::from([1u8; 32]);
        assert!(
            ProofParams::new(id, 28, 1, false).is_err(),
            "strength 1 accepted"
        );
        assert!(
            ProofParams::new(id, 28, 64, false).is_err(),
            "strength 64 accepted"
        );
        // k28 leaves 2 section bits, so the ceiling is 28 - 2 - 1 = 25.
        assert!(ProofParams::new(id, 28, 25, false).is_ok());
        assert!(
            ProofParams::new(id, 28, 26, false).is_err(),
            "over the ceiling accepted"
        );
    }
}
