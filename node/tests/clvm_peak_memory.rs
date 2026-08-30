// Peak-memory gate for the CLVM VM.
//
// `soak_clvm_memory.rs` proves the VM RETAINS nothing across runs. That is a different property
// from how much it holds DURING one run, and a leak-free allocator can still have an enormous
// high-water mark: a bump allocator frees nothing until end-of-run, so every eval intermediate
// stays live and peak grows with the program's total allocation rather than its live set. Window
// validation then multiplies that peak by the worker count, which is what puts a 4 GiB machine at
// risk even though steady-state RSS is flat.
//
// So this gate measures the HIGH-WATER of a single generator run and holds it under a ceiling.
// The workload is a cost-maxed mainnet generator (~10.9 of the 11B cost limit): the heaviest thing
// consensus permits in one block, and the shape that stresses allocation hardest.
//
// Fixtures are committed (`core/tests/fixtures/heavy_generators/`), so this runs in the normal
// `cargo test` gate — no corpus, no DB, no network, no chain sync.

use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, execute_block_generator_result,
};
use dg_xch_core::consensus::constants::MAINNET;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// System allocator that tracks live bytes and their high-water mark. Exact — unlike a sampling
/// profiler or jemalloc's cached stats, there is no arena or purge noise to threshold around.
struct Tracking;

