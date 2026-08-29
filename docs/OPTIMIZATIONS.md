# The Full-Node Performance Campaign: Optimizations, Complexity Analysis, and the Sync-Time Floor

This document records every performance optimization landed on the `full-node` branch,
the algorithmic analysis behind each, and a postulated theoretical minimum for a
fully-validating genesis sync of Chia mainnet under the best possible code disposition.

Evidence discipline: every quantitative claim is labeled
**[CITED]** (measured on our hardware or read from the code at the cited location),
**[DERIVED]** (computed from cited quantities; arithmetic shown), or
**[ASSUMPTION]** (stated model input; the error bars live here).
Negative results are recorded with the same care as wins — a rung that measured slower
is knowledge, and it is what stops the same idea being re-tried later.

Hardware references: "Xeon" is the cluster builder/node class (E5-era, ~10 usable cores
in our pods); "M-series" is an Apple laptop; the Pi profile targets a Raspberry Pi 4
(4×A72, 4 GiB). All per-op numbers name their machine.

---

## 1. The measured ratchet

Live genesis-sync block rate on the Postgres node (`dg-xch-node-pg`), in order of landing.
Every step was measured on the live node or an offline corpus replay before it shipped
**[CITED]**:

| # | Change | Rate (blocks/s) | Where measured |
|---|--------|-----------------|----------------|
| 0 | Baseline (correct but serial) | 0.83 | live node |
| 1 | Per-block VDF proof parallelism | 2.04 | live node |
| 2 | Next-window prefetch overlap | 2.43 | live node |
| 3 | GMP for primality checking (discriminant generation) | 3.44 | live node |
| 4 | Postgres async commit (`synchronous_commit=off`) | 5.0 | live node |
| 5 | One store batch per block | 5.7 | live node |
| 6 | Cross-block window VDF pipeline | 5.8 (tx era) | live node |
| 7 | Window-level CLVM + BLS body parallelism | 6.3 → **14.78** (2.36×) | era-a corpus replay |
| 8 | Confirm WAL-stall attack: `wal_compression=zstd` (config, Postgres) + batched sync-before-link (code, mmap); pg coin-journal measured OUT (§6 ledger) | 3.7 → **6.7** (1.8×, config alone, dust era) | a live EPYC host |

Offline anchors: the genesis-1024 corpus replay went **842 s → 54 s** across the campaign
**[CITED]**. Store I/O per block went **230 ms → 2 ms** (steps 4+5) **[CITED]**.
The two-node `--sync-from` split (§3.9) cuts full-chain *calendar* time from ~23 days
to ~1 week **[CITED]** (measured node rates, extrapolated over the remaining spans).

Era-dependent composite rates on the current code **[CITED]**: era-a (2022 tx peak)
14.78 blocks/s; era-b (heavy modern, 4.69M) 5.40 blocks/s; era-c pre-fork light stretch
443 blocks/s; era-c through the hard fork 12.5–31.7 blocks/s; live early-era ~6 blocks/s
(VDF-bound). The spread is real: per-block work varies by two orders of magnitude across
eras, which is why single-number "blocks per second" claims about any Chia node need an
era attached.

---

## 2. Where the time goes (measurement before optimization)

Two instruments drove the campaign; both are in-tree and rerunnable.

**Pipeline spans** (`tracing` spans on the live node): per 32-block window on the Xeon,
post-campaign — `window.vdf` ≈ 2.66 s across all cores, `window.body` 46–146 ms for
8–15 tx blocks, `block.stage` ≈ 5–56 ms, `record.derive` ≈ 20 ms, `store.persist` ≈ 2–4 ms
(Postgres) / ≈ 19 ms (mmap, first measurement) **[CITED]**.

**Class-group phase profiler** (`vdf` feature `phase-profile`,
`vdf/tests/phase_profile.rs`): decomposition of one NUDUPL squaring on the real
1024-bit discriminant, Xeon **[CITED]**:

| Phase | µs/op | Share |
|---|---|---|
| `lehmer_gcdinv_sw` (full GCD-inverse) | 14.58 | 54.6% |
| `xgcd_partial_sw` (partial GCD) | 6.14 | 23.0% |
| Composition multiplies/divisions | 3.61 | 13.5% |
| k-prep (mul + div_exact + div_mod_floor) | 1.21 | 4.6% |
| `to_bigint` + `reduce` | 1.14 | 4.3% |

The two Lehmer loops are **77.6%** of squaring; squaring is the unit of VDF verification;
VDF verification is the dominant irreducible work of sync (§5). This chain of dominance
is why the campaign kept returning to `vdf/src/form.rs` and `vdf/src/limbs.rs`.

---

## 3. The optimization catalog

Each entry: what changed, why it works, the complexity view, and the measured delta.

### 3.1 Store I/O collapse: async commit + one batch per block

*Where:* `stores/src/postgres/mod.rs` (per-connection `synchronous_commit=off`),
`stores/src/{sqlite,postgres,mmap}/{block,coin}.rs` `_in`-suffixed batch variants.

A block's persistence (record + body + coin deltas + status) previously issued many
autocommit statements — each paying a WAL fsync round trip. Now every per-block write
lands in **one** batch on **one** transaction, and Postgres group-commits asynchronously.

*Complexity:* unchanged asymptotically — O(adds + removals) rows per block — but the
**constant** collapsed from ~dozens of fsync-bound round trips to one:
**230 ms → 2 ms per block [CITED]**. This was the single largest constant-factor win of
the campaign and it is pure systems hygiene, no algorithm involved. Durability note:
async commit can lose the last few commits on a crash; for a *validating re-sync* the
data is reproducible by construction, so the trade is sound (the tip-follow steady state
keeps the same setting because the store is a cache of the network's truth, not the
authority).

### 3.2 Per-block, then cross-block (window) VDF parallelism

*Where:* `node/src/header.rs` (`QueuedVdf`, `VdfSink`, `verify_vdf_batch`),
`node/src/sync/mod.rs` (`follow_blocks_reporting`), `node/src/engine.rs`
(`stage_block_pre`, staged-delta confirm).

Header validation (`core/src/consensus/block_header_validation.rs`) checks up to ~13
VDF proof sites per header (challenge/reward/infused-challenge chains at sub-slot,
signage-point, and infusion-point positions). Stage 1 verified one block's proofs in
parallel (0.83 → 2.04 **[CITED]**). Stage 2 restructured the whole sync step: the
sequential header walk *defers* every proof into a window-wide sink, the entire
32-block window's proofs drain across all cores in one batch, and blocks confirm
in order afterwards. A failed drain bisects by queue watermark to name the offending
height; the clean prefix still confirms.

*Why cross-block beats per-block:* a block has a variable number of proofs (2–5 typical,
more at sub-slot boundaries); per-block batches under-fill the machine at the tail of
every block (Amdahl on a 2–5-way batch). A 32-block window pools ~1M form-ops
(**[DERIVED]** 2.66 s wall × ~10 draining cores ÷ 26 µs ≈ 1.0M) — enough to saturate
any core count we own.

*Complexity:* total work unchanged; wall-clock per window goes from
`Σ_b max_chain(b)` to `(Σ_b ops_b)/C + drain overhead`. Genesis-1024 replay: 842 s → 54 s
across the campaign; the window pipeline's own step measured 5.7 → 5.8 live in a tx era
and dominated the offline replay gain **[CITED]**.

### 3.3 Window-level CLVM + BLS body parallelism

*Where:* `node/src/engine.rs` (`run_body_expensive`, `PrecomputedBody`),
`node/src/sync/mod.rs` (the `window.body` scope).

