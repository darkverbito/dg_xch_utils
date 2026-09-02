use crate::pos2::aes_hash::{AES_G_ROUNDS, AesHash};
use crate::pos2::blake_hash::{block_with_plot_id, hash_block_256, words_from_bytes32};
use crate::pos2::constants::{NUM_CHAIN_LINKS, TESTNET_G_XOR_CONST};
use crate::pos2::params::ProofParams;
use dg_xch_core::blockchain::sized_bytes::Bytes32;

/// One pairing's outputs: the next table's `match_info`, its metadata, and the filter bits that
/// decide whether the pairing survives at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairingResult {
    pub match_info: u32,
    pub meta: u64,
    pub test: u32,
}

fn mask32(bits: u32) -> u32 {
    if bits >= 32 {
        u32::MAX
    } else {
        (1u32 << bits) - 1
    }
}

fn mask64(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

/// The hashing layer between the plot parameters and the raw AES and BLAKE3 primitives.
#[derive(Debug, Clone)]
pub struct ProofHashing {
    params: ProofParams,
    aes: AesHash,
}

impl ProofHashing {
    #[must_use]
    pub fn new(params: ProofParams) -> Self {
        let aes = AesHash::new(&params.plot_id(), params.k());
        Self { params, aes }
    }

    #[must_use]
    pub fn aes(&self) -> &AesHash {
        &self.aes
    }

    /// Table 1 spends the plot's strength here as extra hashing rounds. Later tables do not, which
    /// is why strength costs the plotter but not the verifier.
    fn extra_rounds_bits(&self, table_id: u32) -> u32 {
        if table_id == 1 {
            u32::from(self.params.strength()) - 2
        } else {
            0
        }
    }

    /// `g(x)`: an x value's `match_info`. Testnet plots fold in a constant so they cannot be farmed
    /// on mainnet.
    #[must_use]
    pub fn g(&self, x: u32) -> u32 {
        let x = if self.params.is_testnet() {
            x ^ TESTNET_G_XOR_CONST
        } else {
            x
        };
        self.aes.g_x(x, AES_G_ROUNDS)
    }

    #[must_use]
    pub fn matching_target(
        &self,
        table_id: u32,
        match_key: u32,
        meta: u64,
        num_target_bits: u32,
    ) -> u32 {
        self.aes
            .matching_target(table_id, match_key, meta, self.extra_rounds_bits(table_id))
            & mask32(num_target_bits)
    }

    fn split(
        lanes: [u32; 4],
        num_match_info_bits: u32,
        out_num_meta_bits: u32,
        num_test_bits: u32,
    ) -> PairingResult {
        let meta = u64::from(lanes[1]) | (u64::from(lanes[2]) << 32);
        PairingResult {
            match_info: lanes[0] & mask32(num_match_info_bits),
            meta: meta & mask64(out_num_meta_bits),
            test: lanes[3] & mask32(num_test_bits),
        }
    }

    #[must_use]
    pub fn pairing_t1(
        &self,
        meta_l: u64,
        meta_r: u64,
        num_match_info_bits: u32,
        out_num_meta_bits: u32,
        num_test_bits: u32,
    ) -> PairingResult {
        let lanes = self.aes.pairing(meta_l, meta_r, self.extra_rounds_bits(1));
        Self::split(lanes, num_match_info_bits, out_num_meta_bits, num_test_bits)
    }

    #[must_use]
    pub fn pairing_t2(
        &self,
        meta_l: u64,
        meta_r: u64,
        num_match_info_bits: u32,
        out_num_meta_bits: u32,
        num_test_bits: u32,
    ) -> PairingResult {
        let lanes = self.aes.pairing(meta_l, meta_r, 0);
        Self::split(lanes, num_match_info_bits, out_num_meta_bits, num_test_bits)
    }

    /// Table 3 only needs the filter bits: its output is a proof fragment, not another pairing.
    #[must_use]
    pub fn pairing_t3(&self, meta_l: u64, meta_r: u64, num_test_bits: u32) -> PairingResult {
        let lanes = self.aes.pairing(meta_l, meta_r, 0);
        PairingResult {
            match_info: 0,
            meta: 0,
            test: lanes[3] & mask32(num_test_bits),
        }
    }

    /// The plot id and challenge hashed together. Plots sharing a group share this, so they share
    /// their selected challenge sets.
    #[must_use]
    pub fn challenge_with_plot_id_hash(&self, challenge: Bytes32) -> [u32; 8] {
        let challenge_words = words_from_bytes32(challenge);
        hash_block_256(&block_with_plot_id(self.params.plot_id(), &challenge_words))
    }

    /// One 64 bit challenge per chain link, produced by re-hashing the previous digest back into
    /// the high half of the block while the plot id stays in the low half.
    #[must_use]
    pub fn chaining_challenge_with_plot_id_hash(
        &self,
        challenge: Bytes32,
    ) -> [u64; NUM_CHAIN_LINKS] {
        let mut block = block_with_plot_id(self.params.plot_id(), &words_from_bytes32(challenge));
        let mut digest = hash_block_256(&block);
        let mut out = [0u64; NUM_CHAIN_LINKS];
        for i in 0..4 {
            out[i] = u64::from(digest[i * 2]) | (u64::from(digest[i * 2 + 1]) << 32);
        }
        for c in 1..NUM_CHAIN_LINKS / 4 {
            block[8..16].copy_from_slice(&digest);
            digest = hash_block_256(&block);
            for i in 0..4 {
                out[c * 4 + i] = u64::from(digest[i * 2]) | (u64::from(digest[i * 2 + 1]) << 32);
            }
        }
        out
    }

    /// The hash that advances a quality chain from one link to the next.
    #[must_use]
    pub fn chain_hash(&self, input: u64) -> u64 {
        self.aes.chain(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(strength: u8, testnet: bool) -> ProofParams {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 11 + 5) as u8;
        }
        ProofParams::new(Bytes32::from(bytes), 28, strength, testnet).expect("params")
    }

    #[test]
    fn testnet_and_mainnet_g_differ() {
        let main = ProofHashing::new(params(2, false));
        let test = ProofHashing::new(params(2, true));
        assert_ne!(main.g(1234), test.g(1234));
        // Mainnet g is the plain hash, with no constant folded in.
        assert_eq!(main.g(1234), main.aes().g_x(1234, AES_G_ROUNDS));
        assert_eq!(
            test.g(1234),
            test.aes().g_x(1234 ^ TESTNET_G_XOR_CONST, AES_G_ROUNDS)
        );
    }

    #[test]
    fn strength_only_adds_rounds_on_table_one() {
        let weak = ProofHashing::new(params(2, false));
        let strong = ProofHashing::new(params(6, false));
        // Table 1 pairings and targets move with strength.
        assert_ne!(
            weak.matching_target(1, 5, 99, 20),
            strong.matching_target(1, 5, 99, 20)
        );
        // Later tables do not.
        assert_eq!(
            weak.matching_target(3, 5, 99, 20),
            strong.matching_target(3, 5, 99, 20)
        );
        assert_eq!(
            weak.pairing_t2(1, 2, 28, 56, 2),
            strong.pairing_t2(1, 2, 28, 56, 2)
        );
    }

    #[test]
    fn a_pairing_is_split_into_its_three_fields() {
        let h = ProofHashing::new(params(2, false));
        let r = h.pairing_t2(0x1234, 0x5678, 28, 56, 2);
        assert!(r.match_info < (1 << 28), "match info exceeded k bits");
        assert!(r.meta < (1u64 << 56), "meta exceeded 2k bits");
        assert!(r.test < 4, "test bits exceeded their width");
    }

    #[test]
    fn table_three_reports_only_filter_bits() {
        let h = ProofHashing::new(params(2, false));
        let r = h.pairing_t3(7, 9, 2);
        assert_eq!(r.match_info, 0);
        assert_eq!(r.meta, 0);
        assert!(r.test < 4);
        // It shares the underlying pairing with table 2, so the filter bits agree.
        assert_eq!(r.test, h.pairing_t2(7, 9, 28, 56, 2).test);
    }

    #[test]
    fn every_chain_link_gets_its_own_challenge() {
        let h = ProofHashing::new(params(2, false));
        let links = h.chaining_challenge_with_plot_id_hash(Bytes32::from([3u8; 32]));
        let mut seen = std::collections::HashSet::new();
        for link in links {
            assert!(seen.insert(link), "chain link challenge repeated");
        }
        // Deterministic, and bound to the challenge.
        assert_eq!(
            links,
            h.chaining_challenge_with_plot_id_hash(Bytes32::from([3u8; 32]))
        );
        assert_ne!(
            links,
            h.chaining_challenge_with_plot_id_hash(Bytes32::from([4u8; 32]))
        );
    }
}
