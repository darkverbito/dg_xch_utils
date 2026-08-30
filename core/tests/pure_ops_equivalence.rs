// Does the non-allocating operator protocol produce identical results to the shipped one?
//
// `pure_ops` is a prototype of the signature DaOneLuna asked for: operators that take `&Arena`
// instead of `&mut Arena`, and so cannot allocate at all. The claim it has to survive is narrow and
// total — for every input, the pure operator and the shipped operator must agree on BOTH the result
// node's serialized bytes AND the exact cost. Cost is the half that would be easy to get subtly
// wrong, because the malloc surcharge depends on the encoded length of a result that does not exist
// yet when the pure operator returns.
//
// The four operators cover the shapes that stress the protocol differently:
//   - `f`      returns an existing node, so the protocol should cost literally nothing;
//   - `c`      builds structure from nodes it never inspects;
//   - `+`      computes a number whose price depends on its own encoded length;
//   - `concat` is the worst case — output size is unknown until every argument is walked, and the
//              result may be large, so a naive protocol would pay an extra full-size copy here.
//
// Inputs are generated from a seeded PRNG and deliberately include the degenerate cases: empty
// arguments, wrong arity, non-atoms where atoms are required, and concatenations large enough that
// an extra copy would show up in the timing comparison rather than hiding in noise.

use dg_xch_core::clvm::arena::{Arena, NodePtr};
use dg_xch_core::clvm::dialect::ChiaDialect;
use dg_xch_core::clvm::pure_ops;
use dg_xch_core::clvm::{core_ops, more_ops};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Atom byte strings biased toward the encodings that have historically broken things.
fn gen_atom(rng: &mut Rng) -> Vec<u8> {
    match rng.below(10) {
        0 => vec![],
        1 => vec![0],
        2 => vec![0x80],
        3 => vec![0xff],
        4 => vec![0, 0x80],
        5 => vec![0x03, 0xff, 0xff, 0xff],
        6 => (0..rng.below(4) + 1).map(|_| rng.next() as u8).collect(),
        // Large atoms: where an extra copy in the protocol would actually cost something.
        _ => (0..rng.below(600) + 1).map(|_| rng.next() as u8).collect(),
    }
}

/// Build an argument list, occasionally malformed so the error paths are compared too.
fn gen_args(arena: &mut Arena, rng: &mut Rng, want_pairs: bool) -> NodePtr {
    let n = rng.below(4);
    let mut items = Vec::new();
    for _ in 0..n {
        let node = if want_pairs && rng.below(3) == 0 {
            // A pair where an atom is expected — drives the error path.
            let a = arena.new_atom(&gen_atom(rng)).expect("atom");
            let b = arena.new_atom(&gen_atom(rng)).expect("atom");
            arena.new_pair(a, b).expect("pair")
        } else {
            arena.new_atom(&gen_atom(rng)).expect("atom")
        };
        items.push(node);
    }
    let mut list = NodePtr::NIL;
    for node in items.into_iter().rev() {
        list = arena.new_pair(node, list).expect("pair");
    }
    list
}

/// Everything observable about an operator's outcome: the exact cost, and the result rendered so
/// two different arenas can be compared without sharing handles.
fn describe(arena: &Arena, r: &Result<(u64, NodePtr), dg_xch_core::errors::ClvmError>) -> String {
    match r {
        Ok((cost, node)) => format!("ok {cost} {}", arena.display(*node)),
        Err(e) => {
            let mut s = format!("err {e:?}");
            s.truncate(200);
            s
        }
    }
}

#[test]
fn pure_operators_match_the_shipped_operators_exactly() {
    const CASES: u64 = 3000;
    let dialect = ChiaDialect::new(0);
    let mut agreed = 0usize;
    let mut errors = 0usize;

    for seed in 0..CASES {
        // Each operator gets its own arena pair so neither run can observe the other's allocations.
        for which in 0..4 {
            let mut rng_a = Rng::new(seed ^ (which as u64) << 32);
            let mut rng_b = Rng::new(seed ^ (which as u64) << 32);
            let mut arena_shipped = Arena::new();
            let mut arena_pure = Arena::new();
            let args_shipped = gen_args(&mut arena_shipped, &mut rng_a, true);
            let args_pure = gen_args(&mut arena_pure, &mut rng_b, true);

            let (shipped, pure) = match which {
                0 => (
                    core_ops::op_first(&mut arena_shipped, args_shipped, u64::MAX, &dialect),
                    pure_ops::apply_pure(
                        pure_ops::op_first,
                        &mut arena_pure,
                        args_pure,
                        u64::MAX,
                        &dialect,
                    ),
                ),
                1 => (
                    core_ops::op_cons(&mut arena_shipped, args_shipped, u64::MAX, &dialect),
                    pure_ops::apply_pure(
                        pure_ops::op_cons,
                        &mut arena_pure,
                        args_pure,
                        u64::MAX,
                        &dialect,
                    ),
                ),
                2 => (
                    more_ops::op_add(&mut arena_shipped, args_shipped, u64::MAX, &dialect),
                    pure_ops::apply_pure(
                        pure_ops::op_add,
                        &mut arena_pure,
                        args_pure,
                        u64::MAX,
                        &dialect,
                    ),
                ),
                _ => (
                    more_ops::op_concat(&mut arena_shipped, args_shipped, u64::MAX, &dialect),
                    pure_ops::apply_pure(
                        pure_ops::op_concat,
                        &mut arena_pure,
                        args_pure,
                        u64::MAX,
                        &dialect,
                    ),
                ),
            };

            let a = describe(&arena_shipped, &shipped);
            let b = describe(&arena_pure, &pure);
            let name = ["first", "cons", "add", "concat"][which];
            assert_eq!(
                a, b,
                "seed {seed} op {name}: the pure operator disagrees with the shipped one.\n\
                 shipped: {a}\n  pure: {b}"
            );
            if shipped.is_err() {
                errors += 1;
            }
            agreed += 1;
        }
    }

    eprintln!(
        "  {agreed} operator invocations agreed exactly ({errors} of them on the error path)"
    );
    assert!(
        errors > 0,
        "no error-path cases were generated; the comparison only covers success"
    );
}