The expensive *pure* half of body validation — CLVM generator execution plus BLS
aggregate-signature verification — has no dependence on validation state (only on the
resolved generator refs and the block bytes). It now precomputes for every tx block of
the window in parallel (`std::thread::scope`) before the sequential stage loop, which
consumes the results. After DIVERGENCE-23 established that the CLVM flag ladder keys on
the block's own height, a precomputed body is *always* valid — the guard machinery was
deleted.

*Complexity:* unchanged work, wall-clock `Σ body_b → max(Σ body_b / C, longest single body)`.
Measured: era-a replay 6.3 → **14.78 blocks/s (2.36×)**; live `window.body` = 46–146 ms
for what was previously ~62 ms *per tx block* serial **[CITED]**.

### 3.4 Weight-proof validation: ~18 min → seconds-to-low-minutes

*Where:* `full-node` weight-proof path (`dg_xch_weight_proof`), phases logged
`1..6`. Sampled sub-epoch segments verify in parallel across threads (phase 4:
"sampled segments verified in parallel", 1224 VDFs across 8 threads in a measured run
**[CITED]**, 33.9 s), and the validated proof is cached and reused across driver ticks
("reuse ANY already-validated proof" — mainnet's tip advances every block, so an
exact-tip cache never hits). ~10× total **[CITED]**.

### 3.5 Fixed-limb, no-allocation class-group arithmetic (the chiavdf structural technique)

*Where:* `vdf/src/limbs.rs` (`Sw<const N>`: sign + length + `[u64; N]` on the stack;
`SwGcd = Sw<20>`, `SwWide = Sw<34>`), consumed throughout `vdf/src/form.rs::square_with`.

The Lehmer loops' operands are bounded (≤ ~1088 bits by the NUCOMP partial-reduction
invariant), so they fit fixed stack arrays — no heap allocation, no `BigInt` sign/limb
bookkeeping, `Copy` semantics. `BigInt` survives only at the Form boundary and in the
rare exact-division fallback. Every op is differentially property-tested against
`num_bigint` (`vdf/src/limbs.rs` tests) and the whole squaring against FLINT/chiavdf
vectors (`vdf/src/form.rs::nucomp_tests`).

### 3.6 The an earlier campaign item rung: fused carry-chain `linear2` + hardware-division word loop

*Where:* `vdf/src/limbs.rs::linear2`, `vdf/src/form.rs` (both `_sw` Lehmer loops).

Phase profiling (§2) placed 77% of squaring in the Lehmer loops; within them, the cost
is (a) `linear2` — the 2×2 matrix application `x·w1 + y·w2` — previously three passes
(two `mul_word_mag`, then `add_signed` with a magnitude compare), and (b) the inner
word-GCD quotient `rr2 / rr1` on `i128`, which is a `__divti3` **libcall** (~30+ cycles),
taken ~35 times per outer iteration.

The fix: `linear2` is now **one** fused pass of pure `u64`
`overflowing_add`/`overflowing_sub` chains — exactly the `adc`/`sbb` shape hand assembly
would use, which LLVM lowers to it. Same-effective-sign inputs run a fused addmul-2;
opposite signs a fused submul-2 in two's complement with a single complement pass when
the result is negative. The word extraction narrowed by one bit
(`bit_len − 63 + 1`) so the inner quotient divides on the 64-bit unit.

Measured `square_with` A/B, real 1024-bit discriminant **[CITED]**:
Xeon **25.8 → 24.5 µs/op**; M-series **9.96 → 8.68 µs/op**. The ARM-side gain matters
disproportionately: the Pi's A72 pays even more for `__divti3` than the Xeon does, and
the Pi is the profile whose floor this loop defines.

**Negative result — BMI2/ADX intrinsics [CITED]:** a full `_mulx_u64` +
`_addcarry_u64`/`_subborrow_u64` variant (runtime-detected, differentially tested
byte-identical over a sign/size sweep) measured **equal** to the portable kernel
(24.97 vs 24.55 µs). LLVM already emits the carry chains from the shaped Rust.
Instruction selection is exhausted as a lever; the variant was deleted rather than
shipped. What remained of chiavdf's edge was believed *structural* — fully in-place
two's-complement state, no per-iteration copies, no sign/length bookkeeping — a
representation redesign (~1–2 days, consensus-critical), scheduled to accompany the
real-Pi hardware run where its effect was expected largest.

**That redesign has since been executed and measured OUT on the real Pi-4 hardware**
(§6.12): byte-identical to this kernel across 7,392 differential property cases plus a
300-step real-discriminant walk, but **4% slower on the Xeon and 6% slower on the
Cortex-A72** — the A72-gains-disproportionately hypothesis is refuted, and the
sign-magnitude fixed-limb kernel above stands as the production representation. The
run also produced the first real-Pi t_op: **39.3 µs/op** on the Cortex-A72 (portable
kernel, dedicated machine) **[CITED]** — the §5 floor model's Pi anchor.

**Negative result — GMP forms [CITED]:** both a naive `rug` NUDUPL (0.66× — 39.6 µs)
and a fully preallocated scratch-buffer rewrite using in-place `extended_gcd_mut` /
floor-division assigns (0.44× — 59.5 µs) are byte-correct but *slower* than the
fixed-limb path at 1024-bit operand sizes. GMP's strength is asymptotic (subquadratic
multiplication engages orders of magnitude above our 16-word operands); at our sizes its
allocation and generality overheads dominate. GMP is judged **out** for form arithmetic
(it remains in use for primality checking, §1 step 3, where it won 3.44/2.43 = 1.42×).

### 3.7 CLVM arena allocation and the destructor leak

*Where:* `core/src/clvm/runtime.rs::alloc`.

The CLVM VM allocates nodes in a `bumpalo` arena — O(1) pointer-bump allocation, zero
per-node free. The subtlety: **bumpalo runs no destructors**, so storing an
`AtomBuf::Owned(Arc<Vec<u8>>)` in the arena leaked the atom's backing buffer —
~90 MB per transaction block of CLVM churn, ~3 MB/block of retained RSS that OOM-cycled
the live node at 24 GiB. The fix copies atom bytes into the arena
(`alloc_slice_copy`) and lets the `Arc` drop at allocation time: flat RSS *and* **+22%
CLVM throughput** **[CITED]** (fewer atomics, better locality). The live node has since
held ~540 MB flat across >650k blocks **[CITED]**. (Superseded structurally by §3.11: the
bumpalo tree arena is gone — the VM now runs on clvm_rs-style compact typed pools.)

### 3.8 Prefetch overlap and peer rotation

*Where:* `full-node/src/daemon.rs` (driver), `node/src/sync/window.rs` (reservation window).

The next window's download launches before this window's validation begins; a peer that
stalls or rejects has its reservation reclaimed and rotated. Overlap removed the serial
network stall between validation bursts: 2.04 → 2.43 **[CITED]**. The reservation
window itself enforces the libbitcoin memory contract W == P == 8
(`node/src/sync/mod.rs::TARGET_OUTBOUND`).

### 3.9 Chain-segmented validation: `--sync-from`

*Where:* `full-node/src/daemon.rs::anchor_at`, `node/src/sync/mod.rs`
(`missing_ref_heights`/`seed_ref_generator`), `node/src/engine.rs` (staged-generator
overlay).

A second node validates the back half of the chain concurrently: it validates the
weight proof, fetches the 96-block anchor span (in ≤32-block chunks — peers reject
wider `RequestBlocks`), runs the headers-first pass over it, and follows from there.
Generator back-refs that point below the anchor span (observed reaching 161k blocks
down **[CITED]**) are fetched from peers on demand and seeded into the engine's overlay.

*Complexity:* embarrassingly parallel across K nodes — each validates N/K blocks; the
weight proof (minutes) is the only duplicated work. K=2 measured: ~23 days → ~1 week
calendar **[CITED]**. §5 treats K-way segmentation as part of the best disposition.

### 3.10 The mmap (libbitcoin) store profile

