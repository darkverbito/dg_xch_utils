# The CLVM allocator investigation

Everything learned while answering DaOneLuna's objection to the CLVM runtime rewrite:
what was measured, what was wrong, what Chia Network already tried, and what is left.

Tags marking this state:

| tag | tree | branch | holds |
| --- | --- | --- | --- |
| `clvm-allocator-investigation` (`ddebcd4`) | `dgxch-pr-fold` | `pr49-review-fixes` | test suite, fixtures, fuzz targets, and the completed three-phase implementation (§8b) |
| `bumpalo-runtime-restored` (`3d463a0`) | `dgxch-bumpalo` | `bumpalo-runtime-restore-wt` | fully restored bumpalo runtime, allocation profiler |

---

## 1. The question

DaOneLuna's three directives, verbatim:

> Remove chia allocator. Restore bumpalo arena allocation
> Confirm memory issues are gone with bumpalo
> Further optimize sexp runtime with out the api requiring an arena to be passed around. The runtime should handle it

Behind them, his objection: the CLVM runtime he wrote was replaced with something he
considers a clone of Chia's code, in violation of a standing directive not to use CNI code
outside POS2. He wanted to know why an OOM required a rewrite, and he disliked losing an API
where operators did not have to thread an allocator around.

He was substantially right, and the rewrite was substantially unnecessary. The details matter,
because one part of his position does not survive measurement.

---

## 2. What was proven

All measurements are from the cluster builder (Xeon E5-2690 v2), release builds, using the
test suite described in §8. Every gate is corpus-free: no chain sync, no database, no network,
48 KB of committed fixtures.

### 2.1 His bumpalo runtime does not leak

Seven shapes, each soaked and measured with an exact counting global allocator:

| shape | retained per run |
| --- | --- |
| plain generator (834752) | 0 B |
| backref-compressed generator (ROM bootstrap path) | 0 B |
| generator resolving a reference block (4671894) | 0 B |
| cost-maxed, 49 spends (9189472) | 0 B |
| cost-maxed, 20 spends (9189481) | 0 B |
| mempool admission (`conditions_from_spend_bundle`) | 0 B |
| malicious generator, cost-roof rejection | 0 B |
| malicious generator, pair-pool-limit rejection | 0 B |
| concurrent validation, 3 shapes | 0 B |

Zero, not "small". **His two 2026-08 leak fixes genuinely closed the OOM.**

- `edeebf2` — owned atoms: bumpalo runs no destructors, so an `AtomBuf::Owned(Arc<Vec<u8>>)`
  stored in the arena never decremented its Arc. Copying the bytes into the arena and letting
  the Arc drop at allocation time took a 200-run soak from 0.7 → 18.8 GB to a flat 0.75 GB,
  and made it 22% faster.
- `c7b9008` — owned pairs: the same bug one level up. `c` (cons) builds the generator's output
  condition list out of `PairBuf::Owned((Arc<SExp>, Arc<SExp>))`, so the atoms-only fix left
  156,320 B/run leaking. Deep-converting owned pairs recursively took it to −6 B/run.

The compact-arena rewrite (`af8854d`) landed **after** both. It was never what fixed the OOM.

### 2.2 His runtime is consensus-correct

| gate | oracle | result |
| --- | --- | --- |
| cost wall, 46 contiguous mainnet blocks (9,179,155–9,179,200) | on-chain declared `transactions_info.cost` | pass |
| conditions digest, 16 blocks | full `SpendBundleConditions` sha256, harvested from the compact arena | pass |
| operator vectors, base + mempool modes | hand-computed CLVM semantics + pinned golden | 132/132 |

The cost wall is the strong one — it compares against what the chain itself declared, with no
dependence on the other runtime.

### 2.3 Operators can be arena-free at zero cost

See §7. Byte-identical results and costs across 12,000 invocations; parity performance.

---

## 3. The one real problem: intra-run peak

Block **4,671,894** — a real mainnet block that resolves a generator reference and fans out to
532 spends:

| runtime | peak |
| --- | --- |
| bumpalo | **1026 MiB** |
| compact arena | **143 MiB** |

Both are leak-free across runs. The entire difference is what is held **during** a single run.

Other shapes for context:

| workload | arena | bumpalo |
| --- | --- | --- |
| cost-maxed block (10.9B cost) | 11.25 MiB | 12.96 MiB |
| three concurrent cost-maxed | 31.42 MiB | 38.50 MiB |
| backref-compressed | 3.29 MiB | 16.16 MiB |
| **ref-resolving, 532 spends** | **142.94 MiB** | **1026.00 MiB** |

