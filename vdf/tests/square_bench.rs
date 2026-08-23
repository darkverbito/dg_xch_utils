use dg_xch_vdf::discriminant::create_discriminant_bytes;
use dg_xch_vdf::form::{Form, nucomp_bound};
use num_bigint::{BigInt, Sign};
use std::time::Instant;

// Decompose the class-group squaring cost (the trace-identified ~330ms/proof hotspot): full
// square_with per-op time on the real 1024-bit mainnet-style discriminant, held against the
// generator form evolving as verification would evolve it. Run:
//   cargo test --release -p dg_xch_vdf --test square_bench -- --ignored --nocapture
#[test]
#[ignore = "timing tool"]
fn decompose_squaring_cost() {
    let challenge = [7u8; 32];
    let bytes = create_discriminant_bytes(&challenge, 1024).expect("discriminant");
    // The stored form is the negated magnitude of a negative discriminant.
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let l = nucomp_bound(&discriminant);
    let mut f = Form::generator(&discriminant).expect("generator");

    // Warm + evolve to a representative operand.
    for _ in 0..64 {
        f = f.square_with(&discriminant, &l).expect("square");
    }

    const N: u32 = 2000;
    let start = Instant::now();
    for _ in 0..N {
        f = f.square_with(&discriminant, &l).expect("square");
    }
    let per_op = start.elapsed() / N;
    eprintln!("SQUARE: {per_op:?}/op over {N} ops");

    // A verification does ~2 * ceil(iters-bits) squarings-equivalent per fast_pow; report the
    // implied proof cost for the observed per-op time at a typical 33-bit exponent budget.
    let ops_per_verify = 2_000u32; // order-of-magnitude: two ~1000-op fast_pow walks
    eprintln!(
        "implied verify cost at ~{ops_per_verify} ops: {:?}",
        per_op * ops_per_verify
    );
}