*Where:* `dg_xch_stores` mmap backend; deployed in-cluster as `the mmap deployment` at a
Pi-4 resource envelope (4 cores / 4 GiB). First measurements **[CITED]**:
confirming from genesis within seconds of boot, `store.persist` ≈ 19 ms/block (vs 2 ms
Postgres-async — the write-amplification study is open), container memory ≈ 18 MB at
start of sync. Included here for completeness; its optimization story is just beginning.

### 3.11 Compact-arena CLVM VM: NodePtr typed pools

*Where:* `core/src/clvm/arena.rs` (new), `runtime.rs`, `dialect.rs`, `core_ops.rs`,
`more_ops.rs`, `debug_ops.rs`, `utils.rs`.

§3.7 fixed the bumpalo destructor leak but left the structural cost in place: every
op result was deep-copied into a bump arena that retained **all** eval intermediates
until end-of-run. The phase probe attributed 429 of 430 MiB of a dust-era block's
peak to the ROM bootstrap run alone, and the window pipeline multiplied that by the
worker count (8 workers ≈ 1 GiB; a 32-core era replay ≈ 7 GiB steady) **[DERIVED,
`core/tests/rom_phase_probe.rs` / `node/tests/body_mem_probe.rs`]**.

The replacement mirrors clvm_rs 0.17.7 `allocator.rs` — the representation proven at
mainnet scale **[CITED]**: 32-bit `NodePtr` handles (6-bit object tag, 26-bit index)
into typed pools (`pair_vec`: 8-byte cons cells; `atom_vec`: `(start,end)` spans into
a grow-only byte heap), canonical positive integers ≤ 26 bits encoded inline in the
handle, and ghost accounting so the inline/zero-copy optimizations cannot change the
consensus `MAX_NUM_ATOMS`/`MAX_NUM_PAIRS` (62,500,000) limits. The eval loop and all
operators run on `NodePtr`: cons is a pool push, path traversal is a handle copy, an
op result allocates exactly once, and `new_substr`/`new_concat` keep clvm_rs's
zero-copy span/ghost shortcuts. `ClvmRuntime::new/run` signatures are unchanged;
program/args are imported into the arena once per run and the result exported once.
All `unsafe` left the VM (the old lifetime-widening `Bump::alloc` blocks are gone).

*Measured (locked-bench A/B at 8ab7559 vs 879e0e4, era-b dust corpus)* **[DERIVED]**:

| Metric | old tree-arena | compact arena | delta |
|---|---|---|---|
| ROM bootstrap run h=4694232, Xeon | 429 MiB / 1.12–1.40 s | **50.5 MiB / 0.46–0.67 s** | 8.5x / ~2.3x |
| Full per-block pipeline, Xeon | 1.10–1.14 s | **0.48–0.49 s** | 2.3x |
| Single-block peaks (9 dense blocks) | 213–438 MiB | **37–57 MiB** | ~8x |
| 8-worker window batch | 1.02 GiB / 1.61 s | **0.35 GiB / 1.13 s** | 2.9x / 1.4x |
| era_replay 8192 blocks (40-thread) | 5.11 blk/s, 7.06 GiB RSS | **6.06 blk/s, 3.09 GiB RSS** | +18.6% / −56% |
| Pi-4 A72, dedicated: ROM run | 430 MiB / 2.46–2.47 s | **52–54 MiB / 0.77 s** | 8x / 3.2x |
| Pi-4 A72: full per-block pipeline | 2.44–2.49 s | **0.82 s** | 3.0x |

Semantics: every probe reports identical cost (351,943,684 ROM / 4,690,351,684 total
at h=4694232) and identical spends on both VMs; the 8192-block era replay validates
end-to-end against the chia-attested records with zero walls, and the full core +
node suites are green in release and debug, clippy `-D warnings` clean.

One deliberate behavior change: a runaway allocation bomb (the chia `TEST_GENERATOR`)
now trips `TooManyPairs` just before the cost roof instead of `CostExceeded`, because
the consensus pair limit now exists at all. clvm_rs counts eval pairs identically
(one `new_pair` per operand via `cons_op`; checkpoint restores convert freed pairs to
ghost pairs), so this is the mainnet-faithful outcome (`core/tests/generator_tools.rs`
documents it).

Not ported (deliberately): clvm_rs's `gc_candidate` transparent-checkpoint GC, which
frees intermediates mid-run after selected operators. Ghost accounting makes it
invisible to the limits, so it is a pure peak-memory refinement on top of this
representation — worth revisiting only if adversarial deep-`apply` programs show up
as a memory concern; the dust-era working set is already ~50 MiB/run.

