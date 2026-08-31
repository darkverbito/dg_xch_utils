# Algorithmic finality: where each hot path stands, and how we know

This document records, durably, which of this node's hot paths have reached their floor —
"finality" — and the evidence that pins each verdict. Its reason to exist is a specific,
expensive lesson learned twice in opposite directions: **whether a reference library can be
beaten is a property of the operand size and call pattern, not of the library**, and the only
way to know is to build the alternative, prove it verdict-identical, and measure it on the
real silicon. Every claim below carries its numbers, its hardware, and the commit that holds
the code, so no future campaign re-derives or re-forgets it.

Method notes that apply throughout:

- A **finality verdict** requires: (1) a complete alternative implementation, (2) a
  differential gate proving it behaviorally identical to what it would replace, (3) wall-clock
  measurement on an idle machine, three reps, on every deployment architecture. Estimates,
  extrapolations, and profiles alone have both failed us; only the triple has held.
- **Negative results are kept, not deleted.** A verdict-identical alternative that lost on
  speed stays in-tree as dormant verification infrastructure — an independent oracle for any
  future change to the path it shadows. `gmp_form.rs` and the native Baillie–PSW
  (`discriminant.rs::is_probable_prime_native` + `mont.rs`) are both of this kind.
- **Decision trees are stated before the measurement runs**, so the outcome cannot bend the
  criteria. The BPSW campaign's tree was written down before the A72 bench executed; the
  negative branch was taken exactly as pre-stated.

---

## 1. The central finding: the GMP crossover

One library, two opposite finality verdicts, and the boundary between them is the finding.

### 1a. Class-group form arithmetic: GMP loses (measured 2026-08, "GMP forms verdict")

At NUDUPL operand sizes (~1024-bit intermediates, but issued as many *small, individual*
mpz calls), the fixed-limb `Sw` representation beats a preallocated-scratch rug/GMP port:

| implementation | per squaring op (Xeon) |
| --- | --- |
| fixed-limb `Sw` (portable Rust) | **26.1 µs** |
| rug/GMP with preallocated scratch | 59.5 µs (0.44×) |

Byte-correct by a 256-squaring differential; the loser is preserved in `gmp_form.rs` with a
`square_bench` A/B. Cause: **per-call overhead** — sign/size normalization, limb bookkeeping,
and dispatch inside mpz dominate when each call does little work. The production form path is
fixed-limb and this verdict is final for that call shape.

### 1b. Primality testing: GMP wins (measured 2026-08-31, this campaign)

At powm size — one `mpz_powm` of a 264-bit exponent is ~400 *sequential, internal* limb
operations — the same overhead amortizes to nothing and GMP's hand-written `addmul_1`
assembly is the entire cost. A native Baillie–PSW with a fixed-limb CIOS Montgomery ladder
(no allocation, no wide division, u128 accumulation — the same style that won in 1a) lost
decisively on **both** deployment architectures:

| 264-bit prime search (the `get_b` shape, 200 distinct seeds/op) | Xeon | Pi-4 A72 @1.8 GHz |
| --- | --- | --- |
| reference: chunked screen + `mpz_probab_prime_p(24)` | **557 µs** | **1.302 ms** |
| native: screen + fixed-limb MR-2 + bigint Lucas | 1,329 µs (2.4× slower) | 3.398 ms (2.6× slower) |