#[test]
fn the_worst_case_operator_pays_no_extra_copy() {
    // `concat` is where a naive protocol would lose: returning `Vec<u8>` would copy the whole
    // result once more than the shipped operator does. Describing the work instead (source nodes +
    // total length) lets the caller write straight into the arena heap. Compare the two on large
    // concatenations; the pure path must not be materially slower.
    use std::time::Instant;

    let dialect = ChiaDialect::new(0);
    const REPS: usize = 4000;

    let build = |arena: &mut Arena, rng: &mut Rng| {
        let mut list = NodePtr::NIL;
        let mut nodes = Vec::new();
        for _ in 0..8 {
            let blob: Vec<u8> = (0..2048).map(|_| rng.next() as u8).collect();
            nodes.push(arena.new_atom(&blob).expect("atom"));
        }
        for node in nodes.into_iter().rev() {
            list = arena.new_pair(node, list).expect("pair");
        }
        list
    };

    let mut arena = Arena::new();
    let mut rng = Rng::new(0xC0AC);
    let args = build(&mut arena, &mut rng);

    // Warm both paths so allocation growth is not charged to whichever runs first.
    for _ in 0..64 {
        let _ = more_ops::op_concat(&mut arena, args, u64::MAX, &dialect);
        let _ = pure_ops::apply_pure(pure_ops::op_concat, &mut arena, args, u64::MAX, &dialect);
    }

    let t0 = Instant::now();
    for _ in 0..REPS {
        let r = more_ops::op_concat(&mut arena, args, u64::MAX, &dialect).expect("concat");
        std::hint::black_box(r.0);
    }
    let shipped = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    for _ in 0..REPS {
        let r = pure_ops::apply_pure(pure_ops::op_concat, &mut arena, args, u64::MAX, &dialect)
            .expect("concat");
        std::hint::black_box(r.0);
    }
    let pure = t1.elapsed().as_secs_f64();

    let ratio = pure / shipped;
    eprintln!(
        "  concat 8x2048 B: shipped {shipped:.4}s, pure {pure:.4}s, ratio {ratio:.2}x over {REPS} reps"
    );
    assert!(
        ratio < 1.5,
        "the pure protocol is {ratio:.2}x slower on the worst-case operator, which means it is \
         paying a copy the shipped operator avoids — the description-based design has failed its \
         central claim"
    );
}

#[test]
fn the_protocol_overhead_is_small_on_a_trivial_operator() {
    // `concat` mixes two costs: the protocol itself, and moving a `Vec` of source nodes through
    // the returned description. `f` isolates the first — it reads two pairs and returns an
    // existing node, so essentially all the time is dispatch. If the protocol had meaningful
    // fixed overhead it would show here at its worst, since there is no real work to amortize it
    // against; in a real run the eval loop dwarfs this.
    use std::time::Instant;

    let dialect = ChiaDialect::new(0);
    const REPS: usize = 200_000;

    let mut arena = Arena::new();
    let inner_a = arena.new_atom(&[1, 2, 3]).expect("atom");
    let inner_b = arena.new_atom(&[4, 5, 6]).expect("atom");
    let pair = arena.new_pair(inner_a, inner_b).expect("pair");
    let args = arena.new_pair(pair, NodePtr::NIL).expect("pair");

    for _ in 0..1000 {
        let _ = core_ops::op_first(&mut arena, args, u64::MAX, &dialect);
        let _ = pure_ops::apply_pure(pure_ops::op_first, &mut arena, args, u64::MAX, &dialect);
    }

    let t0 = Instant::now();
    for _ in 0..REPS {
        let r = core_ops::op_first(&mut arena, args, u64::MAX, &dialect).expect("first");
        std::hint::black_box(r.0);
    }
    let shipped = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    for _ in 0..REPS {
        let r = pure_ops::apply_pure(pure_ops::op_first, &mut arena, args, u64::MAX, &dialect)
            .expect("first");
        std::hint::black_box(r.0);
    }
    let pure = t1.elapsed().as_secs_f64();

    let ratio = pure / shipped;
    eprintln!(
        "  first (pure dispatch): shipped {shipped:.4}s, pure {pure:.4}s, ratio {ratio:.2}x over {REPS} reps"
    );
    assert!(
        ratio < 1.5,
        "the protocol costs {ratio:.2}x on an operator that does no work, so the overhead is in          the dispatch itself and every operator would pay it"
    );
}