*Follow-on levers (measured shape, not yet taken):* (a) the remaining ~50 MiB/run and
a slice of wall is the `SExp` tree boundary — programs are parsed into an `Arc`-tree
`SExp` and imported into the arena, and the result is exported back; parsing the
serialized generator directly into the arena (and reading conditions straight from
`NodePtr`s) removes both copies. (b) With eval overhead ~2–3x down, sha256
(`compress256`, software on both fleet Xeons and the Pi) is an even larger share of
the remaining CLVM CPU (30% of the sync-from node's window, sha2-dominated); puzzle-reveal hash
dedup across a block's repeated puzzles is the algorithmic attack there.

---

### 3.11 Discriminant memoization, BPSW reference parity, and the per-proof NUCOMP bound

*Where:* `vdf/src/discriminant.rs` (`DISCRIMINANT_CACHE`, `is_probable_prime`),
`vdf/src/proof.rs::check_n_wesolowski`, `vdf/src/form.rs::fast_pow_form_with`.

*Target:* post-§3.6 flamegraphs (`temp/flamegraph-pg-POSTROLL-20260818.svg`) showed
genesis-syncing nodes spending 25.8% of CPU in VDF verification with
`discriminant::hash_prime` alone at **10.6% of total CPU [CITED]** — the Fiat-Shamir
prime derivation was being re-run from scratch for every proof checked.

**The redundancy, measured** (probe: `vdf` feature `hashprime-probe`, per-call seed
hash + candidate count over the genesis-1024 corpus replay) **[CITED]**: 16,230
`hash_prime` calls over mainnet blocks 0..=1023 — 5,098 discriminant derivations
(1024-bit) collapsing to just **1,094 unique challenges** (4.7× redundancy; the hottest
challenge derived 110×; ~381 primality candidates per derivation), plus 11,132 `get_b`
derivations (264-bit) that are **~90% unique** — every VDF on a chain shares its
sub-slot's challenge, while `get_b` hashes the proof forms themselves. Three changes:

1. **Bounded challenge→prime memo** (200 entries, LRU, `Mutex<Vec<…>>`) in
   `create_discriminant_int` — mirroring the reference node's `@lru_cache(maxsize=200)`
   on `get_discriminant` (chia-blockchain `chia/types/blockchain_format/vdf.py`
   **[CITED]**). `get_b` is deliberately NOT cached (measured ~90% unique). Post-change
   corpus probe: derivations drop 5,098 → 1,206 (the 112 over the 1,094 unique are
   parallel verifiers racing the same challenge before first insert — benign; the
   derivation is deterministic) **[CITED]**.
2. **Primality parity with chiavdf**: `is_probably_prime(30)` → `is_probably_prime(24)`.
   GMP treats reps ≤ 24 as exactly its Baillie-PSW test (MR base 2 + strong Lucas;
   vendored `gmp-6.3.0-c/mpz/millerrabin.c`: `reps -= 24; if (reps > 0)` adds
   random-base rounds only above 24 **[CITED]**), and chiavdf's `integer::prime()` is
   `is_prime_bpsw` — strengthened BPSW with **no** extra MR rounds (chiavdf
   `src/integer_common.h` → `src/primetest.h` **[CITED]**). Exceeding the reference is
   not extra safety: a BPSW pseudoprime (none known) that chiavdf accepts but an extra
   round rejects would make our search continue to a *different* prime — a consensus
   fork. Parity means exactly BPSW.
3. **Per-proof NUCOMP bound** in the verifier: `check_n_wesolowski` now computes
   `nucomp_bound` once and threads it through every segment and the final Wesolowski
   check (`fast_pow_form_with`), where the prover already did (§`prove_with_discriminant`).
   Previously each verify-side exponentiation paid two 1024-bit sqrts and each segment
   composition re-derived the discriminant via b²−4ac.

*Measured* (an offline builder Xeon, interleaved A/B binaries, medians of 7 / 3 rounds under
concurrent load) **[CITED]**: repeated-challenge fixture verify **92.0 ms → 10.3 ms/op
(8.9×)**; distinct-seed derivation unchanged within noise (34.1 vs 34.6 ms — the reps
change is ~2%, it is a correctness/parity fix); genesis-1024 corpus replay wall
**91.4 s → 60.0 s (1.52×)**, all runs fully green. Consensus gates: chiavdf discriminant
vectors byte-identical, two new differential cache tests (miss/hit/eviction paths ≡
direct derivation), full vdf + weight-proof suites release+debug.

*Next lever:* the reference node also memoizes whole proof verifications
(`@lru_cache(maxsize=1000)` on `verify_vdf`, same file **[CITED]**) — worth measuring
whether identical (challenge, proof) pairs recur across our validation contexts before
porting. (Measured and ported in §3.12.)

---

### 3.12 Window VDF drain: dedup + LPT work-stealing + the whole-proof memo + saturated-serial segments

*Where:* `node/src/header.rs::verify_vdf_batch` (dedup, LPT work-stealing dispatch),
`vdf/src/proof.rs::verify_vdf` (memo) / `verify_vdf_serial`,
`vdf/src/validation.rs::validate_vdf_info_serial`,
`node/src/primitives.rs::ConsensusPrimitives::verify_vdf_serial` (defaulted seam method).
Measurement instrument: `dg_xch_node` feature `drain-probe`
(`DGXCH_DRAIN_PROBE=wall|serial`), per-batch wall/process-CPU/recurrence and, in
`serial` mode, the per-proof cost split.

*Target:* post-an earlier campaign item the per-window VDF drain was the fleet's largest busy phase
(Pi/A72 109–118 ms/blk, mmap 81–154, sync-from 122, Postgres 51–79, EPYC 14–27; live idle 0–12%).

**The split, measured** (era-e corpus, 32-block windows, 12-CPU-cgroup Xeon builder,
serial probe over 27 windows) **[CITED]**: ~159 proofs/window (~5.0/block); **8.1–8.5%
of queued proofs are byte-identical repeats** (blocks sharing a signage point carry the
same sp VDF proofs — the recurrence §3.11 deferred, now measured); per-proof serial
cost 8.9–216 ms (witness_type 3 ≈ 55% of count; cost scales with segment count, and
Wesolowski verify cost is *iteration-count independent* — the verifier's exponents are
~264-bit regardless of T, so the "iterations × t_op" floor is the PROVER's floor, not
ours); drain work 15.4 s/window against a 2.05 s measured wall = **1.60× the 12-CPU
work floor**, with the contiguous-chunk imbalance predicting exactly 1.60×
(chunk_max/chunk_mean). The imbalance was the whole gap.

Four output-identical changes:
1. **Exact-bytes dedup before dispatch** — one representative per identical
   (input, info, proof, target); the batch AND over duplicates equals the AND over
   uniques. Dedup must live at dispatch: a parallel drain runs identical proofs
   CONCURRENTLY, so a result memo alone misses them (measured: memo-only left
   process CPU flat).
2. **LPT work-stealing dispatch** — an atomic cursor over a witness_type-descending
   order replaces contiguous pre-chunking; a failed proof flips a shared flag and the
   window bisect attributes the height exactly as before.
3. **Whole-proof memo** (capacity 1000, exact length-prefixed argument bytes as key,
   `true` and `false` cached) — the §3.11 next-lever port of the reference node's
   `@lru_cache(maxsize=1000)` on `verify_vdf`
   (chia `chia/types/blockchain_format/vdf.py` **[CITED]**); serves cross-window and
   live-tip recurrence (unfinished/finished/gossip re-verification).
4. **Saturated-serial segments** — when the drain has ≥1 proof per worker, verify on
   the worker thread only: the per-segment two-thread split bought no wall there and
   cost +17% process CPU (measured); single proofs at the live tip keep the split.
   Chia comparison: chia's long sync submits one process-pool job per block over
   32-block batches (`full_node.py::prevalidate_blocks` →
   `multiprocess_validation.py::pre_validate_block`,
   `MAX_BLOCK_COUNT_PER_REQUESTS = 32` **[CITED]**) — block-granular, VDFs serial
   inside each job; our drain is proof-granular across the same batch shape, strictly
   finer.

*Measured* (era-e corpus A/B, `window.vdf` wall, all nodes fully green) **[CITED]**:

| Host shape | before | after | delta | after vs work floor |
|---|---|---|---|---|
| 12-CPU-cgroup Xeon (126 windows) | 54.5 ms/blk | **38.0 ms/blk** | −30.3% | 1.04× |
| 4-CPU taskset Xeon (40 windows) | 133.0 ms/blk | **106.8 ms/blk** | −19.7% | 1.01× |
| real Pi-4 A72, node stopped (24 windows) | 230.2 ms/blk | **196.6 ms/blk** | −14.6% | 1.02× |

The drain is now within 1–4% of its work floor on every measured shape: further wall
reduction must come from *reducing work* (per-op t_op in `form.rs`/`limbs.rs`,
`get_b` hash-prime derivations) or from overlapping the drain with the sequential
stage walk — not from scheduling.

Consensus gates: chiavdf vectors byte-identical, memo differential tests (miss/hit/
corruption/bounded), serial-vs-parallel differential, full vdf + node + weight-proof
suites release+debug, boundary_sweeps, era-a 8,224-block + era-e full corpus replays
confirmed green.

---

### 3.13 Fused Straus/Shamir pair exponentiation: one squaring chain per Wesolowski check

*Where:* `vdf/src/form.rs::fast_pow_form_pair_with` (new),
`vdf/src/proof.rs::pow_pair_product` (replaces `pow_pair`), consumed by
`verify_segment`/`verify_wesolowski` on the serial (`parallel=false`) path.

*Target:* §3.12 left the drain within 1–4% of its scheduling floor and named work
reduction as the next lever. Convergence flamegraph (sync-from node, tx-dense, live):
`verify_vdf_serial`/`check_n_wesolowski` = 52.6% of CPU, `fast_pow_form_with` = 37.95%.

**The pricing, instruction level first** **[DERIVED, an offline builder]**: objdump of the built
kernel confirms §3.6/§6.12 stand — `linear2` has zero mid-loop calls (panic path only),
the Lehmer body is 4×`linear2` + the rare BigInt fallback, no `__divti3`; t_op measured
24.6 µs Xeon / 39.7 µs Pi-A72 (parity with the §6.12 anchors). The kernel is at its
floor. The waste was **structural, one level up**: each Wesolowski check evaluates
`witness^b` and `x^r` as two INDEPENDENT ~264-bit 4-bit-window chains — per chain
14 table multiplies + ~260 squarings + ~62 window multiplies ≈ 336 group ops (measured
8.02 ms ≈ 326 t_op), so the serial path (the saturated drain, §3.12 change 4 — the
path carrying the 52.6%) pays ~673 group ops per check, and the verifier only ever
consumes the PRODUCT of the two powers.

**The fix**: Straus/Shamir simultaneous double exponentiation (Knuth Vol. 2 §4.6.3
**[CITED literature]**) — one interleaved chain, two 4-bit tables, the squaring run
shared: 28 table multiplies + ~260 squarings + ≤2 window multiplies per window ≈ 411
group ops, predicted 0.61×. Group-identical by the same argument the windowed loop
already relies on (unique reduced representative; differentially gated byte-identical
over a 144-pair exponent-size ladder crossing every route boundary — zero, 1-bit,
≤64-bit fallback, 65-bit, 264-bit top-bit-set). The latency-bound parallel path (live
tip, single proof) deliberately keeps the §3.12 two-thread split: its critical path is
one ~336-op chain < the ~411-op fused chain.

*Measured* (interleaved A/B under the bench lock, all gates green) **[CITED]**:

| Probe | before | after | ratio |
|---|---|---|---|
| pair product micro, Xeon | 15.9 ms/op | **10.0 ms/op** | 0.63× |
| pair product micro, real Pi-4 A72 (±0.3% spread) | 26.6 ms/op | **16.8 ms/op** | 0.633× |
| whole-proof serial verify (vdf.txt fixture), Xeon | 15.4 ms/op | **9.9 ms/op** | 0.64× |
| whole-proof serial verify, real Pi-4 A72 | 26.4 ms/op | **16.8 ms/op** | 0.635× |
| `window.vdf`, era-e full, 12-worker Xeon (126 windows) | 39.4 ms/blk | **27.5 ms/blk** | −30.2% |
| `window.vdf`, era-e-first24, 4-CPU taskset Xeon | 108.9 ms/blk | **75.2 ms/blk** | −31.0% |
| `window.vdf`, era-e-first24, real Pi-4 (node stopped) | 198.5 ms/blk | **143.3 ms/blk** | −27.8% |

The before-runs reproduce §3.12's after-numbers (39.4 vs 38.0, 198.5 vs 196.6), so the
gain composes with the drain work rather than re-measuring it. Consensus gates: chiavdf
vector suites, the 144-pair fused-vs-composed differential, serial-vs-parallel and memo
differentials, full vdf + weight-proof suites release+debug, scoped clippy
(vdf/node/weight-proof, all targets, `-D warnings`) clean, era-a 8,192-block corpus
replay green (25.96 blk/s), byte-identical by the replay's chia-attested-record check.

*What remains at this level (named, not yet taken):* (a) signed-window/wNAF interleaving
— class-group inversion is a `b`-negation (free), so w=5 odd-power tables would cut the
fused chain to ~368 ops (predicted ~0.90×, a second consensus-critical surface for ~10%;
measure before believing); (b) `get_b` hash-prime derivation, now ~10–15% of a serial
check (0.8–1.45 ms/op, ~90% unique so uncacheable, §3.11); (c) the squaring kernel
itself is closed (§3.6, §6.12, and the objdump above).

---

## 4. Algorithmic complexity of the hot path

Notation: n = discriminant bit-size (1024 on mainnet), W = machine word (64),
k = n/W = 16 words; C = cores; N = chain height. "Word-op" = one 64×64→128 multiply-add
or comparable.

### 4.1 NUDUPL squaring (`vdf/src/form.rs::square_with`)

The squaring is FLINT's `qfb_nudupl` (Shanks/Atkin NUCOMP specialization; see Cohen,
*A Course in Computational Algebraic Number Theory*, §5.4 **[CITED literature]**),
ported statement-for-statement. Its parts:

