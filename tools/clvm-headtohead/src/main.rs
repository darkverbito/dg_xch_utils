//! Head-to-head peak memory: clvm_rs against dg_xch's CLVM VM, on the same real mainnet work.
//!
//! WHAT MAKES THIS COMPARABLE
//!
//! Both VMs run under one counting global allocator, so peak means the same thing on both sides.
//! Each VM's own counters describe its internal units (spans, pools, bump chunks) and cannot be
//! compared across implementations; total live bytes can.
//!
//! WHAT MAKES IT FAITHFUL
//!
//! A block generator is not simply "run the serialized program". Reproducing what a validating
//! node actually does means matching four things, and getting any of them wrong measures nothing:
//!
//!   1. Back-reference decoding. Post-hardfork mainnet generators are serialized with 0xfe
//!      back-references; the plain decoder rejects them outright.
//!   2. The ROM bootstrap. Before the hard fork a generator is not executed directly — it is
//!      passed as an ARGUMENT to the ROM bootstrap program, which drives it.
//!   3. The generator argument list: `(generator (ref_block_bytes ...))`, where each referenced
//!      block's serialized generator is supplied as a single atom.
//!   4. The block's cost ceiling: the CLVM budget is the block limit minus the generator's own
//!      byte cost, so both sides get the same budget and stop at the same place.
//!
//! An earlier version of this harness ran the raw generator against a nil environment and
//! reported ~1-2 MiB for a block whose real peak is ~143 MiB. It was measuring startup, not
//! execution. The gap between those numbers is the reason each of the four points above is spelled
//! out rather than assumed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Tracking;
impl Tracking {
    fn bump(d: usize) {
        let live = LIVE.fetch_add(d, Ordering::Relaxed) + d;
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
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n > l.size() {
            Self::bump(n - l.size());
        } else {
            LIVE.fetch_sub(l.size() - n, Ordering::Relaxed);
        }
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static ALLOC: Tracking = Tracking;

/// Start a measurement window at the current live level.
fn open() -> usize {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    base
}
/// High-water above the window's starting level, in MiB.
fn close(base: usize) -> f64 {
    PEAK.load(Ordering::Relaxed).saturating_sub(base) as f64 / (1024.0 * 1024.0)
}

const MAX_BLOCK_COST: u64 = 11_000_000_000;
const COST_PER_BYTE: u64 = 12_000;

const ROM_BOOTSTRAP: &str = include_str!("../rom_bootstrap.hex");
const GEN_4671894: &str =
    include_str!("../../clvmtest/core/tests/fixtures/chia_generator_tests/block-4671894.txt");
const REF_4671893: &str =
    include_str!("../../clvmtest/core/tests/fixtures/chia_generator_tests/block-4671894.env");
const GEN_9189472: &str =
    include_str!("../../clvmtest/core/tests/fixtures/heavy_generators/block-9189472.txt");

fn unhex(s: &str) -> Vec<u8> {
    let s = s.lines().next().expect("fixture has content").trim();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// The CLVM budget a validating node gives the generator: the block ceiling less the generator's
/// own serialized byte cost. Both VMs get the identical figure.
fn cost_budget(generator_bytes: &[u8]) -> u64 {
    MAX_BLOCK_COST - (generator_bytes.len() as u64) * COST_PER_BYTE
}

// ---------------------------------------------------------------- clvm_rs

fn clvm_rs_rom(gen_bytes: &[u8], ref_bytes: Option<&[u8]>) -> (f64, String) {
    use clvmr::allocator::Allocator;
    use clvmr::chia_dialect::{ChiaDialect, ClvmFlags};
    use clvmr::run_program::run_program;
    use clvmr::serde::{node_from_bytes, node_from_bytes_backrefs};

    let rom_bytes = unhex(ROM_BOOTSTRAP);
    let budget = cost_budget(gen_bytes);

    let run = |a: &mut Allocator| -> Result<u64, String> {
        let rom = node_from_bytes(a, &rom_bytes).map_err(|e| format!("rom decode: {e:?}"))?;
        // Generators arrive back-reference serialized; the plain decoder cannot read them.
        let generator =
            node_from_bytes_backrefs(a, gen_bytes).map_err(|e| format!("generator decode: {e:?}"))?;

        // args = (generator ((ref ...)))
        //
        // The ref list is wrapped in a further list, which is one level deeper than the obvious
        // reading of "generator plus its references". Getting it wrong makes the ROM traverse
        // into an atom (PathIntoAtom) rather than doing any work.
        let mut refs = a.nil();
        if let Some(rb) = ref_bytes {
            let atom = a.new_atom(rb).map_err(|e| format!("ref atom: {e:?}"))?;
            refs = a.new_pair(atom, a.nil()).map_err(|e| format!("ref list: {e:?}"))?;
        }
        let refs_wrapped = a.new_pair(refs, a.nil()).map_err(|e| format!("ref wrap: {e:?}"))?;
        let args_tail = a
            .new_pair(refs_wrapped, a.nil())
            .map_err(|e| format!("args tail: {e:?}"))?;
        let args = a
            .new_pair(generator, args_tail)
            .map_err(|e| format!("args: {e:?}"))?;

        let dialect = ChiaDialect::new(ClvmFlags::empty());
        match run_program(a, &dialect, rom, args, budget) {
            Ok(r) => Ok(r.0),
            Err(e) => Err(format!("run: {e:?}")),
        }
    };

    // Warm once so first-touch growth is charged to neither side.
    {
        let mut a = Allocator::new();
        let _ = run(&mut a);
    }
    let mut a = Allocator::new();
    let base = open();
    let outcome = run(&mut a);
    let peak = close(base);
    drop(a);
    match outcome {
        Ok(cost) => (peak, format!("cost={cost}")),
        Err(e) => (peak, e),
    }
}

fn clvm_rs_simple(gen_bytes: &[u8]) -> (f64, String) {
    use clvmr::allocator::Allocator;
    use clvmr::chia_dialect::{ChiaDialect, ClvmFlags};
    use clvmr::run_program::run_program;
    use clvmr::serde::node_from_bytes_backrefs;

    let budget = cost_budget(gen_bytes);
    // Post-hard-fork regime, mapped onto their flag names.
    let flags = ClvmFlags::ENABLE_KECCAK_OPS_OUTSIDE_GUARD
        | ClvmFlags::DISABLE_OP
        | ClvmFlags::LIMITS
        | ClvmFlags::CANONICAL_INTS
        | ClvmFlags::NEW_COST_MODEL
        | ClvmFlags::RELAXED_BLS;

    let run = |a: &mut Allocator| -> Result<u64, String> {
        let program =
            node_from_bytes_backrefs(a, gen_bytes).map_err(|e| format!("decode: {e:?}"))?;
        let dialect = ChiaDialect::new(flags);
        let nil = a.nil();
        run_program(a, &dialect, program, nil, budget)
            .map(|r| r.0)
            .map_err(|e| format!("run: {e:?}"))
    };

    {
        let mut a = Allocator::new();
        let _ = run(&mut a);
    }
    let mut a = Allocator::new();
    let base = open();
    let outcome = run(&mut a);
    let peak = close(base);
    drop(a);
    match outcome {
        Ok(cost) => (peak, format!("cost={cost}")),
        Err(e) => (peak, e),
    }
}

// ---------------------------------------------------------------- dg_xch

/// Run the ROM bootstrap with exactly the arguments the clvm_rs side is given, so both measure
/// the same work.
///
/// The validator entry point (`execute_block_generator_result`) is NOT used here: it also charges
/// the generator's serialized byte cost and then extracts conditions from the output, so its cost
/// figure is for the whole block. Comparing that against a bare ROM run put 4.25B against 523M —
/// two different workloads wearing the same label. Matching costs is the check that the comparison
/// is real; matching peaks is the result.
fn dg_xch_rom(gen_hex: &str, ref_hex: Option<&str>) -> (f64, String) {
    use dg_xch_core::clvm::program::SerializedProgram;
    use dg_xch_core::clvm::runtime::ClvmRuntime;
    use dg_xch_core::clvm::sexp::{AtomBuf, SExp};

    let gen_bytes = unhex(gen_hex);
    let budget = cost_budget(&gen_bytes);

    let build = || -> Result<(SExp<'static>, SExp<'static>), String> {
        let rom = SerializedProgram::from(unhex(ROM_BOOTSTRAP))
            .to_program()
            .map_err(|e| format!("rom decode: {e:?}"))?
            .sexp()
            .to_owned();
        let generator = SerializedProgram::from(gen_bytes.clone())
            .to_program_backrefs()
            .map_err(|e| format!("generator decode: {e:?}"))?
            .sexp()
            .to_owned();
        // (generator ((ref ...))) — the same shape the validator builds.
        let refs = match ref_hex {
            Some(r) => SExp::from(vec![SExp::Atom(AtomBuf::new(unhex(r)))]),
            None => SExp::from(Vec::<SExp>::new()),
        };
        let args = SExp::from(vec![generator, SExp::from(vec![refs])]);
        Ok((rom, args))
    };

    let (rom, args) = match build() {
        Ok(v) => v,
        Err(e) => return (0.0, e),
    };

    {
        let mut rt = ClvmRuntime::new(budget, 0);
        let _ = rt.run(&rom, &args);
    }
    let base = open();
    let mut rt = ClvmRuntime::new(budget, 0);
    let outcome = rt.run(&rom, &args);
    let peak = close(base);
    match outcome {
        Ok((cost, _)) => (peak, format!("cost={cost}")),
        Err(e) => (peak, format!("{e:?}")),
    }
}

#[allow(dead_code)]
fn dg_xch_block(gen_hex: &str, ref_hex: Option<&str>, height: u32) -> (f64, String) {
    use dg_xch_core::clvm::program::SerializedProgram;
    use dg_xch_core::consensus::block_generator::{
        BlockGeneratorFlags, BlockGeneratorInput, GeneratorReference,
        execute_block_generator_result,
    };
    use dg_xch_core::consensus::constants::MAINNET;

    let build = || BlockGeneratorInput {
        transactions_generator: SerializedProgram::from(unhex(gen_hex)),
        generator_refs: ref_hex
            .map(|r| {
                vec![GeneratorReference {
                    height: height - 1,
                    index: 0,
                    generator: SerializedProgram::from(unhex(r)),
                }]
            })
            .unwrap_or_default(),
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::for_height(&MAINNET, height),
    };

    let input = build();
    let _ = execute_block_generator_result(&input); // warm

    let base = open();
    let outcome = execute_block_generator_result(&input);
    let peak = close(base);
    match outcome {
        Ok(c) => (peak, format!("cost={} spends={}", c.cost, c.spends.len())),
        Err(e) => (peak, format!("{e:?}")),
    }
}

fn main() {
    println!("Peak live bytes during one generator run, both VMs under the same allocator.\n");
    println!(
        "{:<30} {:>12} {:>12}   {}",
        "block", "clvm_rs", "dg_xch", "outcome"
    );
    println!("{}", "-".repeat(86));

    // Pre-hard-fork: the ROM bootstrap path, with a referenced block. This is the shape whose
    // peak drove the whole investigation — 532 spends.
    let gen = unhex(GEN_4671894);
    let rf = unhex(REF_4671893);
    let (rs, rs_note) = clvm_rs_rom(&gen, Some(&rf));
    let (dg, dg_note) = dg_xch_rom(GEN_4671894, Some(REF_4671893));
    println!(
        "{:<30} {rs:>10.2} MiB {dg:>8.2} MiB   rs[{rs_note}] dg[{dg_note}]",
        "4671894 (532 spends, ROM)"
    );
    println!(
        "{:<30} {:>10} {:>12}   costs must MATCH for the peaks to mean anything",
        "", "", ""
    );

    // The post-hard-fork path is deliberately NOT compared here. A simple generator is a quoted
    // `(q . REST)`, so running it costs ~20 and returns the spend list; the block's real ~10.9B
    // cost comes from executing each spend's puzzle afterwards. Comparing "run the generator"
    // across the two would put a whole-block figure against a near-no-op — which is exactly the
    // mistake the first version of this harness made. A fair comparison there needs per-spend
    // execution replicated on the clvm_rs side, which is a larger piece of work.
    let gen2 = unhex(GEN_9189472);
    let (rs2, rs2_note) = clvm_rs_simple(&gen2);
    println!(
        "{:<30} {rs2:>10.2} MiB {:>12}   rs[{rs2_note}] (generator only — not comparable)",
        "9189472 (cost-maxed)", "n/a"
    );

    println!(
        "\nA row reporting an error, or a cost far below the block's real cost, measured only the\n\
         work done before it stopped. Those are not results."
    );
}
