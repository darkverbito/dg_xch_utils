// U1 RED TEST — the owned `SExp` type has a RECURSIVE `Drop`.
//
// `SExp::Pair(PairBuf::Owned((Arc<SExp>, Arc<SExp>)))` forms an owned tree; dropping a deeply-nested
// chain recurses one native stack frame per level, so an adversarial CLVM structure that decodes to
// a deep owned list overflows the stack WHEN DROPPED (SIGABRT). chia/clvm_rs are immune by design —
// they never materialize a deep owned tree (arena / `NodePtr` indices), so their teardown is a flat
// arena free. This is a DIVERGENCE from clvm_rs (see docs/security-audit-2026-08.md, finding U1).
//
// The test builds a right-nested owned SExp `(() () () … )` DEPTH deep (iteratively — building never
// recurses) and drops it on a bounded 1 MiB worker stack. On pr-52 HEAD the recursive `Drop`
// overflows and aborts the process; the iterative-`Drop` fix drops it in O(1) stack and this passes.
use dg_xch_core::clvm::sexp::{AtomBuf, PairBuf, SExp};
use std::sync::Arc;

fn build_deep(depth: usize) -> SExp<'static> {
    let mut node = SExp::Atom(AtomBuf::new(Vec::new())); // nil tail
    for _ in 0..depth {
        node = SExp::Pair(PairBuf::Owned((
            Arc::new(SExp::Atom(AtomBuf::new(Vec::new()))), // first = ()
            Arc::new(node),                                 // rest = the deepening chain
        )));
    }
    node
}

#[test]
fn deep_owned_sexp_drops_without_stack_overflow() {
    // 500k levels overflow any reasonable stack under a per-level recursive Drop; a bounded 1 MiB
    // worker stack makes the crash deterministic, while an iterative Drop passes comfortably.
    const DEPTH: usize = 500_000;
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let deep = build_deep(DEPTH);
            drop(deep); // recursive Drop overflows here on pr-52 HEAD (SIGABRT)
            DEPTH
        })
        .expect("spawn worker");
    let n = handle
        .join()
        .expect("the deep owned SExp must drop without overflowing the stack");
    assert_eq!(n, DEPTH);
}