**Lehmer GCD loops** (`lehmer_gcdinv_sw`, `xgcd_partial_sw`). Classical Lehmer
(Knuth, *TAOCP* Vol. 2 §4.5.2 **[CITED literature]**): each outer iteration extracts
the leading word of the operands, runs a word-sized Euclidean inner loop (O(W)-bounded
iterations of single-word divisions — now on the hardware divider, §3.6), and applies
the accumulated 2×2 matrix to the full-width operands. Each outer iteration removes
Θ(W) bits, so there are Θ(n/W) outer iterations; each matrix application is 4 fused
`linear2` passes of O(k) word-ops each. Total: **Θ(n²/W²) = Θ(k²) word-ops** — for
n = 1024, ~Θ(16²·4) ≈ low-thousands of word-multiplies, matching the measured ~20 µs
Lehmer share at ~5 ns/word-multiply-add chain step **[DERIVED]**. The partial variant
stops at |D|^(1/4) (half the bits), costing ~¼ of the full loop — visible in the
measured 54.6% vs 23.0% split (§2).

**Composition multiplications** (`Sw::mul`): schoolbook, **Θ(k²) word-ops** per product
(Knuth Vol. 2 §4.3.1 M(n) **[CITED literature]**); ~10 products at mixed widths ≈ 13.5%
measured. Subquadratic multiplication (Karatsuba at ~20+ words, FFT far above) does not
engage at k ≤ 34 — this is precisely why GMP lost (§3.6).

**Division** (`divrem_mag`): Knuth Algorithm D **[CITED — the routine cites Vol. 2
§4.3.1 in-tree]**, Θ(k²) worst case, with a single-word fast path.

**Per-squaring total: Θ(n²/W²) word-ops with a small constant**; measured constants
24.5 µs (Xeon) / 8.68 µs (M-series) at n = 1024 **[CITED]**. No asymptotic
improvement is available at this operand size — every remaining factor is constant-level
(the §3.6 structural redesign), which is why the campaign treats squaring as having a
hardware-determined floor t_op.

### 4.2 Wesolowski proof verification (`vdf/src/proof.rs`)

An n-Wesolowski proof of T iterations at recursion depth d is d chained segments plus a
base proof (Wesolowski, *Efficient verifiable delay functions*, 2018
**[CITED literature]**). Per segment (`verify_segment`, proof.rs:112):
compute r = 2^{iters} mod b (cheap), then **two independent ~264-bit form
exponentiations** (`B_BITS = 264`, form.rs:9 **[CITED]**) — run in parallel in-tree —
plus one composition and one `get_b` hash. Each exponentiation is 4-bit fixed-window
(form.rs:616): 264 squarings + ≤66 window multiplies + 14 table entries ≈ **~344 group
ops [DERIVED]**. On the serial (drain) path the pair is evaluated as ONE fused
Straus/Shamir chain sharing the squarings (§3.13): a segment costs **~411 group ops**
there; on the latency path the two chains run on two threads (~700 group ops total,
~350 on the critical path), and a typical block proof measures **~2,000 group-ops
per verification [CITED**, `vdf/tests/square_bench.rs` comment and measurement —
pre-fusion accounting].

Verification is **O(d · log b)** group operations — *independent of T*, the count of
prover iterations. This asymmetry (prover does T sequential squarings; verifier does
~10³ per proof) is the entire reason a full node can validate years of timelord work
in hours.

### 4.3 Per-block VDF verification load

Header validation checks the challenge-chain, reward-chain, and (in deficit windows)
infused-challenge-chain VDFs at signage point and infusion point, plus 3 per finished
sub-slot (block_header_validation.rs, ~13 `validate_vdf` sites **[CITED]**). Averaged
over the live corpus: `window.vdf` ≈ 2.66 s WALL time per 32-block window across ~10
cores at 26 µs/op ⇒ **~32,000 group-ops/block ≈ 16 proof-verifies per block [DERIVED]**
in VDF-dense eras — consistent with the ~13 validate_vdf sites plus sub-slot proofs.
(An earlier revision divided wall time by t_op without multiplying by cores — a 10×
understatement caught when the floor table disagreed with measured node rates.) Density
swings hard by era: the light era-c stretch replays at 443 blocks/s on 12 cores ⇒ ~1,000
ops/block there **[DERIVED]** — a ~30× spread the floor model must carry as a range.

### 4.4 CLVM execution (`core/src/clvm/runtime.rs`)

