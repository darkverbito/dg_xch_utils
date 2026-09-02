use dg_xch_core::clvm::assemble::assemble_text;
use dg_xch_core::clvm::program::{Program, SerializedProgram};
use dg_xch_core::clvm::runtime::ClvmRuntime;
use dg_xch_core::clvm::sexp::{AtomBuf, SExp};
use dg_xch_core::consensus::block_generator::{
    BlockGeneratorFlags, BlockGeneratorInput, execute_block_generator_result,
};
use dg_xch_core::consensus::constants::{ConsensusConstants, MAINNET};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

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

const HEAVY_49_SPENDS: &str =
    include_str!("../../core/tests/fixtures/heavy_generators/block-9189472.txt");

fn generator_of(hex_src: &str) -> SerializedProgram {
    SerializedProgram::from_hex(hex_src.lines().next().expect("fixture has content").trim())
        .expect("generator fixture is valid hex")
}

fn clamped_constants(max_block_cost_clvm: u64) -> ConsensusConstants {
    let mut c = MAINNET;
    c.max_block_cost_clvm = max_block_cost_clvm;
    c
}

fn measure<F: Fn()>(soak_iters: usize, run: F) -> (usize, usize) {
    run();
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    run();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);

    let base = LIVE.load(Ordering::Relaxed);
    for _ in 0..soak_iters {
        run();
    }
    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(base) / soak_iters;
    (retained, peak)
}

fn assert_released(label: &str, retained: usize) {
    assert!(
        retained <= 64,
        "{label}: retained {retained} B/run — a refused/finished run does not release. Rejected \
         inputs are attacker-triggerable and unbounded, so retention here is an OOM lever."
    );
}

#[test]
fn a_real_generator_clamped_below_its_cost_is_refused_and_releases() {
    let _guard = MEASUREMENT_LOCK.lock().expect("measurement lock");
    let generator = generator_of(HEAVY_49_SPENDS);
    let constants = clamped_constants(50_000_000);
    let input = BlockGeneratorInput {
        transactions_generator: generator,
        generator_refs: vec![],
        constants,
        height: 9_189_472,
        flags: BlockGeneratorFlags::for_height(&constants, 9_189_472),
    };
    let full = execute_block_generator_result(&input);
    assert!(
        full.is_err(),
        "a 10.9B-cost block validated under a 50M ceiling without error"
    );

    let (retained, peak) = measure(20, || {
        let _ = execute_block_generator_result(&input);
    });
    eprintln!(
        "  clamped generator (50M ceiling): retained={retained} B/run peak={:.2} MiB",
        peak as f64 / (1024.0 * 1024.0)
    );
    assert_released("clamped generator", retained);
    assert!(
        peak < 64 * 1024 * 1024,
        "refusing at a 50M-cost ceiling allocated {:.1} MiB — not proportional to the ceiling; \
         the arena is holding intermediates a bounded run should have released",
        peak as f64 / (1024.0 * 1024.0)
    );
}

#[test]
fn the_same_generator_at_a_range_of_ceilings_never_leaks() {
    let _guard = MEASUREMENT_LOCK.lock().expect("measurement lock");
    let generator = generator_of(HEAVY_49_SPENDS);
    for ceiling in [1_000_000u64, 100_000_000, 1_000_000_000] {
        let constants = clamped_constants(ceiling);
        let input = BlockGeneratorInput {
            transactions_generator: generator.clone(),
            generator_refs: vec![],
            constants,
            height: 9_189_472,
            flags: BlockGeneratorFlags::for_height(&constants, 9_189_472),
        };
        let (retained, peak) = measure(6, || {
            let _ = execute_block_generator_result(&input);
        });
        eprintln!(
            "  ceiling {ceiling}: retained={retained} B/run peak={:.2} MiB",
            peak as f64 / (1024.0 * 1024.0)
        );
        assert_released(&format!("ceiling {ceiling}"), retained);
        assert!(
            peak < 128 * 1024 * 1024,
            "ceiling {ceiling}: peaked at {:.1} MiB",
            peak as f64 / (1024.0 * 1024.0)
        );
    }
}

#[test]
fn realistic_nesting_depth_parses_runs_and_drops_cleanly() {
    let _guard = MEASUREMENT_LOCK.lock().expect("measurement lock");
    const DEPTH: usize = 4096;
    let mut blob = Vec::with_capacity(DEPTH * 2 + 1);
    for _ in 0..DEPTH {
        blob.extend_from_slice(&[0xff, 0x01]);
    }
    blob.push(0x80);

    let parse = || {
        let serialized = SerializedProgram::from(blob.clone());
        serialized.to_program().is_ok()
    };
    assert!(parse(), "a {DEPTH}-deep list should parse");
    let (retained, peak) = measure(20, || {
        parse();
    });
    eprintln!(
        "  realistic depth {DEPTH}: retained={retained} B/run peak={:.2} MiB",
        peak as f64 / (1024.0 * 1024.0)
    );
    assert_released("realistic-depth parse", retained);
}

#[test]
fn oversized_atom_headers_are_refused_without_preallocating() {
    let _guard = MEASUREMENT_LOCK.lock().expect("measurement lock");
    for claimed in [u32::MAX as u64, 8 * 1024 * 1024 * 1024] {
        let mut blob = vec![0xfc];
        blob.extend_from_slice(&claimed.to_be_bytes()[3..]);
        blob.extend_from_slice(b"tiny");

        let parse = || {
            let serialized = SerializedProgram::from(blob.clone());
            serialized.to_program().is_ok()
        };
        let err = !parse();
        let (retained, peak) = measure(50, || {
            parse();
        });
        eprintln!(
            "  atom header claiming {claimed} B: err={err} retained={retained} B/run peak={:.3} MiB",
            peak as f64 / (1024.0 * 1024.0)
        );
        assert!(err, "an atom claiming {claimed} B from 4 real bytes parsed");
        assert_released(&format!("atom header {claimed}"), retained);
        assert!(
            peak < 8 * 1024 * 1024,
            "refusing a {claimed} B claim allocated {:.1} MiB first — the length is trusted \
             before the bytes exist",
            peak as f64 / (1024.0 * 1024.0)
        );
    }
}

#[test]
fn a_reused_runtime_releases_between_programs() {
    let _guard = MEASUREMENT_LOCK.lock().expect("measurement lock");
    let programs: Vec<Program<'static>> = [
        "(* (q . 123456) (q . 123456))",
        "(sha256 (q . 0x4142434445464748))",
        "(c (q . 1) (c (q . 2) (q . ())))",
        "(concat (q . \"aa\") (q . \"bb\") (q . \"cc\"))",
    ]
    .iter()
    .map(|s| assemble_text(s).expect("raw CLVM assembles"))
    .collect();
    let nil = SExp::Atom(AtomBuf::new(vec![]));

    let mut runtime = ClvmRuntime::new(1_000_000_000, 0);
    for p in &programs {
        let _ = runtime.run(p.sexp(), &nil);
    }
    let base = LIVE.load(Ordering::Relaxed);
    const CYCLES: usize = 400;
    for _ in 0..CYCLES {
        for p in &programs {
            runtime.run(p.sexp(), &nil).expect("small program runs");
        }
    }
    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(base) / CYCLES;
    eprintln!("  runtime reuse: retained={retained} B/cycle over {CYCLES} cycles");
    assert!(
        retained <= 64,
        "a reused runtime retains {retained} B per program cycle — reset is not releasing"
    );
}