#[test]
#[ignore = "timing tool"]
fn decompose_verify_cost() {
    use dg_xch_vdf::form::{fast_pow_form, get_b};
    let challenge = [7u8; 32];
    let bytes = create_discriminant_bytes(&challenge, 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let l = nucomp_bound(&discriminant);
    let mut x = Form::generator(&discriminant).expect("generator");
    for _ in 0..64 {
        x = x.square_with(&discriminant, &l).expect("square");
    }
    let mut y = x.clone();
    for _ in 0..64 {
        y = y.square_with(&discriminant, &l).expect("square");
    }

    const N: u32 = 50;
    let start = Instant::now();
    let mut b = num_bigint::BigInt::from(0);
    for _ in 0..N {
        b = get_b(&discriminant, &x, &y).expect("get_b");
        // perturb inputs so each get_b runs a fresh prime search
        x = x.square_with(&discriminant, &l).expect("square");
    }
    eprintln!("GET_B: {:?}/op over {N} ops", start.elapsed() / N);

    let start = Instant::now();
    const M: u32 = 20;
    for _ in 0..M {
        let _ = fast_pow_form(&x, &discriminant, &b).expect("pow");
    }
    eprintln!(
        "FAST_POW(264-bit): {:?}/op over {M} ops (exponent bits {})",
        start.elapsed() / M,
        b.bits()
    );
}

// A/B for the Wesolowski pair product witness^b · x^r — the serial verify path's dominant term:
// two independent single-exponent chains + composition (the pre-fusion shape) against the fused
// Straus/Shamir shared-squaring chain, result-checked against each other every iteration. Run:
//   cargo test --release -p dg_xch_vdf --test square_bench pair_pow -- --ignored --nocapture
#[test]
#[ignore = "timing tool"]
fn pair_pow_fused_vs_two_chains() {
    use dg_xch_vdf::form::{fast_pow_form_pair_with, fast_pow_form_with, get_b};
    let challenge = [7u8; 32];
    let bytes = create_discriminant_bytes(&challenge, 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let l = nucomp_bound(&discriminant);
    let mut x = Form::generator(&discriminant).expect("generator");
    for _ in 0..64 {
        x = x.square_with(&discriminant, &l).expect("square");
    }
    let mut w = x.clone();
    for _ in 0..64 {
        w = w.square_with(&discriminant, &l).expect("square");
    }
    let y = w.square_with(&discriminant, &l).expect("square");
    // Real Wesolowski-shaped exponents: b from the Fiat-Shamir prime search (264-bit, top bit
    // set), r = 2^iters mod b at a mainnet-scale iteration count.
    let b = get_b(&discriminant, &x, &y).expect("get_b");
    let r = {
        let mut acc = BigInt::from(2u8);
        for _ in 0..27 {
            acc = (&acc * &acc) % &b; // 2^(2^27) mod b — a full-width r
        }
        acc
    };

    const N: u32 = 20;
    let start = Instant::now();
    let mut composed = None;
    for _ in 0..N {
        let f1 = fast_pow_form_with(&w, &discriminant, &l, &b).expect("pow w^b");
        let f2 = fast_pow_form_with(&x, &discriminant, &l, &r).expect("pow x^r");
        composed = Some(f1.multiply_with(&f2, &discriminant, &l).expect("compose"));
    }
    let two_chains = start.elapsed() / N;

    let start = Instant::now();
    let mut fused = None;
    for _ in 0..N {
        fused =
            Some(fast_pow_form_pair_with(&w, &b, &x, &r, &discriminant, &l).expect("fused pair"));
    }
    let fused_t = start.elapsed() / N;
    assert_eq!(
        composed.expect("ran"),
        fused.expect("ran"),
        "fused pair result diverged from the two-chain composition"
    );
    eprintln!(
        "PAIR-POW: two-chains={two_chains:?}/op  fused={fused_t:?}/op  ratio {:.3}x (b bits {}, r bits {})",
        fused_t.as_nanos() as f64 / two_chains.as_nanos() as f64,
        b.bits(),
        r.bits()
    );
}

// A/B: GMP (rug) NUDUPL vs the fixed-limb SwWide path, correctness-checked against each other on
// every iteration. Run:
//   cargo test --release -p dg_xch_vdf --test square_bench gmp_vs_limbs -- --ignored --nocapture
#[test]
#[ignore = "timing tool"]
fn gmp_vs_limbs() {
    use dg_xch_vdf::gmp_form::{GForm, to_rug};
    let challenge = [7u8; 32];
    let bytes = create_discriminant_bytes(&challenge, 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let l = nucomp_bound(&discriminant);
    let gl = to_rug(&l);
    let mut f = Form::generator(&discriminant).expect("generator");
    for _ in 0..64 {
        f = f.square_with(&discriminant, &l).expect("square");
    }
    let mut g = GForm::from_form(&f);

    // Correctness: 256 squarings must agree form-for-form.
    let mut fx = f.clone();
    for i in 0..256 {
        fx = fx.square_with(&discriminant, &l).expect("limb square");
        g = g.square_with(&gl).expect("gmp square");
        assert_eq!(g.to_form(), fx, "divergence at squaring {i}");
    }
    eprintln!("correctness: 256 squarings agree");

    const N: u32 = 2000;
    let start = Instant::now();
    let mut fb = fx.clone();
    for _ in 0..N {
        fb = fb.square_with(&discriminant, &l).expect("square");
    }
    let limb = start.elapsed() / N;
    let mut gb = GForm::from_form(&fx);
    let start = Instant::now();
    for _ in 0..N {
        gb = gb.square_with(&gl).expect("square");
    }
    let gmp = start.elapsed() / N;
    eprintln!(
        "LIMB: {limb:?}/op  GMP: {gmp:?}/op  ratio {:.2}x",
        limb.as_nanos() as f64 / gmp.as_nanos() as f64
    );
}

// Third contender: the preallocated-scratch GMP path (chiavdf-style in-place mpz ops).
#[test]
#[ignore = "timing tool"]
fn gmp_scratch_vs_limbs() {
    use dg_xch_vdf::gmp_form::{GForm, GScratch, to_rug};
    let challenge = [7u8; 32];
    let bytes = create_discriminant_bytes(&challenge, 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let l = nucomp_bound(&discriminant);
    let gl = to_rug(&l);
    let mut f = Form::generator(&discriminant).expect("generator");
    for _ in 0..64 {
        f = f.square_with(&discriminant, &l).expect("square");
    }
    let mut g = GForm::from_form(&f);
    let mut scratch = GScratch::default();

    let mut fx = f.clone();
    for i in 0..256 {
        fx = fx.square_with(&discriminant, &l).expect("limb square");
        g.square_with_scratch(&gl, &mut scratch)
            .expect("scratch square");
        assert_eq!(g.to_form(), fx, "divergence at squaring {i}");
    }
    eprintln!("correctness: 256 scratch squarings agree");

    const N: u32 = 2000;
    let start = Instant::now();
    let mut fb = fx.clone();
    for _ in 0..N {
        fb = fb.square_with(&discriminant, &l).expect("square");
    }
    let limb = start.elapsed() / N;
    let mut gb = GForm::from_form(&fx);
    let start = Instant::now();
    for _ in 0..N {
        gb.square_with_scratch(&gl, &mut scratch).expect("square");
    }
    let gmp = start.elapsed() / N;
    eprintln!(
        "LIMB: {limb:?}/op  GMP-SCRATCH: {gmp:?}/op  ratio {:.2}x",
        limb.as_nanos() as f64 / gmp.as_nanos() as f64
    );
}