Explicit-stack machine (no recursion), bump-arena nodes: **O(cost)** — Chia's cost
model charges per op and per byte, so execution time is linear in charged cost by
construction, bounded per block by the 11×10⁹ cost ceiling. Measured mainnet tx blocks
run 10⁸–4×10⁸ cost units in 10–60 ms **[CITED]** (~5 cost-units/ns). BLS aggregate
verification adds one pairing-heavy check per tx block (9–17 pairs measured typical
**[CITED]**), O(pairs) with a large constant.

### 4.5 Pipeline memory and speedup

The chaser holds O(W·id + P·block) memory — W pending identifiers (~60 B each) and at
most P in-flight downloaded blocks; the *store* is the reorder buffer
(`node/src/sync/mod.rs` doc + `SyncMetrics` peak gauges prove the bound live
**[CITED]**; RSS flat at ~540 MB over 650k+ blocks). Wall-clock per window:

  T_window ≈ max(VDF_ops/C, longest_single_proof) + max(Σbody/C, longest_body) + Σ serial residue

with the serial residue = in-order stage/derive/persist ≈ 5–10 ms/block on the Xeon
node **[CITED span data]**. That residue is the Amdahl term §5 must respect.

---

## 5. The theoretical minimum genesis-sync time

**Chain shape [ASSUMPTION unless noted]:** N ≈ 9.15×10⁶ blocks (mid-2026 tip
**[CITED]**); ~45% transaction blocks; average VDF load 3,200 group-ops/block
(**[DERIVED]** §4.3 — the dominant, era-averaged number); average tx-block body
(CLVM + BLS) ≈ 30 ms Xeon-serial **[CITED, window.body per-block range]**.

### 5.1 The compute floor (full validation)

> **Correction (this revision):** an earlier version of this section used 3,200
> ops/block, dividing the window drain's WALL time by t_op without multiplying by the
> cores doing the draining — a 10× understatement caught because the resulting table
> could not be reconciled with measured node rates. The numbers below use the corrected
> density and cross-check against live measurements.

Per-block VDF density is a RANGE (§4.3): ~32,000 group-ops/block in VDF-dense eras,
~1,000 in the lightest stretches. The conservative (dense-era) total:
N × 32,000 ≈ **2.9×10¹¹ group operations [DERIVED]**, i.e. at t_op the per-block core-cost
is 32,000·t_op:

  T_vdf ≈ N · ops_block · t_op / C  (near-perfect parallelism holds: §3.2 pools ~1M
  independent ops per window, far above any C we consider)

Dense-era-density table (upper bound; the blended chain average is lower):

| t_op | C=8 | C=16 | C=32 | C=64 |
|---|---|---|---|---|
| 24.5 µs (Ivy Bridge Xeon E5-2690 v2, today) | 10.4 d | 5.2 d | 2.6 d | 1.3 d |
| **7.97 µs [CITED — measured on our EPYC 9015 (Zen 5), portable kernel, no asm]** | 3.4 d | 1.7 d | 20 h | 10 h |

Cross-checks against reality **[CITED]**: era-a replays at 14.78 blocks/s on a ~12-thread
Ivy builder = 0.81 core-s/block — almost exactly 32,000 × 24.5 µs = 0.78 core-s/block;
the live ~6 blocks/s early-era rate on ~10 threads matches likewise. The model and the
measurements now agree.

The Zen 5 measurement *retires the hand-assembly assumption*: modern commodity cores at
7.97 µs beat the 10 µs asm-parity postulate running the shaped portable Rust. The
2013-era fleet's 24.5 µs is a silicon artifact, not a software one.

The Pi profile now has a measured anchor: **t_op = 39.3 µs on the real Pi-4
Cortex-A72 [CITED** — dedicated machine, node service stopped, §6.12 run]. At 4 cores,
dense-era density ⇒ N × 32,000 × 39.3 µs ÷ 4 ≈ **33 days** full validation
**[DERIVED]** (blended chain average lower) — the quantified case for why the Pi
follows a validated chain (weight-proof + tip-follow or `--sync-from` segment) rather
than genesis-syncing solo.

Body work: 0.45·N × 30 ms ≈ 1.24×10⁵ core-seconds ⇒ adds hours, not days, at any C
**[DERIVED]**; second-order next to the VDFs.

### 5.2 The serial residue

In-order confirm work that no parallel disposition removes: stage bookkeeping, record
derivation chained on the previous record, and the store commit — **~2 ms/block** best
measured **[CITED]**, ⇒ T_serial ≥ N × 2 ms ≈ 5.1 h. With the corrected VDF density this
is *not* the near-term binding constraint on our hardware (compute is), but it becomes
binding the moment a machine's parallel throughput approaches ~100 blocks/s — i.e. on
many-core modern boxes and in light eras (era-c's 443 blocks/s replay already runs into
per-block overheads). Window-batched confirms (one store batch per 32 blocks) push it
toward ~0.5 ms/block **[ASSUMPTION]** and are the standing engineering item.

*2026-08-18 update:* the dust-era measurement overturned the "~2 ms/block residue"
figure for the Postgres node — confirm had grown to **595 ms/block** (95% of the window)
at height ~1.69M, and the decomposed cause was WAL full-page-image amplification under
forced checkpoints, not per-statement overheads. See
[`hardware-reports/confirm-wal-stall-epyc.md`](hardware-reports/confirm-wal-stall-epyc.md)
for the decomposition, the live `wal_compression=zstd` A/B (3.7 → 6.7 blk/s), the
catch-up coin-journal (pg) and batched sync-before-link (mmap) fixes, and the fleet
config recommendations.

### 5.3 Best-disposition single node

  T_single ≈ max(T_serial, T_vdf + T_body)

Dense-density upper bounds: a 32-thread Ivy node ≈ **2.6–3 days**; the EPYC 9015 alone
(8 cores) ≈ **3.5 days**; one modern 32-core (Zen 5-class) machine ≈ **~1 day**
**[DERIVED]**. The blended chain average (light eras included) lands each of these
somewhat lower. Against the current live two-node ~1 week: the remaining headroom is
mostly (a) modern silicon (3× per core), (b) simply more cores per node than our
10-thread pods, (c) confirm batching for the light-era regime.

### 5.4 K-node segmentation

`--sync-from` (§3.9) makes the chain K-way separable with only the weight proof
(~1.5 min **[CITED]**) duplicated:

  T_K ≈ T_single / K + minutes

On hardware we own today, EPYC + one 40-thread r720 split ≈ **~1.5 days** full
validation; eight modern 32-core nodes ≈ **~3–4 hours** **[DERIVED]**. The scheme's
only coupling is the ref-seeding fetch at each anchor.

### 5.5 Weaker dispositions (for scale)

- **Assume-valid** (signatures + CLVM skipped below a reviewed milestone; VDFs still
  verified — our `assume_valid` seam): removes T_body only; VDF compute dominates, so
  the §5.1 table effectively stands **[DERIVED]**.
- **Weight-proof trust + download only** (no historical validation — what light-sync
  does): bounded by network and store bandwidth. Chain bodies ≈ 120 GB
  **[ASSUMPTION]**; at 1 Gbps ≈ 16 min transfer + store ingest at ~100 MB/s ≈ 20 min ⇒
  **~40 minutes**, hardware-bound **[DERIVED]**.

### 5.6 What dominates the error bars

(1) ops/block varies ±3× by era (sub-slot density — §4.3); the 3,200 average is
corpus-weighted, not uniform. (2) t_op is per-microarchitecture; our own two machines
differ 2.8×. (3) The 2 ms residue is *our stack's* number — a different store discipline
moves it either way. The model's shape — `max(serial residue, VDF/C)` with K-way
division — is robust; the absolute hours carry ±2× bars.

---

## 6. Dead ends — the negative-results ledger

