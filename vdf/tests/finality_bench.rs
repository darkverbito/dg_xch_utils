// Executable finality: every performance verdict in docs/algorithmic-finality.md, reproduced
// in one command on the current machine, both contenders in the same binary.
//
//   cargo test --release -p dg_xch_vdf --test finality_bench -- --ignored --nocapture
//
// Each probe prints both sides and the ratio. A verdict in the document is falsified the day
// a probe here disagrees with it on a deployment target — which is the point: the claims stay
// measurable, never archival.

use dg_xch_vdf::testing::{
    is_probable_prime_native, is_probable_prime_reference, miller_rabin_base2_bigint,
    miller_rabin_base2_fixed,
};
use num_bigint::BigUint;
use std::time::Instant;

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

fn candidates(count: usize, bits: usize, seed: u64) -> Vec<BigUint> {
    let mut rng = Rng(seed);
    (0..count)
        .map(|_| {
            let mut blob = vec![0u8; bits / 8];
            for b in &mut blob {
                *b = rng.next() as u8;
            }
            let mut n = BigUint::from_bytes_be(&blob);
            n.set_bit(0, true);
            n.set_bit((bits - 1) as u64, true);
            n
        })
        .collect()
}

/// The GMP-crossover verdict (finality doc §1b): the reference primality test against the
/// verdict-identical native Baillie–PSW, on the get_b candidate shape. The document records
/// the native side losing 2.4× (Xeon) and 2.6× (A72); this probe re-measures that claim
/// wherever it runs.
#[test]
#[ignore = "manual finality probe"]
fn primality_reference_vs_native() {
    let cands = candidates(600, 264, 0x9E37_79B9_7F4A_7C15);
    // Verdict identity first — a speed comparison between disagreeing functions is void.
    for n in &cands {
        assert_eq!(
            is_probable_prime_native(n),
            is_probable_prime_reference(n),
            "verdicts diverged at {n}; the speed comparison below would be meaningless"
        );
    }
    let t = Instant::now();
    let mut acc = 0u32;
    for n in &cands {
        acc += u32::from(is_probable_prime_reference(n));
    }
    let reference = t.elapsed();
    let t = Instant::now();
    let mut acc2 = 0u32;
    for n in &cands {
        acc2 += u32::from(is_probable_prime_native(n));
    }
    let native = t.elapsed();
    assert_eq!(acc, acc2);
    eprintln!(
        "primality 264-bit: reference {:?}/op, native {:?}/op, native/reference = {:.2}x",
        reference / cands.len() as u32,
        native / cands.len() as u32,
        native.as_secs_f64() / reference.as_secs_f64(),
    );
}

/// The Montgomery-ladder half of the same verdict in isolation: fixed-limb MR-2 against the
/// bigint MR-2. Isolates the kernel question from the Lucas/screen mix above.
#[test]
#[ignore = "manual finality probe"]
fn miller_rabin_fixed_vs_bigint() {
    let cands = candidates(2_000, 264, 0xB5AD_4ECE_DA1C_E2A9);
    for n in &cands {
        assert_eq!(
            miller_rabin_base2_fixed(n).expect("fits"),
            miller_rabin_base2_bigint(n),
            "MR-2 diverged at {n}"
        );
    }
    let t = Instant::now();
    let mut acc = 0u32;
    for n in &cands {
        acc += u32::from(miller_rabin_base2_fixed(n).expect("fits"));
    }
    let fixed = t.elapsed();
    let t = Instant::now();
    let mut acc2 = 0u32;
    for n in &cands {
        acc2 += u32::from(miller_rabin_base2_bigint(n));
    }
    let bigint = t.elapsed();
    assert_eq!(acc, acc2);
    eprintln!(
        "MR-2 264-bit: fixed {:?}/op, bigint {:?}/op, fixed/bigint = {:.2}x",
        fixed / cands.len() as u32,
        bigint / cands.len() as u32,
        fixed.as_secs_f64() / bigint.as_secs_f64(),
    );
}
