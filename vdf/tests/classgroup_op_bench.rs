// Seconds-fast micro-benchmark of the class-group hot ops (square / multiply) on a real
// 1024-bit-discriminant form — the inner loop of VDF verification and therefore of weight-proof
// validation. Run this (seconds), not the full weight-proof validation, while iterating on the
// arithmetic.
//
//   cargo test --release -p dg_xch_vdf --test classgroup_op_bench -- --nocapture

use std::time::Instant;

use dg_xch_vdf::create_discriminant_bytes;
use dg_xch_vdf::form::{Form, fast_pow_form};
use num_bigint::{BigInt, Sign};

const ITERS: u32 = 2_000;

// Walk a realistic operand distribution: keep squaring (as fast_pow_form does), folding in a
// multiply every few steps so both ops are exercised on evolving reduced forms.
#[test]
fn classgroup_square_and_multiply_micro_bench() {
    // The discriminant is negative (imaginary quadratic field): bytes are the magnitude.
    let bytes = create_discriminant_bytes(b"classgroup-op-bench-seed", 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let generator = Form::generator(&discriminant).expect("generator");

    // Advance off the generator so operands look like mid-verification forms.
    let mut x = generator.clone();
    for _ in 0..64 {
        x = x.square().expect("warmup square");
    }

    let start = Instant::now();
    let mut s = x.clone();
    for _ in 0..ITERS {
        s = s.square().expect("square");
    }
    let square_total = start.elapsed();

    let start = Instant::now();
    let mut m = x.clone();
    for _ in 0..ITERS {
        m = m.multiply(&x).expect("multiply");
    }
    let multiply_total = start.elapsed();

    // A realistic mixed exponentiation for the end-to-end per-op blend.
    let exponent = BigInt::from(1_000_003u64);
    let start = Instant::now();
    let p = fast_pow_form(&x, &discriminant, &exponent).expect("pow");
    let pow_total = start.elapsed();
    assert!(p.a.bits() > 0, "use the result");

    println!(
        "CLASSGROUP-OP-BENCH: square={:.2}us/op multiply={:.2}us/op pow(2^20ish)={:.1}ms",
        square_total.as_secs_f64() * 1e6 / f64::from(ITERS),
        multiply_total.as_secs_f64() * 1e6 / f64::from(ITERS),
        pow_total.as_secs_f64() * 1e3,
    );
}

// Per-segment overhead outside the class-group ops: get_b = hash_prime(264-bit) prime search.
#[test]
fn get_b_micro_bench() {
    use dg_xch_vdf::form::get_b;
    let bytes = create_discriminant_bytes(b"classgroup-op-bench-seed", 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let g = Form::generator(&discriminant).expect("generator");
    let mut x = g.clone();
    for _ in 0..8 {
        x = x.square().expect("square");
    }
    let y = x.square().expect("square");
    let start = std::time::Instant::now();
    let n = 50u32;
    for _ in 0..n {
        let b = get_b(&discriminant, &x, &y).expect("get_b");
        assert!(b.bits() > 200);
    }
    let el = start.elapsed();
    println!(
        "GETB-BENCH: {:.2}ms/op",
        el.as_secs_f64() * 1e3 / f64::from(n)
    );
}
