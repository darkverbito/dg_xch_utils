use aes::cipher::generic_array::GenericArray;
use aes::hazmat::cipher_round;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::traits::SizedBytes;
#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::{uint8x16_t, vaeseq_u8, vaesmcq_u8, vdupq_n_u8, veorq_u8};
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{
    __m128i, _mm_aesenc_si128, _mm_cvtsi128_si32, _mm_loadu_si128, _mm_set_epi32, _mm_storeu_si128,
};

/// Round counts, ported from `src/pos/aes/AesHash.hpp`. Sixteen is the reference's tuning point:
/// fast enough for a Pi class solver, heavy enough to keep a GPU compute bound.
pub const AES_G_ROUNDS: u32 = 16;
pub const AES_PAIRING_ROUNDS: u32 = 16;
pub const AES_MATCHING_TARGET_ROUNDS: u32 = 16;
pub const AES_CHAINING_ROUNDS: u32 = 16;

/// True when the CPU carries AES instructions, checked once rather than per hash.
fn has_native_aes() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::is_x86_feature_detected!("aes")
    }
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("aes")
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// The plot's hash function: AES rounds keyed by the two halves of the plot id.
///
/// The 128 bit state is four little endian 32 bit lanes, matching `_mm_set_epi32(i3, i2, i1, i0)`
/// with `i0` in the low lane, and each round applies `aesenc` against both keys in turn.
#[derive(Debug, Clone)]
pub struct AesHash {
    key1: [u8; 16],
    key2: [u8; 16],
    k: u8,
    native: bool,
}

impl AesHash {
    #[must_use]
    pub fn new(plot_id: &Bytes32, k: u8) -> Self {
        let bytes = plot_id.bytes();
        let mut key1 = [0u8; 16];
        let mut key2 = [0u8; 16];
        key1.copy_from_slice(&bytes[0..16]);
        key2.copy_from_slice(&bytes[16..32]);
        Self {
            key1,
            key2,
            k,
            native: has_native_aes(),
        }
    }

    fn state(i0: u32, i1: u32, i2: u32, i3: u32) -> [u8; 16] {
        let mut state = [0u8; 16];
        state[0..4].copy_from_slice(&i0.to_le_bytes());
        state[4..8].copy_from_slice(&i1.to_le_bytes());
        state[8..12].copy_from_slice(&i2.to_le_bytes());
        state[12..16].copy_from_slice(&i3.to_le_bytes());
        state
    }

    fn lane(state: &[u8; 16], index: usize) -> u32 {
        let mut word = [0u8; 4];
        word.copy_from_slice(&state[index * 4..index * 4 + 4]);
        u32::from_le_bytes(word)
    }

    fn apply(&self, state: [u8; 16], rounds: u32) -> [u8; 16] {
        if self.native {
            #[cfg(target_arch = "x86_64")]
            // SAFETY: `native` is only set when the aes feature was detected at runtime.
            return unsafe { self.apply_x86(state, rounds) };
            #[cfg(target_arch = "aarch64")]
            // SAFETY: `native` is only set when the aes feature was detected at runtime.
            return unsafe { self.apply_aarch64(state, rounds) };
        }
        self.apply_portable(state, rounds)
    }

    /// The portable path: correct everywhere, but a bitsliced software round is several times the
    /// cost of the instruction, so it is a fallback rather than the plotting path.
    fn apply_portable(&self, mut state: [u8; 16], rounds: u32) -> [u8; 16] {
        let mut block = GenericArray::clone_from_slice(&state);
        let key1 = GenericArray::clone_from_slice(&self.key1);
        let key2 = GenericArray::clone_from_slice(&self.key2);
        for _ in 0..rounds {
            cipher_round(&mut block, &key1);
            cipher_round(&mut block, &key2);
        }
        state.copy_from_slice(block.as_slice());
        state
    }

