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

// FIXED (chino, CLVM-core): the teardown is now iterative. The naive `impl Drop for SExp` does not
// compile — `SExp` carries `const NULL_SEXP`/`const ONE_SEXP` used as `&'static`, and a type with an
// explicit `Drop` impl is not const-promotable, cascading into `from_bool`, `SExpIter`, and
// `Program::new_const`. Instead the `Drop` lives on `PairBuf` (the field type of `SExp::Pair`), which
// leaves `SExp` const-promotable, so none of that surface changes. The `Drop` unwinds the owned
// `Arc<SExp>` spine with an explicit heap worklist (no recursion) and only dismantles links it solely
// owns (`Arc::try_unwrap`), leaving shared subtrees intact. See `core/src/clvm/sexp.rs`.
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

// SHARED-OWNERSHIP CORRECTNESS: the iterative teardown must dismantle only the links it SOLELY owns
// and stop at any `Arc` that is shared with another holder, leaving that shared subtree wholly
// intact. This builds a deep sole-owned prefix whose tail is a small subtree held alive by an
// independent `Arc` clone, drops the prefix (which exercises the iterative unwind), and then proves
// the clone is still a fully-walkable, correct structure — i.e. `Arc::try_unwrap` correctly refused
// to tear the shared tail apart.
#[test]
fn shared_arc_tail_survives_iterative_drop_of_sole_owned_prefix() {
    // A small, distinctly-shaped shared tail: (1 2 3), i.e. three-deep right-nested owned pairs.
    fn small_list() -> SExp<'static> {
        let mut node = SExp::Atom(AtomBuf::new(Vec::new()));
        for v in [3u8, 2, 1] {
            node = SExp::Pair(PairBuf::Owned((
                Arc::new(SExp::Atom(AtomBuf::new(vec![v]))),
                Arc::new(node),
            )));
        }
        node
    }
    // Depth of the shared tail as an independent measurement, so the assertion below is self-checking.
    fn owned_pair_depth(mut node: &SExp<'static>) -> usize {
        let mut d = 0;
        while let SExp::Pair(PairBuf::Owned((_, rest))) = node {
            d += 1;
            node = rest.as_ref();
        }
        d
    }

    let shared_tail: Arc<SExp<'static>> = Arc::new(small_list());
    let keep = shared_tail.clone(); // second, independent owner of the tail
    assert_eq!(Arc::strong_count(&keep), 2);
    let expected_depth = owned_pair_depth(&keep);
    assert_eq!(expected_depth, 3);

    // Build a deep sole-owned prefix that ends in the shared tail (deep enough to demand the
    // iterative unwind, on a bounded worker stack so a recursive teardown would abort).
    const PREFIX: usize = 200_000;
    let handle = std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            let mut node: Arc<SExp<'static>> = shared_tail; // strong_count now 2 (chain + keep)
            for _ in 0..PREFIX {
                node = Arc::new(SExp::Pair(PairBuf::Owned((
                    Arc::new(SExp::Atom(AtomBuf::new(Vec::new()))),
                    node,
                ))));
            }
            // Move the prefix out of its Arc (sole owner) and drop it: the iterative teardown walks
            // down the prefix, reaches the shared tail (strong_count 2), and must NOT dismantle it.
            let prefix = Arc::try_unwrap(node).unwrap_or_else(|a| (*a).clone());
            drop(prefix);
        })
        .expect("spawn worker");
    handle
        .join()
        .expect("dropping the sole-owned prefix must not overflow the stack");

    // The shared tail is now solely owned by `keep` and must be byte-identical to a fresh build.
    assert_eq!(Arc::strong_count(&keep), 1);
    assert_eq!(owned_pair_depth(&keep), expected_depth);
    assert_eq!(*keep, small_list());
    assert_eq!(keep.tree_hash(), small_list().tree_hash());
}
