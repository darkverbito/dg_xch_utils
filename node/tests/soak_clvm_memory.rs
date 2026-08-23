// Steady-state memory soak — the regression gate for the CLVM/collection leak CLASS.
//
// Every memory leak this node has had was the same footgun: per-block work that stores data in a
// long-lived structure (a collection, or the CLVM bumpalo arena) whose release is missing,
// conditional, or — as one real bug was — only partial (owned atoms freed, owned pairs not). Rust's
// ownership does not catch it: the structure is legitimately owned, it just never shrinks. A unit
// test asserting a value is wrong can't see it; only a SOAK — run the hot path many times and prove
// live memory does not grow — can.
//
// This test wires a deterministic counting allocator (System-backed; unlike jemalloc's cached stats
// it has no arena/purge noise, so the flatness epsilon is tight) and replays a REAL tx-era block's
// body validation (CLVM generator parse + run — the exact path a prior bug leaked) many times, asserting
// live bytes do not grow per run. Before the fix this retained 156,320 B/run; after, ~0.
//
// It runs in the normal `cargo test -p dg_xch_node` gate — no DB, no corpus, no network.

mod common;

use dg_xch_core::consensus::block_generator::{
    execute_block_generator_result, BlockGeneratorFlags, BlockGeneratorInput,
};
use dg_xch_core::consensus::constants::MAINNET;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

// Live bytes = sum(alloc sizes) - sum(dealloc sizes). Exact and deterministic — a single
// retained allocation per run shows up, with none of jemalloc's fragmentation/holdback jitter.
struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, l, new_size) };
        if !p.is_null() {
            // net change in tracked live bytes for the resized allocation
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

// The CLVM body-validation path must retain no memory across runs. This is the exact path a prior bug
// leaked (the c/cons operator built the generator's output list from Owned pairs, which the bumpalo
// arena stored without ever dropping their Arcs). Replaying one real block's validation thousands of
// times must leave live memory flat.
#[test]
fn clvm_body_validation_is_steady_state() {
    let block = common::load_full_block(5_000_000);
    let generator = block
        .transactions_generator
        .clone()
        .expect("a tx block carries a generator");
    let height = block.height();
    let input = BlockGeneratorInput {
        transactions_generator: generator,
        // Ref-less: the CLVM operators (and thus the arena's Owned-pair allocations — the leak site)
        // execute regardless of whether refs resolve; the result value is irrelevant to retention.
        generator_refs: Vec::new(),
        constants: MAINNET,
        height,
        flags: BlockGeneratorFlags::for_height(&MAINNET, height),
    };

    // Warm-up settles one-time lazy allocations (dialect tables, thread-locals) before baselining.
    for _ in 0..50 {
        let _ = execute_block_generator_result(&input);
    }
    let base = LIVE.load(Ordering::Relaxed);

    // 200 is ample: the counting allocator is exact (no jitter), so the pre-fix 156,320 B/run bug
    // would show ~31 MB retained here while a correct arena shows ~0 — a landslide either way. Kept
    // low so the gate stays fast in `cargo test`.
    const ITERS: usize = 200;
    for _ in 0..ITERS {
        // Drop each result immediately; a correct arena releases everything on runtime drop.
        let _ = execute_block_generator_result(&input);
    }

    let retained = LIVE.load(Ordering::Relaxed).saturating_sub(base);
    let per_run = retained / ITERS;

    // The prior bug retained 156,320 B/run. The threshold sits ~150x below that and comfortably
    // above counter jitter, so the gate trips on a real per-run leak without being flaky.
    assert!(
        per_run < 1024,
        "CLVM body validation retained {per_run} B/run ({retained} B over {ITERS} runs) — a memory \
         leak. The arena-Owned-pair bug was 156,320 B/run; anything growing per run means a \
         collection or arena is keeping per-block data. Do NOT relax this threshold to make it pass."
    );
}