    /// Keys and state stay in registers for the whole round loop, which is what makes this
    /// latency bound on `aesenc` rather than on moving bytes.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "aes")]
    unsafe fn apply_x86(&self, state: [u8; 16], rounds: u32) -> [u8; 16] {
        unsafe {
            let key1 = _mm_loadu_si128(self.key1.as_ptr().cast::<__m128i>());
            let key2 = _mm_loadu_si128(self.key2.as_ptr().cast::<__m128i>());
            let mut block = _mm_loadu_si128(state.as_ptr().cast::<__m128i>());
            for _ in 0..rounds {
                block = _mm_aesenc_si128(block, key1);
                block = _mm_aesenc_si128(block, key2);
            }
            let mut out = [0u8; 16];
            _mm_storeu_si128(out.as_mut_ptr().cast::<__m128i>(), block);
            out
        }
    }

    /// `aesenc(state, key)` is `MixColumns(SubBytes(ShiftRows(state))) ^ key`; on aarch64 that is
    /// `vaeseq_u8` against a zero key, then `vaesmcq_u8`, then the key xor.
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "aes")]
    unsafe fn apply_aarch64(&self, state: [u8; 16], rounds: u32) -> [u8; 16] {
        unsafe {
            let zero = vdupq_n_u8(0);
            let key1 = std::ptr::read_unaligned(self.key1.as_ptr().cast::<uint8x16_t>());
            let key2 = std::ptr::read_unaligned(self.key2.as_ptr().cast::<uint8x16_t>());
            let mut block = std::ptr::read_unaligned(state.as_ptr().cast::<uint8x16_t>());
            for _ in 0..rounds {
                block = veorq_u8(vaesmcq_u8(vaeseq_u8(block, zero)), key1);
                block = veorq_u8(vaesmcq_u8(vaeseq_u8(block, zero)), key2);
            }
            let mut out = [0u8; 16];
            std::ptr::write_unaligned(out.as_mut_ptr().cast::<uint8x16_t>(), block);
            out
        }
    }

    /// Mask for the low `k` bits. A `k` of 32 or more keeps the whole word rather than shifting by
    /// the word width.
    fn k_mask(&self) -> u32 {
        if self.k >= 32 {
            u32::MAX
        } else {
            (1u32 << self.k) - 1
        }
    }

    /// `g` over many x values at once, writing `match_info` for each into `out`.
    ///
    /// Plotting calls `g` once per x across the whole `2^k` space, and each call is independent.
    /// Hashing a block of them inside one call lets the processor keep several `aesenc` chains in
    /// flight, which turns a latency bound loop into a throughput bound one. A single `g_x` cannot
    /// do this: the instruction path lives behind a `target_feature` boundary the optimiser will
    /// not inline through, so every scalar call serialises on its own dependency chain.
    ///
    /// # Panics
    /// If `out` is shorter than `xs`.
    pub fn g_x_batch(&self, xs: &[u32], out: &mut [u32], rounds: u32) {
        assert!(
            out.len() >= xs.len(),
            "output buffer is shorter than the input"
        );
        #[cfg(target_arch = "x86_64")]
        if self.native {
            // SAFETY: `native` is only set when the aes feature was detected at runtime.
            unsafe { self.g_x_batch_x86(xs, out, rounds) };
            return;
        }
        for (x, slot) in xs.iter().zip(out.iter_mut()) {
            *slot = self.g_x(*x, rounds);
        }
    }

    /// Eight lanes is enough to cover `aesenc` latency on the processors this runs on without
    /// spilling the round state out of registers.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "aes")]
    unsafe fn g_x_batch_x86(&self, xs: &[u32], out: &mut [u32], rounds: u32) {
        const LANES: usize = 8;
        unsafe {
            let key1 = _mm_loadu_si128(self.key1.as_ptr().cast::<__m128i>());
            let key2 = _mm_loadu_si128(self.key2.as_ptr().cast::<__m128i>());
            let mask = self.k_mask();
            let mut i = 0;
            while i + LANES <= xs.len() {
                let mut block: [__m128i; LANES] =
                    std::array::from_fn(|j| _mm_set_epi32(0, 0, 0, xs[i + j] as i32));
                for _ in 0..rounds {
                    for b in &mut block {
                        *b = _mm_aesenc_si128(*b, key1);
                    }
                    for b in &mut block {
                        *b = _mm_aesenc_si128(*b, key2);
                    }
                }
                for (j, b) in block.iter().enumerate() {
                    out[i + j] = (_mm_cvtsi128_si32(*b) as u32) & mask;
                }
                i += LANES;
            }
            while i < xs.len() {
                out[i] = self.g_x(xs[i], rounds);
                i += 1;
            }
        }
    }

    /// `g(x)`: the function that turns an x value into its `match_info`.
    #[must_use]
    pub fn g_x(&self, x: u32, rounds: u32) -> u32 {
        let state = self.apply(Self::state(x, 0, 0, 0), rounds);
        Self::lane(&state, 0) & self.k_mask()
    }

    /// The target a left entry projects onto, which a right entry's `match_target` must equal.
    ///
    /// `extra_rounds_bits` multiplies the work by `1 << bits`; table 1 uses it to spend the plot's
    /// strength, which is what makes plotting expensive without costing verification.
    #[must_use]
    pub fn matching_target(
        &self,
        table_id: u32,
        match_key: u32,
        meta: u64,
        extra_rounds_bits: u32,
    ) -> u32 {
        let state = Self::state(table_id, match_key, meta as u32, (meta >> 32) as u32);
        let rounds = AES_MATCHING_TARGET_ROUNDS << extra_rounds_bits;
        Self::lane(&self.apply(state, rounds), 0)
    }

    /// The four lane result a pairing produces: match info, two words of metadata, and test bits.
    #[must_use]
    pub fn pairing(&self, meta_l: u64, meta_r: u64, extra_rounds_bits: u32) -> [u32; 4] {
        let state = Self::state(
            meta_l as u32,
            (meta_l >> 32) as u32,
            meta_r as u32,
            (meta_r >> 32) as u32,
        );
        let rounds = AES_PAIRING_ROUNDS << extra_rounds_bits;
        let state = self.apply(state, rounds);
        [
            Self::lane(&state, 0),
            Self::lane(&state, 1),
            Self::lane(&state, 2),
            Self::lane(&state, 3),
        ]
    }

    /// The hash that links proof fragments into a quality chain.
    #[must_use]
    pub fn chain(&self, input: u64) -> u64 {
        let state = Self::state(input as u32, (input >> 32) as u32, 0, 0);
        let state = self.apply(state, AES_CHAINING_ROUNDS);
        u64::from(Self::lane(&state, 0)) | (u64::from(Self::lane(&state, 1)) << 32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference's regression plot id: `plot_id[i] = i * 11 + 5`, used at k28.
    fn regression_plot_id() -> Bytes32 {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 11 + 5) as u8;
        }
        Bytes32::from(bytes)
    }

    /// Reproduces `aes_regression_results` from the reference's `tests/test_aes.cpp`, in order.
    fn regression_results(hasher: &AesHash) -> Vec<u32> {
        let mut out = Vec::with_capacity(41);
        for x in [0u32, 1, 0x1234_5678, 0xFFFF_FFFF, 0xABCD_EF12] {
            out.push(hasher.g_x(x, AES_G_ROUNDS));
        }
        for extra_bits in [0u32, 1] {
            for meta in [0u64, 0x0123_4567_89AB_CDEF, 0xFEDC_BA98_7654_3210] {
                out.push(hasher.matching_target(1, 0xDEAD_BEEF, meta, extra_bits));
                out.push(hasher.matching_target(3, 0x0123_ABCD, meta, extra_bits));
            }
        }
        for extra_bits in [0u32, 1] {
            for (l, r) in [
                (0x0123_4567_89AB_CDEFu64, 0x0FED_CBA9_8765_4321u64),
                (0, 0),
                (0xFFFF_FFFF_FFFF_FFFF, 0xAAAA_AAAA_AAAA_AAAA),
            ] {
                out.extend_from_slice(&hasher.pairing(l, r, extra_bits));
            }
        }
        out
    }

    /// Frozen from the reference implementation's `kAesRegression` in `tests/aes_test_cases.hpp`.
    const AES_REGRESSION: [u32; 41] = [
        127_783_594,
        124_767_263,
        141_145_580,
        258_627_148,
        11_512_430,
        409_340_410,
        505_582_378,
        1_721_018_924,
        1_480_140_290,
        1_396_524_303,
        3_587_190_239,
        707_841_484,
        1_331_347_346,
        1_241_879_891,
        2_167_726_916,
        3_067_597_756,
        4_168_169_983,
        1_091_708_360,
        2_175_255_815,
        2_816_383_768,
        1_674_980_125,
        2_543_702_698,
        4_091_426_003,
        533_075_521,
        3_859_141_200,
        31_044_209,
        4_179_457_918,
        1_030_061_401,
        3_699_883_668,
        210_961_197,
        1_476_679_550,
        3_006_735_961,
        939_518_466,
        1_218_571_309,
        716_491_999,
        1_747_602_127,
        1_064_749_683,
        1_584_340_891,
        3_071_410_499,
        4_118_871_486,
        1_400_922_689,
    ];

    #[test]
    fn the_hash_matches_the_reference_regression_vector() {
        // Bit for bit conformance with the reference implementation. Every later stage of pos2
        // rests on these bytes, so this is the test that decides whether the port is real.
        let hasher = AesHash::new(&regression_plot_id(), 28);
        let got = regression_results(&hasher);
        assert_eq!(got.len(), AES_REGRESSION.len());
        for (i, (got, want)) in got.iter().zip(AES_REGRESSION.iter()).enumerate() {
            assert_eq!(got, want, "regression value {i} diverged");
        }
    }

    #[test]
    fn the_native_and_portable_paths_agree() {
        // The instruction path is only safe to use because it produces the same bytes as the
        // portable one, so assert that rather than trusting it.
        let hasher = AesHash::new(&regression_plot_id(), 28);
        for x in [0u32, 1, 0x1234_5678, 0xFFFF_FFFF] {
            let state = AesHash::state(x, 0, 0, 0);
            assert_eq!(
                hasher.apply(state, AES_G_ROUNDS),
                hasher.apply_portable(state, AES_G_ROUNDS),
                "native and portable diverged for x {x}"
            );
        }
    }

    #[test]
    fn the_batch_and_scalar_paths_agree() {
        let hasher = AesHash::new(&regression_plot_id(), 28);
        // Deliberately not a multiple of the lane count, so the tail is exercised too.
        let xs: Vec<u32> = (0..37u32).map(|i| i.wrapping_mul(0x9E37_79B9)).collect();
        let mut out = vec![0u32; xs.len()];
        hasher.g_x_batch(&xs, &mut out, AES_G_ROUNDS);
        for (i, x) in xs.iter().enumerate() {
            assert_eq!(
                out[i],
                hasher.g_x(*x, AES_G_ROUNDS),
                "batch diverged at {i}"
            );
        }
    }

    #[test]
    fn g_x_is_masked_to_k_bits() {
        for k in [16u8, 20, 28, 30] {
            let hasher = AesHash::new(&regression_plot_id(), k);
            for x in [0u32, 1, 0xDEAD_BEEF] {
                assert!(
                    hasher.g_x(x, AES_G_ROUNDS) < (1u32 << k),
                    "k {k} leaked bits above the mask"
                );
            }
        }
    }

    #[test]
    fn extra_rounds_change_the_result() {
        let hasher = AesHash::new(&regression_plot_id(), 28);
        assert_ne!(
            hasher.matching_target(1, 7, 99, 0),
            hasher.matching_target(1, 7, 99, 1)
        );
        assert_ne!(hasher.pairing(1, 2, 0), hasher.pairing(1, 2, 1));
    }

    #[test]
    fn the_keys_are_the_two_halves_of_the_plot_id() {
        let a = AesHash::new(&regression_plot_id(), 28);
        let mut swapped = regression_plot_id().bytes();
        swapped.swap(0, 16);
        let b = AesHash::new(&Bytes32::from(swapped), 28);
        assert_ne!(a.g_x(1, AES_G_ROUNDS), b.g_x(1, AES_G_ROUNDS));
    }

    #[test]
    fn chaining_is_deterministic_and_input_sensitive() {
        let hasher = AesHash::new(&regression_plot_id(), 28);
        assert_eq!(hasher.chain(12345), hasher.chain(12345));
        assert_ne!(hasher.chain(12345), hasher.chain(12346));
    }
}
