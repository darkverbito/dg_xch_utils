use crate::pos2::feistel::FeistelCipher;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use std::io::Error;

/// A proof fragment: `2k` bits of ciphertext standing in for eight x values.
pub type ProofFragment = u64;

/// Encodes eight x values into a proof fragment, ported from `src/pos/ProofFragment.hpp`.
///
/// Only the top half of every other x survives: `x1`, `x3`, `x5` and `x7` contribute their high
/// `k/2` bits and the rest are dropped. That is what makes a stored fragment smaller than the proof
/// it stands for, and why recovering a full proof from fragments needs a solver.
#[derive(Debug, Clone)]
pub struct ProofFragmentCodec {
    cipher: FeistelCipher,
}

impl ProofFragmentCodec {
    pub fn new(plot_id: Bytes32, k: u32) -> Result<Self, Error> {
        Ok(Self {
            cipher: FeistelCipher::new(plot_id, k)?,
        })
    }

    #[must_use]
    pub fn k(&self) -> u32 {
        self.cipher.k()
    }

    fn half_k(&self) -> u32 {
        self.cipher.k() / 2
    }

    /// Pack the surviving halves of `x1, x3, x5, x7` into the `2k` bit block the cipher takes.
    #[must_use]
    pub fn pack(&self, x_values: &[u32; 8]) -> u64 {
        let half = self.half_k();
        let mut bits = 0u64;
        for (slot, index) in [0usize, 2, 4, 6].into_iter().enumerate() {
            let dropped = u64::from(x_values[index] >> half);
            bits |= dropped << (half * (3 - slot as u32));
        }
        bits
    }

    #[must_use]
    pub fn encode(&self, x_values: &[u32; 8]) -> ProofFragment {
        self.cipher.encrypt(self.pack(x_values))
    }

    #[must_use]
    pub fn encode_bits(&self, all_x_bits: u64) -> ProofFragment {
        self.cipher.encrypt(all_x_bits)
    }

    #[must_use]
    pub fn decode(&self, fragment: ProofFragment) -> u64 {
        self.cipher.decrypt(fragment)
    }

    /// The four surviving x halves a fragment carries, in `x1, x3, x5, x7` order.
    #[must_use]
    pub fn x_bits(&self, fragment: ProofFragment) -> [u32; 4] {
        let half = self.half_k();
        let decrypted = self.decode(fragment);
        let mask = (1u64 << half) - 1;
        std::array::from_fn(|slot| ((decrypted >> (half * (3 - slot as u32))) & mask) as u32)
    }

    /// Whether a fragment really stands for these eight x values.
    #[must_use]
    pub fn validates(&self, fragment: ProofFragment, x_values: &[u32; 8]) -> bool {
        let half = self.half_k();
        let recovered = self.x_bits(fragment);
        [0usize, 2, 4, 6]
            .into_iter()
            .enumerate()
            .all(|(slot, index)| x_values[index] >> half == recovered[slot])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot_id() -> Bytes32 {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 11 + 5) as u8;
        }
        Bytes32::from(bytes)
    }

    fn sample_xs(k: u32) -> [u32; 8] {
        let mask = (1u32 << k) - 1;
        std::array::from_fn(|i| ((i as u32) * 7919 + 13) & mask)
    }

    #[test]
    fn encoding_matches_the_reference_vectors() {
        // Emitted from the reference `ProofFragmentCodec::encode` for the same plot id and inputs.
        assert_eq!(
            ProofFragmentCodec::new(plot_id(), 28)
                .expect("codec")
                .encode(&sample_xs(28)),
            27_747_203_251_943_674
        );
        assert_eq!(
            ProofFragmentCodec::new(plot_id(), 30)
                .expect("codec")
                .encode(&sample_xs(30)),
            75_867_245_716_127_150
        );
    }

    #[test]
    fn a_fragment_recovers_the_surviving_halves() {
        for k in [28u32, 30] {
            let codec = ProofFragmentCodec::new(plot_id(), k).expect("codec");
            let xs = sample_xs(k);
            let fragment = codec.encode(&xs);
            let half = k / 2;
            assert_eq!(
                codec.x_bits(fragment),
                [xs[0] >> half, xs[2] >> half, xs[4] >> half, xs[6] >> half],
                "k{k}"
            );
            assert!(codec.validates(fragment, &xs), "k{k}");
        }
    }

    #[test]
    fn the_dropped_x_values_do_not_change_the_fragment() {
        // x2, x4, x6 and x8 contribute nothing, and neither do the low halves of the others.
        let codec = ProofFragmentCodec::new(plot_id(), 28).expect("codec");
        let mut xs = sample_xs(28);
        let before = codec.encode(&xs);
        xs[1] ^= 0xFFF;
        xs[3] ^= 0xFFF;
        xs[0] ^= 1;
        assert_eq!(codec.encode(&xs), before);
    }

    #[test]
    fn a_changed_surviving_half_changes_the_fragment() {
        let codec = ProofFragmentCodec::new(plot_id(), 28).expect("codec");
        let mut xs = sample_xs(28);
        let before = codec.encode(&xs);
        xs[4] ^= 1 << 20;
        assert_ne!(codec.encode(&xs), before);
        assert!(!codec.validates(before, &xs));
    }
}
