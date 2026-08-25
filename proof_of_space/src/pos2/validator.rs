use crate::pos2::chainer::{Chain, Chainer, QualityChainLinks};
use crate::pos2::constants::{NUM_CHAIN_LINKS, TOTAL_XS_IN_PROOF};
use crate::pos2::core::{ProofCore, T1Pairing, T2Pairing, T3Pairing};
use crate::pos2::params::ProofParams;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use std::io::Error;

/// Verifies a proof of space 2 proof, ported from `src/pos/ProofValidator.hpp`.
///
/// A proof is 128 x values in sixteen groups of eight. Each group must pair all the way up through
/// tables 1, 2 and 3, and the sixteen fragments those groups encode to must then form a valid
/// quality chain against the challenge. The plot filter is the caller's job, not this one's.
#[derive(Debug, Clone)]
pub struct ProofValidator {
    core: ProofCore,
}

impl ProofValidator {
    pub fn new(params: ProofParams) -> Result<Self, Error> {
        Ok(Self {
            core: ProofCore::new(params)?,
        })
    }

    #[must_use]
    pub fn core(&self) -> &ProofCore {
        &self.core
    }

    /// Two x values pair into table 2 when their `match_info` relation holds and the pairing's own
    /// filter accepts them.
    #[must_use]
    pub fn validate_table_1_pair(&self, xs: &[u32; 2]) -> Option<T1Pairing> {
        let match_info_l = self.core.hashing.g(xs[0]);
        let match_info_r = self.core.hashing.g(xs[1]);
        if !self
            .core
            .validate_match_info_pairing(1, u64::from(xs[0]), match_info_l, match_info_r)
        {
            return None;
        }
        self.core.pairing_t1(xs[0], xs[1])
    }

    #[must_use]
    pub fn validate_table_2_pairs(&self, xs: &[u32; 4]) -> Option<T2Pairing> {
        let left = self.validate_table_1_pair(&[xs[0], xs[1]])?;
        let right = self.validate_table_1_pair(&[xs[2], xs[3]])?;
        if !self
            .core
            .validate_match_info_pairing(2, left.meta, left.match_info, right.match_info)
        {
            return None;
        }
        self.core.pairing_t2(left.meta, right.meta)
    }

    #[must_use]
    pub fn validate_table_3_pairs(&self, xs: &[u32; 8]) -> Option<T3Pairing> {
        let left = self.validate_table_2_pairs(&[xs[0], xs[1], xs[2], xs[3]])?;
        let right = self.validate_table_2_pairs(&[xs[4], xs[5], xs[6], xs[7]])?;
        if !self
            .core
            .validate_match_info_pairing(3, left.meta, left.match_info, right.match_info)
        {
            return None;
        }
        self.core
            .pairing_t3(left.meta, right.meta, left.x_bits, right.x_bits)
    }

    /// The whole proof. Returns the quality chain links when it holds.
    #[must_use]
    pub fn validate_full_proof(
        &self,
        proof: &[u32; TOTAL_XS_IN_PROOF],
        challenge: Bytes32,
    ) -> Option<QualityChainLinks> {
        let mut fragments = [0u64; NUM_CHAIN_LINKS];
        for (i, slot) in fragments.iter_mut().enumerate() {
            let mut xs = [0u32; 8];
            xs.copy_from_slice(&proof[i * 8..i * 8 + 8]);
            self.validate_table_3_pairs(&xs)?;
            *slot = self.core.fragment_codec.encode(&xs);
        }
        let sets = self.core.select_challenge_sets(challenge);
        if !Chainer::new(&self.core, challenge).validate(&Chain { fragments }, &sets.ranges) {
            return None;
        }
        Some(fragments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator() -> ProofValidator {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 11 + 5) as u8;
        }
        ProofValidator::new(ProofParams::new(Bytes32::from(bytes), 28, 2, false).expect("params"))
            .expect("validator")
    }

    #[test]
    fn an_arbitrary_pair_almost_never_validates() {
        // Pairing is the scarce event the whole format is built on. If random pairs validated at
        // any appreciable rate, the matching rules would not be doing their job.
        let v = validator();
        let mut paired = 0;
        for i in 0..20_000u32 {
            if v.validate_table_1_pair(&[i, i.wrapping_mul(2_654_435_761) & 0x0FFF_FFFF])
                .is_some()
            {
                paired += 1;
            }
        }
        assert!(paired < 200, "{paired} of 20000 random pairs matched");
    }

    #[test]
    fn a_random_proof_is_rejected() {
        let v = validator();
        let proof: [u32; TOTAL_XS_IN_PROOF] =
            std::array::from_fn(|i| ((i as u32).wrapping_mul(2_654_435_761)) & 0x0FFF_FFFF);
        assert!(
            v.validate_full_proof(&proof, Bytes32::from([5u8; 32]))
                .is_none()
        );
    }

    #[test]
    fn an_all_zero_proof_is_rejected() {
        let v = validator();
        assert!(
            v.validate_full_proof(&[0u32; TOTAL_XS_IN_PROOF], Bytes32::from([1u8; 32]))
                .is_none()
        );
    }

    #[test]
    fn the_levels_are_nested() {
        // A table 2 pairing can only exist when both of its table 1 pairings do, so failing to
        // find any table 1 pairing in a range means no table 2 pairing there either.
        let v = validator();
        let mut xs = [0u32; 4];
        for i in 0..2000u32 {
            xs[0] = i;
            xs[1] = i.wrapping_mul(7919) & 0x0FFF_FFFF;
            xs[2] = i.wrapping_add(1);
            xs[3] = i.wrapping_mul(104_729) & 0x0FFF_FFFF;
            if v.validate_table_2_pairs(&xs).is_some() {
                assert!(v.validate_table_1_pair(&[xs[0], xs[1]]).is_some());
                assert!(v.validate_table_1_pair(&[xs[2], xs[3]]).is_some());
            }
        }
    }
}
