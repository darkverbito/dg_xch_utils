#![cfg(feature = "phase-profile")]
use dg_xch_vdf::discriminant::create_discriminant_bytes;
use dg_xch_vdf::form::{Form, nucomp_bound, phase_profile};
use num_bigint::{BigInt, Sign};
use std::time::Instant;

// Phase decomposition of square_with on the real 1024-bit discriminant — the measurement that
// targets the fixed-limb assembly work. Run:
//   cargo test --release -p dg_xch_vdf --features phase-profile --test phase_profile -- --ignored --nocapture
#[test]
#[ignore = "timing tool"]
fn phase_split() {
    let challenge = [7u8; 32];
    let bytes = create_discriminant_bytes(&challenge, 1024).expect("discriminant");
    let discriminant = -BigInt::from_bytes_be(Sign::Plus, &bytes);
    let l = nucomp_bound(&discriminant);
    let mut f = Form::generator(&discriminant).expect("generator");
    for _ in 0..64 {
        f = f.square_with(&discriminant, &l).expect("square");
    }
    let _ = phase_profile::take();

    const N: u32 = 4000;
    let start = Instant::now();
    for _ in 0..N {
        f = f.square_with(&discriminant, &l).expect("square");
    }
    let total = start.elapsed().as_nanos() as u64;
    let acc = phase_profile::take();
    let sum: u64 = acc.iter().sum();
    eprintln!(
        "total {:.2}us/op (instrumented {:.2}us/op)",
        total as f64 / f64::from(N) / 1000.0,
        sum as f64 / f64::from(N) / 1000.0
    );
    for (name, ns) in phase_profile::PHASES.iter().zip(acc.iter()) {
        eprintln!(
            "  {name:<22} {:>8.2}us/op  {:>5.1}%",
            *ns as f64 / f64::from(N) / 1000.0,
            *ns as f64 / sum as f64 * 100.0
        );
    }
}