impl Tracking {
    fn bump(delta: usize) {
        let live = LIVE.fetch_add(delta, Ordering::Relaxed) + delta;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        Self::bump(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        Self::bump(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        if new_size > l.size() {
            Self::bump(new_size - l.size());
        } else {
            LIVE.fetch_sub(l.size() - new_size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, l, new_size) }
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

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

/// The ceiling one cost-maxed generator run may reach.
///
/// The measured high-water for these blocks on the compact arena is ~5 MiB. The bumpalo tree arena
/// this replaced peaked at 429 MiB on a comparable dust-era run because it retained every eval
/// intermediate. 64 MiB sits an order of magnitude above the current cost and an order below the
/// old one, so it tolerates allocator and platform variation while still failing decisively on a
/// return to retain-everything allocation.
const PEAK_CEILING_BYTES: usize = 64 * 1024 * 1024;

fn generator_of(hex: &str) -> Vec<u8> {
    let hex = hex.trim();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("generator hex"))
        .collect()
}

fn run_one(hex: &str, height: u32) -> (u64, usize, usize) {
    let input = BlockGeneratorInput {
        transactions_generator: generator_of(hex).into(),
        generator_refs: Vec::new(),
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::for_height(&MAINNET, height),
    };
    // Settle lazy one-time allocations before baselining so they are not charged to the run.
    let _ = execute_block_generator_result(&input);

    let base_live = LIVE.load(Ordering::Relaxed);
    PEAK.store(base_live, Ordering::Relaxed);
    let conds = execute_block_generator_result(&input).expect("cost-maxed generator runs");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base_live);
    (conds.cost, conds.spends.len(), peak)
}

#[test]
fn a_cost_maxed_generator_run_stays_under_the_peak_ceiling() {
    for (name, hex, declared_cost) in HEAVY {
        let height: u32 = name.parse().expect("height");
        let (cost, spends, peak) = run_one(hex, height);

        assert_eq!(
            cost, declared_cost,
            "height {name}: computed cost {cost} != the cost this block declared on chain \
             ({declared_cost}) — the VM is not producing consensus-identical results"
        );
        assert!(
            spends > 0,
            "height {name}: a cost-maxed generator produced no spends"
        );
        assert!(
            peak <= PEAK_CEILING_BYTES,
            "height {name}: one generator run peaked at {peak} B ({:.1} MiB), over the {} MiB \
             ceiling. A leak-free allocator can still hold every eval intermediate until \
             end-of-run; that is what puts window validation over a 4 GiB machine. Do NOT raise \
             this ceiling to make a change pass — the peak is the property being defended.",
            peak as f64 / (1024.0 * 1024.0),
            PEAK_CEILING_BYTES / (1024 * 1024)
        );
        eprintln!(
            "  h={name}: cost={cost} spends={spends} peak={:.2} MiB",
            peak as f64 / (1024.0 * 1024.0)
        );
    }
}

#[test]
fn the_decompression_and_ref_shapes_stay_under_the_ceiling() {
    // The backref/ROM decompression path and the ref-resolving path allocate differently from a
    // plain run — decompression materializes the expanded program — so their peaks are pinned
    // separately from the cost-maxed shape.
    use dg_xch_core::clvm::program::SerializedProgram;
    use dg_xch_core::consensus::block_generator::GeneratorReference;

    let shapes: [(&str, &str, u32, Vec<GeneratorReference>); 2] = [
        (
            "compressed-834752",
            include_str!("../../core/tests/fixtures/chia_generator_tests/block-834752-compressed.txt"),
            834_752,
            vec![],
        ),
        (
            "with-ref-4671894",
            include_str!("../../core/tests/fixtures/chia_generator_tests/block-4671894.txt"),
            4_671_894,
            vec![GeneratorReference {
                height: 4_671_893,
                index: 0,
                generator: SerializedProgram::from_hex(
                    include_str!(
                        "../../core/tests/fixtures/chia_generator_tests/block-4671894.env"
                    )
                    .trim(),
                )
                .expect("ref generator"),
            }],
        ),
    ];
    for (name, hex, height, refs) in shapes {
        let input = BlockGeneratorInput {
            transactions_generator: generator_of(hex).into(),
            generator_refs: refs,
            constants: MAINNET,
            height,
            flags: BlockGeneratorFlags::for_height(&MAINNET, height),
        };
        let _ = execute_block_generator_result(&input);

        let base_live = LIVE.load(Ordering::Relaxed);
        PEAK.store(base_live, Ordering::Relaxed);
        let conds = execute_block_generator_result(&input).expect("generator runs");
        let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base_live);
        eprintln!(
            "  {name}: spends={} peak={:.2} MiB",
            conds.spends.len(),
            peak as f64 / (1024.0 * 1024.0)
        );
        assert!(!conds.spends.is_empty(), "{name}: produced no spends");
        assert!(
            peak <= PEAK_CEILING_BYTES,
            "{name}: peaked at {:.1} MiB, over the {} MiB ceiling",
            peak as f64 / (1024.0 * 1024.0),
            PEAK_CEILING_BYTES / (1024 * 1024)
        );
    }
}

#[test]
fn concurrent_window_validation_peak_scales_with_workers_not_blocks() {
    // Window validation runs several generators at once. Peak must scale with the WORKER count,
    // not the window size — an allocator that retains intermediates makes every concurrent run
    // hold its whole allocation history at the same moment, which is the multiplication that
    // OOM-killed a 4 GiB node.
    let base_live = LIVE.load(Ordering::Relaxed);
    PEAK.store(base_live, Ordering::Relaxed);

    std::thread::scope(|s| {
        for (name, hex, _) in HEAVY {
            s.spawn(move || {
                let height: u32 = name.parse().expect("height");
                let input = BlockGeneratorInput {
                    transactions_generator: generator_of(hex).into(),
                    generator_refs: Vec::new(),
                    constants: MAINNET,
                    height,
                    flags: BlockGeneratorFlags::for_height(&MAINNET, height),
                };
                let _ = execute_block_generator_result(&input);
            });
        }
    });

    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base_live);
    let ceiling = PEAK_CEILING_BYTES * HEAVY.len();
    assert!(
        peak <= ceiling,
        "three concurrent cost-maxed runs peaked at {:.1} MiB, over the {} MiB ceiling — \
         concurrent peak is not bounded by the per-run cost",
        peak as f64 / (1024.0 * 1024.0),
        ceiling / (1024 * 1024)
    );
    eprintln!(
        "  3 concurrent: peak={:.2} MiB",
        peak as f64 / (1024.0 * 1024.0)
    );
}