### 3.1 This is structural to bump allocation, not a bug and not configurable

bumpalo's own `Allocator::dealloc`:

```rust
// If the pointer is the last allocation we made, we can reuse the bytes,
// otherwise they are simply leaked -- at least until somebody calls reset().
```

The crate itself uses the word *leaked*. Its complete public API — checked against the source
at 3.19.0 — offers `alloc*`, `reset`, `allocated_bytes`, `set_allocation_limit`, and nothing
else. **There is no checkpoint, no rewind, no scoped free.** `set_allocation_limit` converts
unbounded growth into a graceful error, which would mean *rejecting a legitimate block* — a
consensus failure, not a fix.

We did not find a bumpalo defect. A bump allocator is the wrong allocator for a workload that
produces large amounts of dead intermediates inside one run.

### 3.2 A cap does not solve it

`MAX_NUM_ATOMS` / `MAX_NUM_PAIRS` (62,500,000 each) lived only in `arena.rs` and vanished with
it, so the restored bumpalo runtime has no allocation cap at all. Restoring the cap is
necessary but insufficient: block 4,671,894 is *valid*, and capping it means refusing to
validate a block the network accepted.

---

## 4. Where the gigabyte actually goes

From `node/tests/bump_alloc_profile.rs` (in the `dgxch-bumpalo` tree), instrumenting the bump
arena during the 4,671,894 run:

```
arena high-water        1024.00 MiB
owned-ATOM conversions  5,679,352   copying 9.83 MiB of atom bytes
owned-PAIR conversions  8,301,860
pass-through nodes      5,266,236
                        ─────────
total nodes            19,247,448   → 55.8 B/node
```

**Only 0.96% of the gigabyte is actual atom data.** The 5.68M atoms average **1.8 bytes each**.
Everything else is per-node overhead: `size_of::<SExp>()` is 24 bytes, individually
bump-allocated with alignment padding, plus a separate `alloc_slice_copy` for each atom's bytes.

The compact arena stores the same information as:

- `IntPair { first: NodePtr, rest: NodePtr }` — **8 bytes**
- `AtomSpan { start: u32, end: u32 }` — **8 bytes**, indexing one contiguous `Vec<u8>` blob
- small unsigned integers encoded **inline in the handle**, costing no storage at all

19,247,448 × 8 B = **147 MiB**, against the 143 MiB measured. The arithmetic closes.

### 4.1 The decisive corollary: reclamation is not the lever

The compact arena has **no free list and never pops** — 5 pushes, 0 pops or truncates during a
run. `reset` truncates the pools between runs, not within one. It is monotonic, exactly like a
bump allocator.

Both designs allocate ~19.25M nodes for that block, and at the time of measurement neither
reclaimed. **The entire 7.2× difference is node size.** This is why `bump-scope` (the one bump
allocator with scoped rewind) would not have closed it: node size, not garbage, was the gap.

**Corrected by §8b.** "Neither reclaims" described the code as it then stood, not a limit of the
design. Reclamation was subsequently added to the compact arena and is worth 13.65 MiB on this
block (136.68 → 123.03) — real, and an order of magnitude smaller than the 883 MiB that
separated the two representations. The ranking holds: representation first, reclamation second.

---

## 5. What Chia Network already tried

Full clone of `Chia-Network/clvm_rs`, 1307 commits, 2020-11 to 2026-08. Available at
`/Users/grantcermak/Development/irulast/clvm_rs-history`.

### 5.1 They started with DaOneLuna's exact design and abandoned it

The allocator they deleted in June 2021 (`78e5475`, "Drop ArcAllocator and simplify templates"):

```rust
pub enum ArcSExp {
    Atom(ArcAtomBuf),                    // Arc<Vec<u8>> + start/end
    Pair(Arc<ArcSExp>, Arc<ArcSExp>),
}
```

That is structurally identical to `SExp::Atom(AtomBuf)` / `SExp::Pair(PairBuf)` with `Arc`
children. **This is convergent evolution, not copying.** DaOneLuna independently arrived at the
design CNI shipped first and then removed. That framing matters: the objection "you cloned
Chia" and the reality "two people hit the same constraints and reached the same answer" are
very different conversations.

### 5.2 Timeline

