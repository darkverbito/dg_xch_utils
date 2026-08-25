# Rung 4: cross-window confirm/stage pipeline (sqlite catch-up)

Status: SPEC — not started. Written 2026-08-24 after rung 3 shipped. Pick up any time;
every number below is live-measured and every blocker is grounded in code read at
`pr-parity-clean` @ `e92bd80`.

## Where the campaign stands

Grant's bar: **sustained 2,000+ blk/min on the sqlite leg** (dg-xch-node-pr-verify,
5 peers, iSCSI PVC). Measured ladder, steady-state samples:

| rung | change | blk/min | height at sample |
|---|---|---|---|
| baseline | 2 peers | 128 | 1.18M |
| config | 5 peers (inflight 1→16) | 128 | 1.18M — proved fetch was never the limit |
| 1 | staging COMMITS batched per window | 704 | 1.21M |
| 2 | staging READS batched per window | 1,241 | 1.30M |
| 3 | ONE writer transaction per window | 1,510 | 1.44M |

Window profile after rung 3 (32-block window, gauges `fullnode_window_*_micros`):
`stage=0.47s  vdf=0.47s  body=0.06s  confirm=0.11s` — sum 1.11s of a 1.27s wall.
The stages run **serially per window**. No remaining single-stage fix reaches the bar;
the cheap rungs are spent (rung-3 report also killed the candidate-probe skip at
~0.03s/window and deferred coin-read batching for lack of a valid fixture).

## Goal

Overlap window N's **write phase** (confirm: coins + `set_peak`, ~0.11s, plus its share
of the stage walk) with window N+1's **CPU phases** (fetch is already decoupled; stage
record-derivation + vdf drain + body precompute ≈ 0.95s). Target wall per window ≈
max(CPU phase) ≈ 0.5–0.6s → **~2,000–2,500 blk/min** at current-era density.

Non-goals: the near-tip band (per-block commits are correct at tip — farming latency
beats throughput there); the bulk/reservation path; Postgres (epyc already sustains
2,000+; nothing here may regress pg); any consensus or wire behavior.

## Why it cannot be a bolt-on (the rung-3 evidence)

The engine can already decide-then-write. The blocker is the **return contract** of
`follow_blocks_reporting` (node/src/sync/mod.rs), whose `(confirmed_peak, deltas)`
return `block_processor` (full-node/src/daemon.rs) consumes synchronously:

1. the ConfirmedPeak announcer fires on it (peers learn our new tip),
2. the BlockQueue rebase realigns `low_water` to it,
3. `finish_follow_step` pushes wallet coin-state + mempool revalidation off the deltas,
4. `follow_inflight_since` gates the driver's peak-reconcile on "no confirm running."

Naively spawning the confirm forces one of:
- announcing an **uncommitted** peak → peers `RequestBlocks` a tip the store can't
  serve; wallet pushes that vanish on crash — correctness regression;
- window-lagged returns without design → silent semantic drift in announcer, rebase,
  `/health` download-liveness, and the near-tip transition;
- an ad-hoc second decoupling layer under the BlockQueue.

Engine side: the confirm must split decide (fork-choice, coin-rule application against
the fork view) from write (batch build + commit), and the **staged overlay must outlive
the write** — window N+1's coin validation reads window N's created coins from the
overlay until N's write commits. Rung 3 also left the writer connection held across the
window's CPU drains; under pipelining the writer is held by N's in-flight write while
N+1 stages — the staging batch acquisition must not deadlock against it (rung 1 solved
this ordering *within* a window; this is the same problem *across* windows).

## Design

**Shape: a one-deep write pipeline with lagged announcement.** Depth 1 is enough — the
write phase (~0.2s incl. commit) is far shorter than the CPU phase (~0.95s); deeper
pipelines buy nothing and multiply crash states.

1. **Engine decide/write split.**
   - `confirm_staged_batch_in` splits into `decide_batch(deltas) -> DecidedWindow`
     (pure against the fork view + staged overlay; no store writes) and
     `write_window(DecidedWindow, batch) -> committed peak` (builds coin rows +
     `set_peak_in`, commits).
   - **Extension-only gate:** the pipeline path is taken only when every delta in the
     window extends the current decided tip (the catch-up common case). Any non-extending
     delta (fork/reorg landing) drains the pipeline first — the in-flight write completes,
     then the window runs the existing sequential fork-choice path unchanged. Reorgs never
     pipeline.
   - **Overlay lifecycle:** staged-overlay retirement moves from "end of confirm" to
     "write committed." N+1 stages against overlay(N) ∪ overlay(N+1). On write failure,
     both overlays clear and the queue rebases to the last durable peak (the existing
     stall-reclaim machinery is the recovery path — reuse it, do not invent another).

