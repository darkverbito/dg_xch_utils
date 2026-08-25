use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::traits::SizedBytes;
use std::io::{Error, ErrorKind};

/// Feistel rounds the reference uses unless told otherwise.
pub const FEISTEL_ROUNDS: u32 = 4;

/// Low `bits` set, saturating rather than shifting by the word width.
fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

/// The block cipher that turns bit dropped x values into a proof fragment, ported from
/// `src/pos/FeistelCipher.hpp`.
///
/// The block is `2k` bits split into two `k` bit halves, and each round mixes them with a
/// ChaCha20 style quarter round keyed by a `3k` bit slice of the plot id.
#[derive(Debug, Clone)]
pub struct FeistelCipher {
    plot_id: [u8; 32],
    k: u32,
    rounds: u32,
}

impl FeistelCipher {
    pub fn new(plot_id: Bytes32, k: u32) -> Result<Self, Error> {
        Self::with_rounds(plot_id, k, FEISTEL_ROUNDS)
    }

    pub fn with_rounds(plot_id: Bytes32, k: u32, rounds: u32) -> Result<Self, Error> {
        if k > 32 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("k {k} cannot exceed 32"),
            ));
        }
        if 3 * k > 256 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("3k for k {k} cannot exceed 256 bits"),
            ));
        }
        Ok(Self {
            plot_id: plot_id.bytes(),
            k,
            rounds,
        })
    }

    #[must_use]
    pub fn k(&self) -> u32 {
        self.k
    }

    fn rotate_left(value: u64, shift: u32, bit_length: u32) -> u64 {
        let shift = shift.min(bit_length);
        let m = mask(bit_length);
        ((value << shift) & m) | (value >> (bit_length - shift))
    }

    /// A big endian slice of the plot id. The reference builds the segment in a 64 bit register, so
    /// a slice wider than 64 bits keeps only its low word; that truncation is part of the format.
    fn slice_key(&self, start_bit: usize, num_bits: usize) -> u64 {
        let start_byte = start_bit / 8;
        let bit_offset = start_bit % 8;
        let needed_bytes = (bit_offset + num_bits).div_ceil(8);
        if start_byte + needed_bytes > 32 {
            return 0;
        }
        let mut segment: u64 = 0;
        for i in 0..needed_bytes {
            segment = (segment << 8) | u64::from(self.plot_id[start_byte + i]);
        }
        let total_bits = needed_bytes * 8;
        let shift_amount = total_bits - bit_offset - num_bits;
        (segment >> shift_amount) & mask(u32::try_from(num_bits).unwrap_or(64))
    }

    fn round_key(&self, round: usize) -> u64 {
        let bits_for_round = 3 * self.k as usize;
        let start_bit = if self.rounds > 1 {
            round * (256 - bits_for_round) / (self.rounds as usize - 1)
        } else {
            0
        };
        self.slice_key(start_bit, bits_for_round)
    }

    fn round(&self, left: u64, right: u64, round_key: u64) -> (u64, u64) {
        let m = mask(self.k);
        // `wrapping_shr` mirrors the shift-modulo-width the reference relies on when `2k` reaches
        // the register width at k32.
        let mut a = right;
        let mut b = round_key & m;
        let mut c = round_key.wrapping_shr(self.k) & m;
        let mut d = round_key.wrapping_shr(2 * self.k) & m;

        a = a.wrapping_add(b) & m;
        d = Self::rotate_left(d ^ a, 16, self.k);
        c = c.wrapping_add(d) & m;
        b = Self::rotate_left(b ^ c, 12, self.k);

        a = a.wrapping_add(b) & m;
        d = Self::rotate_left(d ^ a, 8, self.k);
        c = c.wrapping_add(d) & m;
        b = Self::rotate_left(b ^ c, 7, self.k);

        (right, (left ^ b) & m)
    }

    #[must_use]
    pub fn encrypt(&self, input: u64) -> u64 {
        let m = mask(self.k);
        let mut left = input.wrapping_shr(self.k) & m;
        let mut right = input & m;
        for round in 0..self.rounds as usize {
            let key = self.round_key(round);
            let (l, r) = self.round(left, right, key);
            left = l;
            right = r;
        }
        (left << self.k) | right
    }

    #[must_use]
    pub fn decrypt(&self, cipher: u64) -> u64 {
        let m = mask(self.k);
        let mut left = cipher.wrapping_shr(self.k) & m;
        let mut right = cipher & m;
        for round in (0..self.rounds as usize).rev() {
            let key = self.round_key(round);
            // The inverse of a round is the same round with the halves swapped.
            let (l, r) = self.round(right, left, key);
            right = l;
            left = r;
        }
        (left << self.k) | right
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

    /// `(k, input, ciphertext)` emitted from the reference `FeistelCipher` for the plot id above.
    const FEISTEL_VECTORS: &[(u32, u64, u64)] = &[
        (28, 0, 680_663_352_959_931),
        (28, 1, 53_950_537_582_426_653),
        (28, 1_250_999_896_491, 63_157_282_695_857_520),
        (28, 281_474_976_710_655, 26_361_481_967_541_154),
        (28, 12_345_678_901, 29_541_298_939_039_514),
        (30, 0, 1_064_829_078_371_294_044),
        (30, 1, 75_867_245_716_127_150),
        (30, 1_250_999_896_491, 395_489_400_676_364_053),
        (30, 281_474_976_710_655, 16_514_633_143_281_432),
        (30, 12_345_678_901, 257_764_385_613_865_671),
        (32, 0, 10_416_083_265_746_737_936),
        (32, 1, 10_621_545_006_909_718_083),
        (32, 1_250_999_896_491, 9_532_783_856_750_979_935),
        (32, 281_474_976_710_655, 13_712_526_740_761_935_299),
        (32, 12_345_678_901, 10_459_832_581_459_965_203),
    ];

    #[test]
    fn encryption_matches_the_reference_vectors() {
        for (i, (k, input, expected)) in FEISTEL_VECTORS.iter().enumerate() {
            let cipher = FeistelCipher::new(plot_id(), *k).expect("valid k");
            let block = if 2 * k >= 64 {
                *input
            } else {
                *input & ((1u64 << (2 * k)) - 1)
            };
            assert_eq!(
                cipher.encrypt(block),
                *expected,
                "vector {i} (k{k}) diverged"
            );
        }
    }

    #[test]
    fn decryption_inverts_encryption() {
        for k in [28u32, 30, 32] {
            let cipher = FeistelCipher::new(plot_id(), k).expect("valid k");
            let block_mask = if 2 * k >= 64 {
                u64::MAX
            } else {
                (1u64 << (2 * k)) - 1
            };
            for v in [0u64, 1, 42, 1_250_999_896_491, 0xDEAD_BEEF_CAFE] {
                let block = v & block_mask;
                assert_eq!(cipher.decrypt(cipher.encrypt(block)), block, "k{k} v{v}");
            }
        }
    }

    #[test]
    fn a_different_plot_id_gives_a_different_ciphertext() {
        let a = FeistelCipher::new(plot_id(), 28).expect("valid k");
        let b = FeistelCipher::new(Bytes32::from([9u8; 32]), 28).expect("valid k");
        assert_ne!(a.encrypt(12345), b.encrypt(12345));
    }

    #[test]
    fn oversized_k_is_refused() {
        assert!(FeistelCipher::new(plot_id(), 33).is_err());
    }
}