| date | commit | what |
| --- | --- | --- |
| 2020-11 | first check-in | Arc-based tree |
| 2021-02 | `5223e74` | first `IntAllocator` — integer indices |
| 2021-02-12 | `f951d3c` | ops take `(allocator, Ptr)` instead of a bundled `Node` |
| 2021-02-13 | `67f3543` | **`Vec<Vec<u8>>` → one `Vec<u8>` + `(start,end)` spans** |
| 2021-02-19 | `b4747d2` | fix parser attempting a 17 GiB allocation |
| 2021-06 | `78e5475` | **drop `ArcAllocator` entirely** |
| 2021 | `14e77fd` | replace Aovec with `Vec` — no arbitrary atom-count ceiling |
| 2022-12 | `30b13fc` | checkpoint / restore |
| 2024-01 | `8bab13d` | small integers inline in the handle, never heap-allocated |
| — | `3e2ae49` | **real garbage collection** |
| — | `f80ab73`, `b9bc774` | interning API (`intern_tree`) |

### 5.3 Why they thread the allocator through every operator

Arvid Norberg's commit message on `f951d3c`, which directly answers directive 3:

> *Node cannot contain a mutable reference and a mutable reference cannot coexist with an
> immutable reference inside the Node object because of borrow-rules. So, passing them
> independently is the only way to pass in a mutable allocator.*

They **built** the ergonomic `Node { allocator, ptr }` bundle DaOneLuna wants, and Rust's borrow
checker forced them off it. The signature he dislikes is not a style choice.