2. **Driver: split the return.** `follow_blocks_reporting` returns
   `{decided_peak, write_handle}`. `block_processor` keeps TWO cursors:
   - `decided_peak` drives the fetch frontier only (the producer may plan the next
     window against it) — never announced, never rebased-to;
   - `durable_peak` (resolved from `write_handle.await` at the TOP of the next
     iteration, before the next window's return is consumed) drives everything
     user-visible: ConfirmedPeak announcement, BlockQueue rebase/`low_water`,
     `finish_follow_step` wallet/mempool side effects, `/health`, metrics
     `peak_height`. Wallet/mempool consume deltas only with `durable_peak` —
     **exactly one window later than today, never earlier**. Nothing observable ever
     refers to an uncommitted block.
   - `follow_inflight_since` covers the whole span while `write_handle` is pending
     (the driver's peak-reconcile stays gated — same rule as today, wider window).

3. **Writer-connection handoff.** N's write owns the writer until commit; N+1's staging
   opens its batch lazily at first archive write (rung-2 behavior) and therefore blocks
   on the writer mutex only when N's write overruns the CPU phase — which is the
   backpressure we want (pipeline degrades to today's serial behavior, never deadlocks).
   Assert the lock order: staging batch is only acquired after the previous
   `write_handle` is either resolved or known-in-flight on the OTHER open batch —
   two batches must never interleave acquisition (single writer = single mutex; the
   lazy-open makes the order total).
4. **Near-tip and band transitions.** `near_tip()` (or the transition into it) drains
   the pipeline: resolve `write_handle`, retire overlays, then the per-block path runs
   exactly as today. Same drain on shutdown and on any stage/vdf error (the rung-3
   drop-the-carry rollback becomes drain-then-drop).

## Crash contract (the gate)

Unchanged classes, one window wider:
- A kill mid-write loses that window's coins/peak wholly (transactional) — archive rows
  from its staging may persist as inert candidates (rung-3 semantics, already proven
  harmless: candidates never confirm without `set_peak`, re-stages upsert).
- A kill while N+1 stages and N's write is in flight: N commits or not (atomic); N+1's
  uncommitted batch vanishes. Resume = re-fetch exactly the missing heights from the
  durable peak. `restart_resume`'s class A ("uncommitted batch vanishes wholly") and
  class C ("resume = exactly the missing heights") must hold verbatim — they are the
  acceptance gate, run first, before any suite.

## Tests (red-first)

1. **Pipeline overlap pin (the RED):** extend `staging_commit_granularity.rs` — drive
   two consecutive catch-up windows through `follow_blocks`; assert via the commit
   histogram + a new `fullnode_window_write_overlap_micros` gauge (or a test-side
   clock on instrumented spans) that window N+1's stage began before window N's commit
   landed. RED today (strictly serial), GREEN with the pipeline.
2. **Durable-announcement pin:** the ConfirmedPeak announcer / wallet push must carry
   only committed heights — instrument the announcer seam in-test; inject a failing
   write and assert nothing was announced for the failed window (RED if announcement
   uses `decided_peak`).
3. **Reorg drain pin:** feed an extension window then a fork window; assert the fork
   window ran on the sequential path with the pipeline drained (no overlap gauge
   movement) and the result matches today's sequential outcome byte-for-byte.
4. **Crash classes:** `restart_resume` 6/6 unchanged, plus one new kill-point between
   decide(N+1) and write(N) completion if the harness supports it.
5. Full suites: `dg_xch_node`, `dg_xch_stores`, `full-node` (known pre-existing
   `puzzle_state` failure excepted — see its own ticket).

## Rollout & measurement

Build on the cluster builder only (env incl.
`LD_LIBRARY_PATH=/opt/toolchain/rustup/toolchains/<tc>/lib` — fresh build-scripts die
without it). Image → roll dg-xch-node-pr-verify (established approve-flow) → 8-min
warmup → 150s steady sample. Adjudicators: rate vs the 2,000 bar,
`window_stage/vdf/confirm_micros` (wall should approach max(stage)), the new overlap
gauge, and `commit_catch_up` latency (watch for writer-hold regressions the rung-3
report flagged). Cold-start confirm readings in the first minutes are artifacts —
steady-state only. Rates at different heights are not like-for-like (density rises).

## Risks, ranked

1. **Correctness: announcing/pushing uncommitted state** — closed by the two-cursor
   design; test 2 pins it.
2. **Deadlock on the single writer** — closed by lazy batch open + total acquisition
   order; the degraded mode is serial, not stuck.
3. **Reorg interaction** — closed by extension-only gating + drain; test 3 pins it.
4. **Scope creep into the BlockQueue** — the queue is untouched; only who calls
   rebase-with-which-cursor changes. If the diff starts touching queue internals, stop
   and re-read this spec.

## Deferred alongside (not this rung)

- **Coin-read batching** (26 reads/block now, worse in dust eras ~1.6M+ — pr-verify is
  heading there): needs a purpose-built fixture with a seeded genesis-history store so
  cross-block spends validate; `fullnode_store_coin_reads_total` is the watch gauge.
- **VDF work reduction** (0.47s/window): scheduling is already within 1.04× of its work
  floor (2026-08-18 campaign); remaining levers are t_op work reduction — separate track.

## File map

- `node/src/sync/mod.rs` — `follow_blocks_reporting` return split; drain points.
- `full-node/src/daemon.rs` — `block_processor` two-cursor loop; announcer/rebase/
  `finish_follow_step` move to `durable_peak`; `follow_inflight_since` span.
- `node/src/engine.rs` — `decide_batch`/`write_window` split from
  `confirm_staged_batch_in`; overlay retirement at commit; extension-only gate.
- `node/tests/staging_commit_granularity.rs` — overlap + durable-announcement +
  reorg-drain pins.
- Prior art in-tree: the BlockQueue producer/consumer decoupling (this is the same
  pattern one level down), rung-1/3 batch-carry ordering, the stall-reclaim recovery
  path. Chia reference: chia commits per-block and announces after `add_block`
  persists — the durable-announcement rule is chia parity, not an invention.