Three reps per cell, idle machines (the Pi's node service stopped for the bench), variance
under 0.1%. The native path is verdict-identical to the reference — see §2 — and still lost.
This verdict is final for the powm call shape: **the 22% of Pi-4 cycles spent in GMP during
sync is the floor cost of the consensus primality test, executed by the best implementation
available on either architecture.**

### 1c. The A72 kernel verdict: the compiler was already optimal (measured 2026-08-31)

The third finality verdict of this arc, closing the last software lever. Hand-written aarch64
kernels (`mul`/`umulh`/`adcs` chains, the loop entirely in asm) for the three limb-row shapes
under the Lehmer walk — fused addmul_2, fused submul_2, and the schoolbook addmul_1 —
byte-identical to the portable loops by an in-binary differential, measured on the idle Pi-4:

| addmul_2 row width | kernel / portable |
| --- | --- |
| n = 4 | 0.92× |
| n = 8 | 1.00× |
| n = 16 | 0.99× |
| n = 30 | 0.98× |

End-to-end t_op: 40.0 µs/op — unchanged from the 39.3–40.2 portable baseline. **LLVM already
emits optimal carry chains for these loops on the A72.** The 39-vs-26 µs A72/Xeon gap that
motivated the lever is therefore silicon, not scheduling: the A72's 64-bit multiplier is not
fully pipelined against `umulh`, and no instruction ordering recovers throughput the execution
unit does not have. Production stays on the portable loops; the assembly lives on in
`limbs_a72.rs` behind its differential (`kernels_match_portable_rows`) and its quantifying
bench (`kernel_bench_rows`), re-runnable on any future ARM core in seconds.

### 1d. The boundary, stated so it can be reused

> GMP is beatable where the work per call is small enough that call overhead dominates
> (form arithmetic: dozens of ops per call). GMP is unbeatable-by-portable-code where the
> work per call is large enough to amortize entry costs into its assembly kernels (powm:
> hundreds of ops per call). The crossover sits between those shapes; measure, never assume,
> anywhere near it.

The wrong prediction that cost this campaign a day: extrapolating 1a's "mpz overhead
dominates at these operand sizes" to the primality path. The operand size was similar; the
**call pattern** was not. That distinction is the durable lesson.

---

## 2. The verification asset the negative result left behind

The native Baillie–PSW is kept dormant because it is the only independent implementation of
the consensus primality decision we have, and it is pinned to the reference by layered
differentials (`vdf/tests/bpsw_differential.rs`):

- **Layer 1 — semantic**: native BPSW (screen → strong MR base 2 → perfect-square rejection
  → strong Lucas, Selfridge Method A) against `mpz_probab_prime_p(24)`:
  every integer below 100,000; the strong-pseudoprime-base-2, Lucas-pseudoprime, and
  Carmichael families; perfect squares and powers; the 19-digit strong pseudoprime to the
  first nine prime bases; 264- and 1024-bit candidate-shaped random inputs with the search's
  bitmask; near-2^k edges. Scale knob `BPSW_DIFFERENTIAL_SCALE`; the deep soak ran ~400,000
  candidate-shaped inputs with zero divergence in 74 s.
- **Layer 2 — representational**: the fixed-limb Montgomery MR-2 against the bigint MR-2 it
  substitutes on the hot width, across every limb-count shape 64→320 bits including the exact
  fit boundaries, plus all small odds exhaustively. The layered soak totaled ~2.6 M verdicts,
  zero divergence.
- The harness went red once during development — an over-cautious fit guard returned `None`
  for n=3 — and caught it before the code could matter. That is the gate working, and the
  reason the layered structure (each layer gated against the one below) is the required
  pattern for any future touch of this path.

Why this matters even as a negative: the primality verdict **selects the consensus prime**.
A single divergence walks `hash_prime` to a different prime and forks the node. Any future
change near this path — a GMP upgrade, a new backend, an arch-specific build — can be
validated against this oracle in minutes instead of re-deriving trust.

---

## 3. The finality ledger

Every hot path in the sync profile, its share on the Pi-4 (the constrained target), and where
it stands. Profile source: 151,334-sample perf capture on the live, compute-saturated Pi-4.

| path | Pi-4 share | verdict | evidence |
| --- | --- | --- | --- |
| GMP primality (`hash_prime`/`get_b`) | ~22% | **FINAL — at floor** | §1b: verified alternative lost 2.4×/2.6× on both targets |
| Class-group form arithmetic (`Sw` NUDUPL/Lehmer) | ~43% | **FINAL — at floor.** Algorithm at 1.02× its work floor AND code at the compiler's floor: hand-written A72 kernels measured 0.92–1.00× of the portable loops (§1c); the A72/x86 gap is the silicon | drain A/B ledger; perf capture; kernel bench |
| VDF drain scheduling | (inside above) | **FINAL** — dedup + LPT work-stealing sits 1–4% above its measured work floor on every shape; "the next levers are work reduction, not scheduling" | era-corpus A/B, three hardware shapes |
| BLS (blst Montgomery kernels) | ~5% | **FINAL** — already hand-written assembly | vendor asm |
| sha256 | ~1.5% | **FINAL** — ring/BoringSSL vectorized block fn, chosen over sha2 by measurement | PR53 A/B |
| CLVM body | ~2–6% | **FINAL for sync purposes** — arena runtime beat clvm_rs on peak (121.6 vs 139.0 MiB) with bit-identical outcomes; body is not the wall on any target | allocator investigation doc |
| memcpy / allocator churn | ~5% | open, small — trims worth ~+3–4% | perf capture |
| u128 wide division | was 3% | **CLOSED** — per-divisor Möller–Granlund reciprocal landed; the reciprocal itself is the one wide divide left, amortized | PR55, divrem differential |
| prime-search screen | (in 22%) | **CLOSED** — compile-time normalized reciprocals per prime chunk; multiplies only, no allocation, on any target | PR55, screen parity test |

### Consequences, in blocks per minute (Pi-4, ~6.0 M era)

- Measured saturated: **288–322 blk/min** (era-dependent; the higher figure arrived with the
  LAN peer holding the queue full).
- Both candidate levers to 400 are now measured dead: the primality lever (§1b, GMP already
  optimal) and the kernel lever (§1c, the compiler already optimal). Remaining trims
  (memcpy/alloc churn, ~3–5%) do not change the category. **The Pi-4 at ~300 blk/min is at
  its silicon floor.** The categorical jump is hardware: the Pi 5's A76 at 2.5–3× on
  published bignum benchmarks → ~700–850 blk/min, genesis in 7–9 days, no assembly required.
- The finality claims themselves are executable: `finality_bench.rs` re-measures §1b and
  `kernel_bench_rows` re-measures §1c on whatever machine runs them, both contenders in one
  binary — a verdict here is falsified the day a probe disagrees on a deployment target.

---

## 4. Process rules this campaign validated (and the failures that forged them)

1. **Differential-first, always.** Every gate in this campaign that existed before its
   implementation caught a real bug before it could ship: the divrem gate (adversarial Knuth-D
   shapes), the screen parity gate, the BPSW fit-guard red. Every consensus divergence that
   *did* ship this month (the generator-mode mis-keying, the checkpoint over-rewind) shipped
   through a green suite that lacked the specific differential.
2. **Pre-state the decision tree.** The BPSW A72 bench had its accept/reject branches written
   down before it ran. When the number came back negative there was no temptation to move the
   goalposts — the revert was mechanical and immediate.
3. **Bench on every deployment target before believing a win.** The x86 bench alone had
   already shown the native path losing; the A72 bench was run anyway because the *hypothesis*
   was A72-specific. Both numbers are in §1b. Had only x86 been measured, the wrong
   conclusion ("portable loses on servers but should win on ARM") would have survived.
4. **Idle-machine, three-rep discipline.** The same bench that reads ±20% on a busy builder
   reads ±0.1% with the node stopped. No number from a contended machine enters this
   document.
5. **Negative results get commits, not deletions.** `gmp_form.rs` (2026-08, forms) and
   `mont.rs` + the BPSW suite (2026-08-31, primality) are both dormant-but-gated. The cost of
   keeping them is near zero; the cost of re-deriving either verdict is a day each, and the
   cost of *mis-remembering* one — as this campaign did with 1a — is a day plus the risk that
   the wrong extrapolation ships.

---

## 5. Where the code lives

- Native BPSW semantic layer: `vdf/src/discriminant.rs` (`is_probable_prime_native`,
  `miller_rabin_base2`, `strong_lucas_selfridge`, `jacobi`).
- Fixed-limb Montgomery MR-2: `vdf/src/mont.rs` (CIOS, `pow2`, `miller_rabin_base2_fixed`).
- Differential gates: `vdf/tests/bpsw_differential.rs` (both layers, scale knob).
- Test surface: `dg_xch_vdf::testing` (native, reference, and both MR-2 halves).
- The production verdict (reference + native screen): `discriminant.rs::is_probable_prime`,
  whose comment carries the measured numbers so the next reader does not need this document
  to avoid the trap — but this document exists so the *reasoning* survives too.
- Sibling records: `docs/clvm-allocator-investigation.md` (the CLVM runtime campaign,
  including its own wrong turns), `docs/OPTIMIZATIONS.md` §3.6/§3.12 (the forms verdict and
  the drain floor).