Related commits show the same pressure: `7eeef46` ("A slice will necessarily hold a lifetime
relationship to the allocator, which prevents also passing in a mutable allocator reference"),
`a31f193` ("avoids overlapping references to the allocator").

**The constraint only binds if operators allocate.** See §7.

### 5.4 Why their memory works

`67f3543` is the whole answer, and it is exactly what our profile measured: individual
`Vec<u8>` per atom became one contiguous blob addressed by 8-byte spans. Against atoms averaging
1.8 bytes, the old scheme paid ~50 bytes of `Vec` header and allocator overhead per 2 bytes of
payload. The 2024 small-int inlining then removed storage entirely for values under 26 bits.

### 5.5 Checkpoints, and their hazard

Because nodes are indices into `Vec`s, a checkpoint is three lengths and restore is three
`truncate()` calls — O(1), no copying, no pointer fixups:

```rust
pub struct Checkpoint { u8s: usize, pairs: usize, atoms: usize }
```

But it creates dangling indices. A `NodePtr` captured before a restore silently refers to
something else afterward. They added a debug-only fingerprint/version scheme to catch it, and
had to re-key their G1/G2 validation cache by raw bytes because *"NodePtrs can be invalidated by
restore_checkpoint() inside softfork guards."*

Worth noting for any future use of `bump-scope`: its compile-time lifetimes would be **safer**
than clvm_rs's runtime assertions for this feature.

### 5.6 Bugs they hit that we now test for

`b4747d2` — "fix parser to not attempt allocating 17 GiB of memory". A serialized atom declaring
an enormous length. Our `clvm_adversarial_limits.rs` covers exactly this and passes at 0.000 MiB
peak while refusing 4 GiB and 8 GiB claims.

### 5.7 Their testing, which is where they are ahead of us

**24 fuzz targets**, including `allocator`, `garbage_collection`, `intern`, `operators`,
`run_program`, `tree_hash`, and several serde-format ones.

The pattern worth stealing is **metamorphic differential testing**:

- `garbage_collection.rs` runs the same generated program with GC on and off, asserting
  identical results **and identical allocator counts**.
- `run_program.rs` sweeps **six** flag combinations per program using checkpoint/restore.
- `allocator.rs` asserts an inlined small atom is indistinguishable from a heap one across
  bytes, length, number, equality, and canonicity.
- `intern.rs` asserts the tree hash is unchanged after interning while node counts drop.

Their program generator (`clvm-fuzzing/src/make_tree.rs`) is **type-directed**: each operator
declares an arity and the *kind* of value each argument wants (`SmallInt`, `LargeBytes`,
`Bytes32`, `G1Point`, `Bool`, `Tree`, `List`), so generated programs run deep instead of dying
on their first argument check.

---

## 6. Wrong turns

Recorded because the errors were expensive and several were the same mistake repeated.

### 6.1 Amending after pushing, four times

Fixed a clippy failure locally, amended it into the already-pushed commit, and pushed again.
Amending rewrites history, so every push was rejected as a non-fast-forward — and the rejection
went unread. CI kept re-testing stale commits while four local worktrees held the fix. Cost:
hours, across PRs 49–52.

**Lesson:** read push output; verify by reading file content off the remote, not by trusting the
push.

### 6.2 Sweeping one tree and assuming four

Each PR is a different tree. A lint sweep on the `#49` tree said nothing about pos2 code that
only exists on `#50`/`#51`. `proof_of_space/src/pos2/blake_hash.rs` failed on branches I had
declared clean.

### 6.3 Over-checking with `--all-targets`

Ran `cargo clippy --all-targets`; CI runs plain `cargo clippy -- -Dwarnings`, which lints lib and
bins only. Chased unused imports in test files that CI never checks, and "fixed" a file that was
never broken.

**Lesson:** read the workflow file before assuming the command.

### 6.4 Blaming a slow test for a builder I killed myself

A long run appeared to hang. It had not hung: the `builder-scale-to-zero` CronJob scaled the
StatefulSet to zero mid-test, because its idle signal is the mtime of `/workspace/.last-build`
and every job I ran wrote to `/scratch` instead. **My own configuration, and I knew it existed.**
Fixed durably with `scripts/cluster-run.sh`, which holds a heartbeat for the life of a job and
stops it on exit via `trap`.

### 6.5 Reporting "leak-free" about a runtime that would OOM the node

I split "retention across runs" (0 B) from "peak within a run" (1026 MiB) and led with the good
number. The distinction is real but the emphasis was wrong: memory that grows to a gigabyte on a
legitimate block and multiplies by worker count until the node dies is a leak by the only
definition that matters operationally.

### 6.6 Blaming the design for my own porting error

The restored bumpalo runtime undercharged block 9,179,161 by exactly **−5,597,941** cost. I was
prepared to treat it as a consensus flaw in his runtime. It was mine: I restored `dialect.rs`
from `c7b9008`, which predates the BLS dispatch fix, leaving opcodes 49–59 **commented out** so
they fell through to `op_unknown` for a token cost. Galactechs commit `748794c` documents that
exact delta from a live node wedge (frozen peak, 61-restart liveness loop). I had ported
`bls_ops.rs` but restored the dispatch table that never calls into it.

**Lesson:** when a restored-from-history component diverges, suspect the port before the design.

### 6.7 Guessing thresholds instead of measuring

Stated "measure first, then set thresholds from data" — then reused a 64 MiB peak ceiling for the
ref-resolving shape without measuring it. It legitimately peaks at 143 MiB. The gate failed on
correct behavior.

### 6.8 A CLVM encoding error in my own test

Built quotes as `(q x)` — a two-element list — instead of `(q . x)`, a dotted pair. Every
generated argument arrived wrapped in an extra list level and failed its type check. Only **19%**
of generated programs executed; the golden would have pinned "wrong argument type" messages
forever while reporting success.

Caught only because I had written an assertion requiring ≥50% of generated programs to execute.
After the fix: **81%**. That guard was worth more than the corpus.

### 6.9 Inferring call-vs-literal from shape

First attempt at the above fix was also wrong: I inferred "is this a nested call?" from whether
the value was a pair, so literal cons cells were evaluated as programs. Tracked explicitly after.

### 6.10 Tagging a commit that contained none of the work

Created `bumpalo-runtime-restored` while the entire restore — 12 files, `arena.rs` deleted, 956
lines removed — was still uncommitted. The tag pointed at a commit without any of it. Caught by
asking whether `arena.rs` still existed in the tagged tree; it did.

### 6.11 An overstated claim about reclamation

Said the compact arena "has no free list, it only pushes." True of our copy, and load-bearing for
§4.1 — but clvm_rs has since added real GC (`3e2ae49`), interning, and small-atom inlining. Our
arena is at roughly their 2021 state. **143 MiB is not the design's floor.**

---

## 7. The prototype: operators that cannot allocate

`core/src/clvm/pure_ops.rs`, with `core/tests/pure_ops_equivalence.rs`.

### 7.1 The idea

CNI is right that a `Node` bundling the allocator with a handle cannot exist (§5.3). But an
operator needs the arena mutably only to **write its result**, and every read helper in this
codebase already takes `&Arena`. Splitting the phases dissolves the conflict:

1. the operator reads its arguments through `&Arena` and returns an owned *description* of its
   result — it has no way to allocate, because it never holds a mutable borrow;
2. the caller, holding `&mut Arena`, materializes that description.

```rust
pub enum OpOut {
    Same(NodePtr),                 // f, r, i — zero allocation
    Pair(NodePtr, NodePtr),        // cons
    Number(Box<SExpNumber>),       // computed number; arena encodes it
    Concat(Vec<NodePtr>, usize),   // source nodes + total length
}
```

The `Concat` variant is the design's load-bearing part. A naive protocol returning `Vec<u8>`
would copy the whole result an extra time. Handing over the source nodes and total length lets
the caller write straight into the arena heap through the existing `new_concat` — no
intermediate buffer.

### 7.2 Results

Four operators spanning the shapes: `f` (returns an existing node), `c` (builds structure),
`+` (computes a number priced by its own encoded length), `concat` (worst case — output size
unknown until every argument is walked).

| property | result |
| --- | --- |
| semantics, 12,000 invocations (7,452 on error paths) | **byte-identical results and costs** |
| `concat`, 8×2048 B, 4000 reps | **0.95×** (pure is marginally faster) |
| `first`, pure dispatch overhead, 200,000 reps | **1.02×** |

An initial run showed 2.21× on `first`. `OpOut` was 48 bytes, so every return moved 48 bytes
across an extra call boundary. **Boxing the `Number` variant and inlining the phase split** let
the compiler fuse the two phases; the overhead went from ~13 ns/call to nothing measurable.

The threshold on the trivial-operator test is now **nanoseconds added per call**, not a ratio: a
ratio on a 10 ns operation is dominated by fixed cost and does not predict end-to-end impact.

### 7.3 What this means

Directive 3 is achievable at zero cost — but on top of the **compact arena**, not bumpalo. He
gets the API he asked for; the storage layer is the part that has to stay.

---

## 8. The test suite

Eleven gates, all corpus-free, from 48 KB of committed fixtures.

### Correctness

| file | what it pins |
| --- | --- |
| `core/tests/clvm_op_vectors.rs` | 132 operator vectors, base + mempool, values/costs/error text; hand-computed oracles where independently computable |
| `node/tests/clvm_conditions_digest.rs` | sha256 of the entire `SpendBundleConditions` for 16 blocks |
| `node/tests/cost_wall_9179161.rs` | 46 real blocks vs on-chain declared costs |
| `core/tests/clvm_random_differential.rs` | 2400 outcomes: 400 type-directed generated programs × 6 real fork configurations |
| `core/tests/clvm_representation_invariants.rs` | inline small atoms indistinguishable from heap; 20,000-value sweep; survival inside pairs |
| `core/tests/tree_hash_dedup.rs` | cached tree hash ≡ naive (pre-existing) |
| `core/tests/pure_ops_equivalence.rs` | pure vs shipped operators, results + costs + timing |

### Memory

| file | what it pins |
| --- | --- |
| `node/tests/clvm_leak_gate.rs` | ≤64 B/run retained across 7 shapes (was 1024 B — 1 KB/run is 5 GB over a mainnet sync) |
| `node/tests/clvm_peak_memory.rs` | per-shape high-water ceilings; concurrent peak scales with workers |
| `node/tests/clvm_adversarial_limits.rs` | clamped-cost rejection, oversized atom headers, deep nesting, runtime reuse |
| `node/tests/soak_clvm_memory.rs` | 200-iteration steady-state soak |

### Robustness and performance

| file | what it pins |
| --- | --- |
| `core/tests/clvm_parser_robustness.rs` | 4000 malformed inputs → 0 panics; 1500/1500 byte-identical round-trip |
| `node/tests/clvm_throughput.rs` | ≥0.30 Gcost/s floor (0.40 measured on the builder) |

### Fuzzing

`fuzz/` (workspace-excluded, needs nightly): `parse_program`, `roundtrip`, `run_program`.

```sh
cargo +nightly fuzz run run_program
```

### Design notes worth keeping

- Goldens are harvested with `UPDATE_GOLDEN=1` and committed. A failure names the seed.
- The randomized generator asserts **≥50% of programs execute**. Without it, a degraded
  generator produces a green suite that measures nothing (§6.8).
- Generation is type-directed, following CNI's approach (§5.7).
- The six flag configurations are the **real** fork ladder from `BlockGeneratorFlags::for_height`,
  not an invented matrix.

---

## 8b. The three-phase implementation (completed)

Executed in the order argued for in §9.1: convert operators first, then interning, then
reclamation. The ordering mattered — see the note on `gc_candidate` below.

### Results on block 4,671,894 (532 spends), the shape that drives peak

| stage | peak | change |
| --- | --- | --- |
| baseline | 142.94 MiB | — |
| Phase 1 — all 49 operators arena-free | 142.94 MiB | **0** |
| Phase 2 — large-atom interning | 136.68 MiB | −4.4% |
| Phase 3 — reclamation | **123.03 MiB** | **−13.9% cumulative** |

Correctness was bit-identical at every step: 132 operator vectors, 2400 randomized outcomes
across six dialects, the 46-block on-chain cost wall, 16 conditions digests. Leak gate 7/7 at
0 B/run throughout. Throughput 0.399–0.408 Gcost/s against a 0.408–0.413 baseline.

### Phase 1 — operators (commit `5edd5a3`)

All 49 operators take `&Arena` and cannot allocate; they return an `OpOut` description that the
runtime materializes at a single site. Zero `&mut Arena` remains in `core_ops`, `more_ops`, or
`bls_ops`. **Directive 3 delivered at no cost in memory or throughput.**

Six variants cover the whole operator set: `Same`, `Pair`, `Number`, `Small`, `Concat`, `Substr`,
plus `NumberPair` for `divmod`. `Concat` and `Substr` carry source nodes rather than bytes, so
large results are written straight into the arena with no intermediate buffer.

Two incidental changes: `SExpNumber` derives `Clone`; the arena exposes `stored_atom_count`,
`stored_pair_count`, `stored_heap_bytes`.

### Phase 2 — interning (commit `ac603ae`)

Only atoms of 32 bytes or more are shared, and **cons cells are never shared**.

The first implementation interned pairs too and cost **52 MiB of peak to save nothing**. The
reason is arithmetic that should have been done on paper first: a cons cell is 8 bytes, a hash-map
entry keyed on its two children is roughly four times that, so pair interning cannot break even
below a 4x repeat rate — and a 19.25M-node block is overwhelmingly distinct. Atom interning has
the same shape unless the payload dominates the entry, which is why the threshold exists. Small
integers are inline already and cost no storage, so they never belonged in a table.

Both intern paths charge the ghost counters on a hit, so `MAX_NUM_ATOMS`, `MAX_NUM_PAIRS` and the
heap ceiling still count every LOGICAL node. Sharing changes what is stored, never what a program
is permitted to build — without that, a program consing the same cell repeatedly would sail past
a consensus ceiling it used to hit.

### Phase 3 — reclamation (commit `acac5e9`)

A checkpoint is recorded when an operator application is queued, before its operands are
evaluated. When the operator returns a self-contained result, the pools are rewound to that point
before the result is written: everything the operand evaluation allocated is unreachable.

**This is where the Phase-1-first ordering paid off.** clvm_rs needs `gc_candidate` — a
hand-maintained per-operator predicate — because with the arena threaded through operators the
runtime can only INSPECT a returned handle and infer whether rewinding around it is safe. With
`OpOut` the variant states it: `Number` and `Small` are computed from scratch and borrow nothing;
every other variant references a node that may live inside the region being reclaimed. A
stale-predicate hazard became a type-level fact.

`Arena::restore` rolls back the ghost counters (reclaimed work must stop counting against the
consensus ceilings exactly as it stops occupying storage) and clears the intern map wholesale
(its entries index into pools being truncated; a surviving entry would resolve to whatever later
occupies the slot — the dangling-index hazard clvm_rs documented and mitigates with debug
fingerprinting).

### What the measurements corrected

- I reported a "36% peak regression from the operator conversion" and blamed `OpOut::Number`
  boxing. Both were wrong. Isolating interning behind a compile-time switch showed the conversion
  costs **zero** (142.94 MiB with interning off, exactly the baseline) and interning was the
  entire +51.7 MiB.
- The interning invariant caught that pair interning was inert before the peak measurement could
  mislead: `atoms 10 then 10, pairs 12 then 12` meant the second identical build deduplicated
  nothing, because pair interning does not work without atom interning. Peak alone would have
  read as "interning does not help this workload" — plausible and wrong.

---

## 8c. Consensus audit against the reference implementation

Prompted by asking what else clvm_rs had that we did not, this compared our operator dispatch and
flag ladder against `chia_rs` and `clvm_rs` directly. It found correctness gaps, not optimizations,
and they outrank everything in §8b.

Sources cloned for this: `../clvm_rs-history` (1307 commits) and `../chia_rs-history` (3024
commits). The authority for activation is `chia_rs`
`crates/chia-consensus/src/spendbundle_validation.rs::get_flags_for_height_and_constants`.

### FIXED — the flag ladder (commit follows §8b)

Three flags were keyed to the wrong fork. None of our gates could see it: the block fixtures sit at
4,671,894 and 9,179,155..9,179,200, below and above the entire affected window.

| flag | chia activates | we did | effect |
| --- | --- | --- | --- |
| `SIMPLE_GENERATOR` | soft fork 9 (8,655,000) | hard fork 1 (5,496,000) | **stricter than consensus for 3,159,000 blocks** — rejects generator refs and non-quoted generators that chia accepts |
| `COST_CONDITIONS` | hard fork 2 (unscheduled) | hard fork 1 | announcement accounting switched ~3.7M blocks early |
| keccak-outside-guard | hard fork 2 (unscheduled) | hard fork 1 | flag on, operator absent |
| `LIMITS` | soft fork 9 | soft fork 8 | identical on mainnet (same height); **diverges on testnet11**, where SF8=3,755,000 and SF9=3,924,000 |

The branch structure was also flattened. Hard fork 2 and soft fork 8 are mutually exclusive in
chia — the hard fork stops disabling modpow — and `LIMITS` applies only between soft fork 9 and
hard fork 2, because the bounded cost model subsumes it.

Our own `constants.rs` already documented the correct answer ("Hard fork 2: keccak/secp outside the
guard, COST_CONDITIONS…"); only the ladder disagreed. `core/tests/flag_ladder.rs` now pins every
transition against an independent transcription of chia's rules, including a testnet11
configuration where the two soft forks sit at different heights — mainnet hides that confusion by
putting them at the same height.

### OPEN — missing operators

Our dispatch stops at opcode 61; chia's reaches 65.

| opcode | operator | chia's gate | size in clvm_rs | status |
| --- | --- | --- | --- | --- |
| 62 | `keccak256` | `ENABLE_KECCAK_OPS_OUTSIDE_GUARD` (hard fork 2) **and** softfork extension 1 (**live now**) | 50 lines | missing |
| 63 | `sha256_tree` | `ENABLE_SHA256_TREE` | 16 lines | missing |
| 64 | `secp256k1_verify` | `ENABLE_SECP_OPS` (hard fork 2) | 104 lines (with 65) | missing |
| 65 | `secp256r1_verify` | `ENABLE_SECP_OPS` (hard fork 2) | — | missing |
| `0x13d61f00` / `0x1c3a8f00` | secp via softfork extension | always, inside the guard | — | missing |

Hard fork 2 is unscheduled (`0xFFFF_FFFA`), so the bare opcodes are not yet reachable. **The
softfork-guard route is reachable today**, which makes the next item the live one.

### OPEN — `softfork` does not execute its guarded program

Ours reads only the first argument, burns the declared cost, and returns nil. It never reads the
extension selector and never runs the program.

chia parses four arguments `[cost, extension, program, env]`, resolves the extension to an
`OperatorSet`, executes the program under it, requires the actual cost to equal the declared cost,
and discards the guard's allocations on exit via a checkpoint.

The directions matter and this one is the wrong way round:

- the flag-ladder bugs made us **stricter** than consensus — we would reject valid blocks and stall,
  which is a liveness failure;
- this makes us **more permissive** — a guarded program that should raise is silently accepted, so
  we could follow a chain the network rejects.

Our runtime also pre-evaluates operator arguments, so a guarded `(keccak256 …)` is evaluated as an
ordinary expression before `op_softfork` ever runs; with opcode 62 absent it becomes `op_unknown`,
yielding nil at token cost rather than a digest.

### Why no existing gate caught any of this

Every gate builds its programs from operators we already implement, and every block fixture sits
outside the affected height ranges. `opcode_coverage.rs` pins that every opcode *we know about* is
handled — it cannot know about one we have never heard of. A missing operator, or a rule keyed to
the wrong fork, is only visible by diffing against the reference implementation.

### Their process, worth adopting

`clvm_rs/docs/new-operator-checklist.md` is their own 13-step procedure for adding a consensus
operator: test vectors generated against another implementation, inclusion in the operator fuzzer,
a benchmark to establish cost, a `gc_candidate()` answer, an activation flag that must not collide
with chia_rs's shared flag space, and an entry in the fuzzing generator's operator table.

One step is a direct check on §8b's design: `gc_candidate()` asks whether an operator returns a
small atom so the interpreter can free everything its invocation allocated. Our `OpOut` answers
that structurally — `Number` and `Small` borrow nothing — so the predicate they maintain by hand
does not exist here.

---

## 9. What remains

### 9.1 Open, ranked

1. ~~Convert the remaining operators.~~ **Done** — see §8b, Phase 1.
2. **Recursive `Drop` stack overflow.** Deeply-nested CLVM overflows the stack when the `SExp`
   tree is dropped — `PairBuf::Owned` nests a child `SExp`, so ~N frames unwind. The consensus
   generator path builds such trees (`generator.sexp().to_owned()`). Recorded as an ignored red
   gate in `clvm_adversarial_limits.rs`. **Index handles eliminate it for free**, which is an
   additional argument for the compact representation. Reachability within block cost and size
   limits needs a security assessment before rating it.
3. **Restore `MAX_NUM_ATOMS` / `MAX_NUM_PAIRS`** if the bumpalo branch is ever revived — they
   vanished with `arena.rs` and the restored runtime has no cap at all.
4. ~~Take the clvm_rs improvements we skipped.~~ **Interning and reclamation done** (§8b).
   Wider small-atom inlining remains: the handle carries 26 bits, and atoms average 1.8 bytes, so
   more of them could avoid storage entirely.
5. **Tests still missing**: the interning invariant if we add interning (tree hash unchanged,
   node counts drop); end-to-end throughput after full operator conversion.

### 9.2 Explicitly ruled out

- **`bump-scope` or any scoped-rewind allocator** as a fix for peak. It reclaims garbage; §4.1
  shows the winning design reclaims nothing. The lever is node size.
- **`set_allocation_limit`** as a fix. It converts growth into rejection of valid blocks.
- **Patching bumpalo.** No defect exists; the behavior is documented and intentional.

### 9.3 A design sketch, if the node representation is ever revisited

Not built, and the compact arena already implements most of it — recorded for completeness:

```rust
struct Ref(u32);           // tag:2 | payload:30
// 00 pair -> pairs index   01 atom -> spans index
// 10 inline (≤3 bytes in the payload)   11 const (nil/one)

struct Heap {
    pairs: Vec<[Ref; 2]>,    // 8 B
    spans: Vec<(u32, u32)>,  // 8 B, only for atoms > 3 bytes
    blob:  Vec<u8>,          // one contiguous byte arena
}
```

Estimated 75–90 MiB on block 4,671,894 if inline coverage reaches the 1.8-byte average. The
30-bit payload caps pools at ~1G nodes — fine against the 62.5M consensus limits, but it needs
the same ghost accounting the current arena has so inlining cannot shift those limits.

---

## 10. Resuming

```sh
# the investigation state
cd /Users/grantcermak/Development/irulast/dgxch-pr-fold
git show clvm-allocator-investigation          # tag message repeats the findings

# the restored bumpalo runtime, for comparison measurements
cd /Users/grantcermak/Development/irulast/dgxch-bumpalo
git show bumpalo-runtime-restored

# CNI history
cd /Users/grantcermak/Development/irulast/clvm_rs-history
git log --oneline --follow -- src/allocator.rs

# run long jobs on the builder without the reaper killing them
scripts/cluster-run.sh /scratch/out.txt '<command>'
```

### Related, and finished

PRs **#49–#53** against `GalactechsLLC/dg_xch_utils` are all green (lint, build on
macos + ubuntu, dependency audit) and land cleanly in sequence, verified with
`git merge-tree --write-tree`. That work is independent of this investigation.

---

## 11. The summary to give DaOneLuna

- **You were right that your allocator does not leak.** Proven across seven shapes at zero bytes
  retained, including the mempool path and both error paths. Your two fixes closed the OOM; the
  rewrite landed afterward and was not what fixed it.
- **You were right that it stays consensus-correct.** 46 blocks against on-chain costs, 16 blocks
  byte-identical, 132/132 operator vectors. The one divergence I found was my porting error.
- **You were right about the API, and it is better than we thought.** Operators can take `&Arena`
  and be structurally incapable of allocating, at measured parity — including on `concat`, where
  the obvious version would lose. CNI hit a borrow-checker wall here and concluded it was
  impossible; splitting read from write gets around it.
- **You were not cloning anyone.** CNI shipped your exact `Arc`-tree design in 2020 and dropped
  it in 2021. Convergent evolution under identical constraints.
- **The one thing that does not survive**: a bump allocator cannot bound memory *within* a run,
  because it frees nothing until reset — bumpalo's own source says so. Block 4,671,894 costs
  1026 MiB under it and 143 MiB under the compact arena, and the difference is per-node storage,
  not reclamation. That is the specific, measured reason for a different storage layer, and the
  only one.
