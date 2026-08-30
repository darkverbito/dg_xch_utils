// Throughput gate for the CLVM VM.
//
// `core/benches/clvm.rs` (criterion) is the precise instrument, but it is not a gate — it never
// fails. This is the coarse regression guard that runs in `cargo test`: it validates a cost-maxed
// mainnet generator many times and asserts the per-run wall time stays under a ceiling, so a
// change that halves throughput trips CI instead of being noticed on a live node.
//
// The metric is cost/second, not seconds/block: the workload is fixed to cost-maxed generators
// (~10.9 of the 11B limit) so wall time is directly comparable across runs, and cost/second is the
// figure that actually bounds sync rate. The ceiling is a wall-clock floor on throughput, set well
// below measured hardware so it is not flaky, but far above a return to the old tree-arena rate.
//
// Fixtures are committed — no corpus, no chain sync. Timing gates are machine-dependent, so this is
// `#[ignore]` by default (CI runners vary too much for a hard wall-clock assert) and run explicitly
// on a known machine: `cargo test -p dg_xch_node --release --test clvm_throughput -- --ignored --nocapture`.

use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, execute_block_generator_result,
};
use dg_xch_core::consensus::constants::MAINNET;
use std::time::Instant;

const HEAVY: [(&str, &str, u64); 3] = [
    (
        "9189472",
        include_str!("../../core/tests/fixtures/heavy_generators/block-9189472.txt"),
        10_917_602_437,
    ),
    (
        "9189475",
        include_str!("../../core/tests/fixtures/heavy_generators/block-9189475.txt"),
        10_756_176_962,
    ),
    (
        "9189481",
        include_str!("../../core/tests/fixtures/heavy_generators/block-9189481.txt"),
        10_786_676_230,
    ),
];

/// Minimum cost/second a release build must sustain on the reference cluster builder (Xeon
/// E5-2690 v2, no SHA-NI). Measured throughput there is ~0.65 Gcost/s (a ~10.9B block in ~16.7s
/// after the ring sha256 change). The floor sits at 0.30 Gcost/s — under half the measured rate,
/// so platform and load variation do not trip it, but a return to the pre-ring or pre-arena rate
/// (which took ~26s/block, ~0.42 Gcost/s, and the tree-arena slower still) fails decisively.
const MIN_GCOST_PER_SEC: f64 = 0.30;

fn generator_of(hex: &str) -> Vec<u8> {
    let hex = hex.trim();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("generator hex"))
        .collect()
}

#[test]
#[ignore = "wall-clock gate; run explicitly on a known machine in release"]
fn cost_maxed_generator_throughput_holds_the_floor() {
    for (name, hex, declared_cost) in HEAVY {
        let height: u32 = name.parse().expect("height");
        let input = BlockGeneratorInput {
            transactions_generator: generator_of(hex).into(),
            generator_refs: Vec::new(),
            constants: MAINNET,
            height,
            flags: BlockGeneratorFlags::for_height(&MAINNET, height),
        };

        // Warm caches, then confirm the result is consensus-identical before timing.
        let conds = execute_block_generator_result(&input).expect("generator runs");
        assert_eq!(
            conds.cost, declared_cost,
            "height {name}: computed cost != the on-chain declared cost; not consensus-identical"
        );

        const REPS: u32 = 3;
        let start = Instant::now();
        for _ in 0..REPS {
            let c = execute_block_generator_result(&input).expect("generator runs");
            std::hint::black_box(c.cost);
        }
        let per_run = start.elapsed().as_secs_f64() / f64::from(REPS);
        let gcost_per_sec = declared_cost as f64 / per_run / 1e9;

        eprintln!("  h={name}: {per_run:.2}s/run  {gcost_per_sec:.3} Gcost/s");
        assert!(
            gcost_per_sec >= MIN_GCOST_PER_SEC,
            "height {name}: {gcost_per_sec:.3} Gcost/s is below the {MIN_GCOST_PER_SEC} Gcost/s \
             floor ({per_run:.2}s for a cost-maxed block) — a throughput regression. Do NOT lower \
             the floor to make a change pass; the floor is the property being defended."
        );
    }
}