Each entry records what was tried, the *mechanism* of failure (not just the number), and
the conditions under which it would be worth revisiting. An idea on this list must not be
retried without new evidence that its stated failure mechanism no longer applies.

### 6.1 GMP (`rug`/libgmp) for NUDUPL form arithmetic — **OUT**

*Tried twice, escalating:* (1) a naive port of `square_with` onto `rug::Integer`
(`vdf/src/gmp_form.rs::GForm::square_with`) — byte-correct against the fixed-limb path
over a 256-squaring walk, but **0.66×** (39.6 µs vs 26.0 µs). Hypothesis at the time:
allocation-bound, so (2) a full scratch-buffer rewrite — 18 preallocated
`Integer::with_capacity(2048)` buffers, in-place `extended_gcd_mut`, floor-division
assigns, zero steady-state allocation (`GScratch::square_with_scratch`). Still
byte-correct, and **worse: 0.44×** (59.5 µs).

*Failure mechanism:* GMP's advantage is asymptotic — subquadratic multiplication
engages at hundreds of words. Our operands are 16–34 words, where GMP pays for
generality (limb-count branching, sign normalization, function-call overhead per op)
that a fixed-width, fully-inlined kernel simply doesn't have. The in-place API also
forces extra swaps/copies (`extended_gcd_mut` returns the cofactor in `other`) that the
scratch rewrite could not eliminate — which is why "remove the allocations" made it
*slower*, not faster: the allocations were never the cost.

*Revisit only if:* the discriminant size grows ≥4× (e.g. a 4096-bit fork parameter),
where subquadratic multiplication begins to matter. At 1024 bits, do not retry.
Note the scope: GMP remains **in use and winning** for primality checking in
discriminant generation (§1 step 3) — the verdict is specific to form arithmetic.

*Evidence:* `vdf/src/gmp_form.rs` (both variants kept in-tree as the record),
`vdf/tests/square_bench.rs::{gmp_vs_limbs, gmp_scratch_vs_limbs}` **[CITED]**.

### 6.2 BMI2/ADX intrinsics (`_mulx_u64` + `_addcarry_u64`/`_subborrow_u64`) for `linear2` — **OUT**

*Tried:* a `#[target_feature(enable = "bmi2,adx")]` kernel with runtime detection,
mirroring the portable carry-chain structure with explicit mulx/adc/sbb intrinsics.
Differentially tested byte-identical against the portable kernel across a full
sign × size × coefficient sweep *before* benchmarking (the consensus-code discipline).

*Result and an honesty correction:* the A/B on the builder read **24.97 vs 24.55 µs —
equal**. But the builder fleet is E5-2690 v2 (Ivy Bridge, 2013), which has **neither
BMI2 nor ADX** — the runtime detection gate silently routed the "intrinsics" run onto
the portable kernel, so that measurement compared portable against itself
**[CITED, /proc/cpuinfo]**. The intrinsics kernel has never been measured on silicon
that can execute it.

*Standing verdict, restated on the honest basis:* dropped for **fleet irrelevance** —
the code cannot run on any r620/r720 node — plus the `unsafe`/detection-branch cost in
consensus code. The mechanism claim ("LLVM already emits the carry chains") is verified
only in the sense that the portable kernel's Xeon number equals the shaped-Rust
expectation; it is *not* verified against real mulx/adcx execution.

*Revisit when:* benchmarking on ADX-capable hardware (the `epyc-lower` node — EPYC 9015,
Zen 5, BMI2+ADX present) if its *portable* t_op suggests headroom; verify with
disassembly that LLVM missed a dual-carry (adcx/adox) structure before re-adding
`unsafe`. The variant was never committed; it must be rewritten from §3.6's structure
if retried.

### 6.3 i128 signed-carry fused `linear2` — **OUT on x86** (and superseded everywhere)

*Tried:* the first fusion attempt — one pass accumulating both word products into a
signed radix-2⁶⁴ digit stream with an `i128` arithmetic-shift carry.

*Result:* **13% faster on Apple Silicon** (9.96 → 8.68 µs) but **10% slower on the
Xeon** (25.8 → 28.4 µs) — an architecture-divergent result that would have shipped a
regression to the actual fleet if only the laptop had been measured.

*Failure mechanism:* every limb's result depends on the previous carry through i128
add + arithmetic shift — a long serialized dependency chain. The Xeon's out-of-order
core pipelines the *old three-pass* code (short independent chains per pass) better
than the "optimized" single fused chain. Apple's wider ALUs hid it. The u64
carry-chain shape (§3.6) keeps the fusion but restores short hardware-carry chains,
winning on both machines.

*Lesson encoded:* never accept a hot-loop A/B from one microarchitecture; the builder
Xeon is the fleet-representative gate.

### 6.4 One-bit-narrower extraction alone (hardware i64 division) — **neutral on Xeon, kept for ARM**

*Tried in isolation:* replacing the Lehmer inner loop's `i128 / i128` (`__divti3`
libcall) with hardware 64-bit division, alone. **Xeon: 25.8 → 26.1 µs — neutral.**

*Mechanism:* the Xeon's inner word-loop cost is dominated by the multiply/compare
chain, not the divide; and one fewer extraction bit means marginally more outer
iterations, offsetting the cheaper divides. Kept anyway because the A72/M-series pay
far more for `__divti3` (M-series composite improved) and the Pi is the profile that
cares **[CITED both A/Bs]**. Recorded so nobody "re-optimizes" the divide on x86
expecting a win there.

### 6.5 Keying the CLVM flag ladder on the previous transaction block — **WRONG (consensus)**

*Held for weeks as established fact* (in code comments, no less): "chia keys the flag
ladder off the PREVIOUS TRANSACTION block's height." Every corpus below the hard fork
agreed, because below any activation boundary both keyings produce identical flags —
the error was *unobservable* until the first tx block past the fork (5,496,002), which
rejected with `InvalidBlockCost` (DIVERGENCE-23).

*Mechanism:* a plausible reading of chia's call sites that no test could distinguish
without a boundary-straddling corpus. The era-c corpus was built precisely to hold the
hard-fork straddle, and it caught it.

*Lesson encoded:* consensus rules that only differ at activation boundaries need a
corpus containing each boundary. Do not trust prev-tx keying anywhere in the ladder;
the block's own height is oracle-verified (`run_block_generator2` cost equality)
**[CITED]**.

### 6.6 Enforcing the 1024-announcement cap in block validation — **WRONG (consensus)**

Chia's `TOO_MANY_ANNOUNCEMENTS` is a *mempool* rule; mainnet carries announcement-heavy
blocks (4,693,324: an 830-spend dust sweep). Enforcing it in consensus rejects real
history (DIVERGENCE-21). The cap lives in spend-bundle (mempool) validation only.
*Lesson:* chia_rs flags that exist "in the same file" as consensus checks are not all
consensus checks — classify each rule by the mode flag that gates it **[CITED]**.

### 6.7 Owned `Arc` atoms in the bumpalo arena — **LEAK (correctness/perf)**

`bumpalo` runs **no destructors**. Storing `AtomBuf::Owned(Arc<Vec<u8>>)` in arena
nodes leaked every atom's backing buffer — ~90 MB per tx block, OOM-cycling the live
node (an earlier campaign item, §3.7). *Lesson encoded:* nothing owning heap memory may be moved into the
arena; copy bytes in (`alloc_slice_copy`) and drop the owner at the boundary. The fix
was also +22% CLVM throughput — destructor-free arenas only deliver their promise when
the contents are actually POD **[CITED]**.

### 6.8 `RefCell` as the window VDF sink — **NOT SEND**

The first window-pipeline sink was `RefCell<Vec<QueuedVdf>>`; it compiled in the
node-only gate and broke the full-node build (the staged future must be `Send`).
Now `std::sync::Mutex` (`node/src/header.rs:61`). *Lesson:* the gate for node-crate
changes must include the full-node clippy/build — it does now **[CITED]**.

