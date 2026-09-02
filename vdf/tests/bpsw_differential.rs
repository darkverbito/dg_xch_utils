use dg_xch_vdf::testing::{is_probable_prime_native, is_probable_prime_reference};
use num_bigint::BigUint;

fn oracle(n: &BigUint) -> bool {
    is_probable_prime_reference(n)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn agrees_on_every_small_integer() {
    for n in 0u32..100_000 {
        let n = BigUint::from(n);
        assert_eq!(is_probable_prime_native(&n), oracle(&n), "diverged at {n}");
    }
}

#[test]
fn agrees_on_the_pseudoprime_families() {
    // Strong pseudoprimes to base 2 (would fool MR-2 alone), Lucas pseudoprimes (would fool
    // the Lucas half alone), Carmichael numbers, perfect squares, and the first BPSW
    // stress values from the literature.
    let cases: &[u64] = &[
        2047,
        3277,
        4033,
        4681,
        8321,
        15841,
        29341,
        42799,
        49141,
        52633,
        65281,
        74665,
        80581,
        85489,
        88357,
        90751, // strong psp base 2
        323,
        377,
        1159,
        1829,
        3827,
        5459,
        5777,
        9071,
        9179,
        10877,
        11419,
        11663,
        13919,
        14839,
        16109,
        16211,
        18407,
        18971,
        19043, // Lucas pseudoprimes
        561,
        1105,
        1729,
        2465,
        2821,
        6601,
        8911,
        10585,
        15841,
        29341,
        41041,
        46657,
        52633,
        62745,
        63973,
        75361, // Carmichael
        25,
        49,
        121,
        169,
        289,
        361,
        529,
        841,
        961,
        1024,
        1048576,             // squares and powers
        3825123056546413051, // strong psp to bases 2,3,5,7,11,13,17,19,23
    ];
    for &c in cases {
        let n = BigUint::from(c);
        assert_eq!(is_probable_prime_native(&n), oracle(&n), "diverged at {c}");
    }
}

#[test]
fn agrees_on_candidate_shaped_inputs() {
    // The exact shapes hash_prime tests: 264-bit (get_b) and 1024-bit (discriminant) odd
    // integers with the search's bitmask applied, plus near-power-of-two edges.
    let scale: u64 = std::env::var("BPSW_DIFFERENTIAL_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut checked = 0u64;
    for bits in [264usize, 1024] {
        let bytes = bits / 8;
        for _ in 0..(2_000 * scale) {
            let mut blob = vec![0u8; bytes];
            for b in &mut blob {
                *b = rng.next() as u8;
            }
            let mut n = BigUint::from_bytes_be(&blob);
            n.set_bit(0, true);
            n.set_bit((bits - 1) as u64, true);
            assert_eq!(
                is_probable_prime_native(&n),
                oracle(&n),
                "diverged at a {bits}-bit candidate: {n}"
            );
            checked += 1;
        }
    }
    // Near 2^k edges: dense carry/overflow territory for modular ladders.
    for k in [64u64, 127, 128, 255, 256, 263, 264, 511, 512] {
        let base = BigUint::from(1u8) << k;
        for delta in 1u8..40 {
            for n in [&base + delta, &base - delta] {
                assert_eq!(
                    is_probable_prime_native(&n),
                    oracle(&n),
                    "diverged near 2^{k} (delta {delta})"
                );
                checked += 1;
            }
        }
    }
    eprintln!("  bpsw differential: {checked} candidates agreed");
}

#[test]
fn fixed_limb_miller_rabin_matches_the_bigint_ladder() {
    // Layer 2: the Montgomery fixed-limb MR-2 against the bigint MR-2 it replaces on the hot
    // width. Same scale knob; every odd shape up to 5 limbs plus the exact fit boundaries.
    use dg_xch_vdf::testing::{miller_rabin_base2_bigint, miller_rabin_base2_fixed};
    let scale: u64 = std::env::var("BPSW_DIFFERENTIAL_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let mut rng = Rng(0xB5AD_4ECE_DA1C_E2A9);
    let mut checked = 0u64;
    for bits in [64usize, 128, 192, 256, 264, 300, 319, 320] {
        for _ in 0..(3_000 * scale) {
            let bytes = bits.div_ceil(8);
            let mut blob = vec![0u8; bytes];
            for b in &mut blob {
                *b = rng.next() as u8;
            }
            let mut n = BigUint::from_bytes_be(&blob);
            n.set_bit(0, true);
            n.set_bit((bits - 1) as u64, true);
            let Some(fixed) = miller_rabin_base2_fixed(&n) else {
                panic!("{bits}-bit candidate did not fit the fixed path");
            };
            assert_eq!(
                fixed,
                miller_rabin_base2_bigint(&n),
                "MR-2 diverged at a {bits}-bit candidate: {n}"
            );
            checked += 1;
        }
    }
    // Small odds exhaustively: every branch of the s-loop and the tiny-d cases.
    for n in (3u64..20_000).step_by(2) {
        let n = BigUint::from(n);
        assert_eq!(
            miller_rabin_base2_fixed(&n).unwrap(),
            miller_rabin_base2_bigint(&n),
            "MR-2 diverged at {n}"
        );
        checked += 1;
    }
    eprintln!("  fixed-vs-bigint MR2: {checked} candidates agreed");
}
