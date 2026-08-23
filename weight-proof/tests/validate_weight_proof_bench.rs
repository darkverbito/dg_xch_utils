// Offline weight-proof validation harness — the perf-optimization anchor. Loads a real committed mainnet
// weight proof and runs the full six-phase `validate_weight_proof` with NO live peer, reporting the total
// wall time and the phase-4 shape (sampled sub-epochs / VDFs) that dominates it. Phase-4 sub-epoch-segment
// VDF verification (class-group modular exponentiation) is the hot path; this is the fixed, deterministic
// target to profile against and to re-measure an optimization on.
//
// Run (release, single-threaded output visible):
//   cargo test --release -p dg_xch_weight_proof --test validate_weight_proof_bench -- --nocapture
//
// Profile the hot path in the dev env by running the same binary under `perf record` / a sampling profiler;
// the VDF verification runs on rayon workers inside phase 4.

mod common;

use std::time::Instant;

use common::load_fixture;
use dg_xch_core::consensus::constants::MAINNET;
use dg_xch_weight_proof::validate_weight_proof;

// Validator + optimizer anchor: proves the real mainnet weight proof validates offline, and prints the wall
// time so an optimization can be measured before/after against the same fixture.
#[test]
fn validate_real_mainnet_weight_proof_offline_and_report_timing() {
    let wp = load_fixture();
    let recent = wp.recent_chain_data.len();

    let start = Instant::now();
    let (valid, summaries) =
        validate_weight_proof(&wp, &MAINNET).expect("real mainnet weight proof validates offline");
    let elapsed = start.elapsed();

    println!(
        "OFFLINE-WP-VALIDATE: valid={valid} recent_chain={recent} summaries={} total={:.3}s",
        summaries.len(),
        elapsed.as_secs_f64()
    );
    assert!(
        valid,
        "the committed real mainnet weight proof must validate"
    );
}
