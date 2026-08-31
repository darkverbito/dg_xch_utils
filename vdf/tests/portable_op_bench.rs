// A/B microbenchmark for the two portable hot paths this PR touches: the 264-bit hash_prime
// search (the get_b shape — the whole live GMP cost) and the multi-limb division behind the
// Lehmer reductions. Wall-clock per-op, before/after on the same machine.
//
//   cargo test --release -p dg_xch_vdf --test portable_op_bench -- --ignored --nocapture

use dg_xch_vdf::create_discriminant_bytes;
use std::time::Instant;

#[test]
#[ignore = "manual A/B benchmark"]
fn bench_264_bit_prime_search() {
    // 264 bits is the get_b width; distinct seeds defeat the discriminant cache so every op is
    // the full candidate walk (hash → screen → BPSW).
    let mut seeds: Vec<[u8; 32]> = Vec::new();
    for i in 0u64..200 {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
        s[8] = 0xB7;
        seeds.push(s);
    }
    for s in &seeds[..8] {
        let _ = create_discriminant_bytes(s, 264);
    }
    let t = Instant::now();
    for s in &seeds {
        let _ = create_discriminant_bytes(s, 264).expect("derives");
    }
    let per = t.elapsed() / seeds.len() as u32;
    eprintln!(
        "264-bit prime search: {per:?}/op over {} seeds",
        seeds.len()
    );
}