### 6.9 Single 96-block `RequestBlocks` for the sync-from anchor — **PEERS REJECT**

`anchor_at` originally fetched its H−64..H+31 span in one request. Chia peers reject
`RequestBlocks` spans wider than 32: every peer answered `RejectBlocks`, and the anchor
loop spun silently at `Ok(false)` (made worse by having no log on the failure path —
also fixed). Chunk to ≤32 and log the miss **[CITED, 20a4f04]**.

### 6.10 Store-engine choices decided before this campaign (recorded to prevent relitigating)

- **redb** as the embedded store: rejected in the 031 direction decision — the store
  contract is query-shaped SQL traits with first-class SQLite + Postgres via sqlx, and
  the Pi profile is served by the purpose-built mmap backend, not a third KV engine.
- **A second tag on a freshly pushed Harbor digest**: races the vulnerability-scan gate
  and 412s — one tag per push (encoded in `.github/workflows/build.yml` comments).

### 6.11 Operational dead ends (cost real hours; do not rediscover)

These are infrastructure, not code, but each burned enough time to earn a line:

- **`kubectl exec`/`cp` streams above ~8 MB truncate silently** on the VPN path — a
  46 MB transfer delivered 46 MB minus a tail with a clean EOF. An MSS clamp (1380) on
  the firewall was applied and verified and did *not* cure it — it is intermittent link
  loss, not PMTU. Standing rule: split ≥8 MB transfers into chunks with per-chunk
  md5 verify + retry; for pod↔pod, move data inside the cluster, never through the
  laptop.
- **Heredoc into `kubectl exec` without `-i`** writes a zero-byte file (`cat` sees
  immediate EOF) and every downstream "run" of that empty script *succeeds instantly* —
  three long gates once "completed" in four minutes this way. Always `kubectl exec -i`
  when piping stdin; verify with `wc -c` after writing.
- **`pkill -f <script name>` matches the kubectl-exec wrapper carrying the same string
  in its cmdline** — it killed the very gate it was meant to relaunch, twice.
  Kill by exact pid or a bracketed pattern (`[d]iv23gate`), never by script-name
  substring from inside a `kubectl exec` whose own command line contains it.
- **Helm chart version in pod-template labels** (`helm.sh/chart: …+<repo-sha>`) rolled
  the entire builder pool on *every* push to the infra repo, killing in-flight builds.
  Pod templates get stable selector labels only.
- **`cargo clippy --workspace` on headless builders** fails in `gdk-sys` (GUI crate
  needs system GTK) — always package-scoped clippy in gates, and never let a gate's
  grep filter swallow `error:` lines silently (two "empty" gate stanzas were compile
  failures wearing green).

### 6.12 In-place two's-complement Lehmer state (the §3.6 "structural redesign") — **OUT (measured on the real Pi)**

*Tried:* the representation redesign §3.6 had deferred to real-Pi hardware — the full
chiavdf-style structural rework of both `_sw` Lehmer loop cores (`lehmer_gcdinv_sw`,
`xgcd_partial_sw`, 77.6% of squaring, §2). The iteration state `[r2, r1, cofactor2,
cofactor1]` moved into fully sign-extended two's-complement `[u64; 20]` arrays inside a
ping-pong arena: each 2×2 matrix application wrote into the opposite half (a role swap —
**zero per-iteration copies**), the complement carried every sign (**no flags**; the fill
word `d[N−1]` is the sign), the fused `x·w1 + y·w2` kernel ran one straight-line pass
with **no complement pass, no magnitude compare, no operand-sign logic** (coefficient
signs select addmul-2 vs submul-2 streams; two's-complement truncation is the signed
result). Word extraction, the `bits` computation, and the i128 inner quotient loop were
kept byte-identical, so the quotient sequence could not diverge. Three formulation
rounds: (i) branch-free masked-subtract corrections — ~9% slower on Xeon (two extra
serial ALU ops per limb); (ii) coefficient-sign stream split restoring the reference
per-limb op count — Xeon parity; (iii) multiply-free tail drain (a fill limb's product
is the per-call constant `u·2^64 − u`) — no measurable change.

*Correctness (all gates green before any benchmark was read):* differentially
property-tested byte-identical against the retained sign-magnitude kernels — 1,680
`linear2` cases (vs BigInt AND the old kernel, full sign × size × coefficient sweep),
4,080 gcdinv pairs, 1,632 partial-xgcd cases across sizes and limits, a 300-step
real-1024-bit-discriminant walk, the FLINT/chiavdf vector suites, full `dg_xch_vdf`
(release + debug, debug asserts exercising the fill/used invariants) and
`dg_xch_weight_proof` suites, clippy `-D warnings` clean.

*Result — the redesign loses on both microarchitectures **[CITED]**:*

| Host | sign-magnitude (shipped) | two's-complement redesign | delta |
|---|---|---|---|
| Xeon E5-2690 v2 (builder, min of 14+ interleaved runs) | **22.98 µs/op** | 23.90 µs/op | **+4%** |
| Pi-4 Cortex-A72 @ 1.8 GHz (dedicated, node service stopped, spread 39.26–39.35 vs 41.57–42.57 over 10 interleaved runs) | **39.28 µs/op** | 41.6 µs/op | **+6%** |

The A72 spread was tight enough (±0.1%) to make the 6% unambiguous; the
"A72 gains disproportionately" hypothesis is refuted.

*Failure mechanism:* the design's ceiling is "reference kernel minus copies plus
invariant upkeep" — the inner multiply/carry loops are shape-identical, so the only
lever was the bookkeeping trade. Measurement says the trade is net-negative: LLVM
already compiles the `Copy`-struct dance into essentially the stores the arena needs
(the copies were never the cost, echoing §6.1's lesson), while the two's-complement
invariants charge real per-iteration work the sign-magnitude representation avoids —
full-width (N=20) sign-fill maintenance, used-limb rescans, full-width negation in the
sign fixups where a flag flip is free, and full-width compares where a length compare
is O(1). Sign-magnitude-with-length is not bookkeeping overhead at these operand sizes;
it is length-adaptive work *avoidance* — the operands shrink every iteration and the
`len` field is what lets every op track that shrinkage.

*Revisit only if:* the kernel moves to hand-written asm where register-resident state
changes the copy economics (ruled out separately, §6.2 — fleet has no ADX and portable
already matches asm shape), or the discriminant grows enough that O(N)-vs-O(len) no
longer dominates the comparison. At 1024 bits on this fleet and on the Pi profile, do
not retry. The experiment (kernel + differential suites) lives unmerged on the
`vdf-tc-lehmer` branch record; the shipped tree is unchanged.

---

## 7. Reproduction

- Squaring bench: `cargo test --release -p dg_xch_vdf --test square_bench -- --ignored --nocapture`
- Phase profile: `cargo test --release -p dg_xch_vdf --features phase-profile --test phase_profile -- --ignored --nocapture`
- Corpus replays: `DGXCH_ERA_CORPUS=<era dir> cargo test --release -p dg_xch_node --test era_replay -- --ignored --nocapture`
  (corpora are built from a chia node's SQLite DB by `full-node/src/bin/corpus_import.rs`)
- Live spans: the node logs `window.vdf` / `window.body` / `block.stage` /
  `record.derive` / `store.persist` span closes at INFO.

*Literature:* Knuth, TAOCP Vol. 2 (3rd ed.), §4.3.1 (classical multiplication,
Algorithm D), §4.5.2 (Euclid/Lehmer GCD); Cohen, *A Course in Computational Algebraic
Number Theory*, §5.4 (composition/NUCOMP); Wesolowski, "Efficient verifiable delay
functions", EUROCRYPT 2019; the chiavdf reference implementation (Chia Network).
