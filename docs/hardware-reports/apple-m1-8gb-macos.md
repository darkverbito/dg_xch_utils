# Hardware report: Apple M1 (8-core, 8 GB), macOS

A genesis-sync run on Apple Silicon, submitted against the "What to report back"
list in [`running-a-full-node.md`](../running-a-full-node.md). Every number below
was measured on the machine described; nothing is extrapolated except where
explicitly labelled.

Two results are worth reading first:

- **§8 — a silent sync stall**: a tracing span panic kills tokio workers while the
  process stays alive and every liveness check still passes. Independently
  reproduced here on non-cluster hardware; **fixed upstream in `3cfb80d`** while
  this report was open, with a `/health` progress endpoint added in `68a6f40`.
  The section carries a correction: an earlier revision blamed the tracing crate,
  which was wrong. The durable finding is that no liveness check detects this —
  only a progress check does.
- **§1 — a store-backend comparison.** On this host SQLite's confirm cost grew
  ~700x between height 48k and 330k and became the binding constraint, while
  Postgres at the same height stayed an order of magnitude cheaper and left the
  node VDF-bound. Same binary, same drive, same chain segment.

## The machine

| | |
|---|---|
| CPU | Apple M1, 8 cores |
| RAM | 8 GB |
| OS | macOS 26.6.1 (Darwin 25.6.0), arm64 |
| rustc | 1.97.1 |
| Store device | USB APFS SSD, 186 MB/s write / 315 MB/s read (measured, 1 GiB `dd`) |
| Build | `--release -p full-node`, features as noted per run |
| Repo | `full-node` @ `9dc147a` |

Both runs used `--genesis-sync` against `introducer.chia.net:8444`, 8 outbound
peers, store on the same external SSD.

## 1. Store backend: SQLite vs Postgres at the same height

`fullnode_window_confirm_micros` is per 32-block window.

| | SQLite | Postgres |
|---|---|---|
| Height | 330,207 | ~335,000 |
| DB size | 2.6 GB | 2.1 GB |
| **Rate** | **5.6 blocks/s** | **23.9 blocks/s** |
| confirm (µs) | 2,764,556 and 4,489,754 (2 samples) | min 33,246 / mean 84,278 / max 791,571 (19 samples) |
| vdf (µs) | 814,855 | mean 905,021 |
| **Store share of window** | **~75%** | **8.5%** |

**~4.3x end-to-end throughput, and the bottleneck moves back to compute where it
belongs.** Postgres's confirm cost is spiky (a 24x spread between min and max,
consistent with checkpoint behaviour) — hence the 19-sample characterisation
rather than a single reading. The SQLite figures are only 2 samples, but both
were within 2.8–4.5 s and the resulting rate was independently confirmed by
block-counter deltas, so the conclusion does not rest on sampling luck.

Postgres settings used are in §6. Notably `synchronous_commit = off`, which is
defensible here because the store is rebuildable from the chain.

## 2. The SQLite scaling curve (why the switch was made)

Same run, same database, increasing height:

| Height | confirm (µs/window) | Rate |
|---|---|---|
| ~3,400 | 3,848 | 23 blocks/s |
| ~48,000 | 3,815 | 19–20 blocks/s |
| 330,207 | 2,764,556 | 5.6 blocks/s |

Confirm cost is flat through the first ~50k blocks and then grows by roughly
three orders of magnitude by 330k — from ~0.1 ms/block to ~86 ms/block. For
reference `running-a-full-node.md` cites ~19 ms/block for mmap and ~2 ms for
Postgres-async; **no SQLite persist figure is currently published**, and on this
host it ends up well outside both.

This is the practical form of the §5.2 claim that the serial residue "becomes
binding the moment a machine's parallel throughput approaches ~100 blocks/s" —
with SQLite on this host it became binding at ~8 blocks/s instead.

## 3. VDF: reproducing the M-series reference

`cargo test --release -p dg_xch_vdf --test square_bench -- --ignored --nocapture
decompose_squaring_cost`, **with the node stopped**:

```
SQUARE: 8.671µs/op over 2000 ops
```

This independently reproduces the 8.68 µs/op "Apple M-series" reference point in
`running-a-full-node.md` on an 8-core M1 — the figure holds on base M1 silicon,
not only on higher-tier parts.

Cross-check against the §5.1 model: at C=8 the table gives 3.4 d for 7.97 µs/op;
scaling to 8.671 µs/op predicts **~3.7 days**. Measured sustained rate of 23.9
blocks/s over the remaining chain implies **~4.3 days**. Model and measurement
agree within ~15% **[DERIVED]**.

## 4. Negative result: build flags do not help

Tested because the workload is compute-bound and the default profile sets no
`[profile.release]` tuning. Both binaries benched back-to-back with the node
**paused** (`SIGSTOP`), same machine, same session:

| Build | µs/op |
|---|---|
| Default (`force-frame-pointers=yes`, no LTO, cgu=16) | **8.671** |
| `-C target-cpu=native`, `lto=fat`, `codegen-units=1`, no frame pointers | **8.897** |

**No improvement — marginally worse, within noise.** The class-group arithmetic
runs through GMP (`rug` / `gmp-mpfr-sys`), which is hand-written aarch64
assembly; Rust codegen options do not reach the hot loop. Suggested for the §6
dead-ends ledger so the experiment is not repeated.

## 5. Methodology warning: do not bench under load

The same benchmark returns **17.114 µs/op** when run while a node is syncing, and
**8.671 µs/op** with the node stopped — a ~2x error that looks exactly like slow
silicon. An early draft of this report drew the wrong conclusion from it.

Recommend `running-a-full-node.md` state explicitly that the two characterisation
benchmarks must be run on an idle machine.

## 6. macOS-specific gaps

Neither blocks a sync, both cost time to diagnose:

**`effective_io_concurrency` must be 0.** Postgres refuses to start otherwise:

```
invalid value for parameter "effective_io_concurrency": 200
DETAIL: must be set to 0 on platforms that lack posix_fadvise().
```

**`fullnode_process_resident_bytes` always reports 0** on macOS. Item 3 of the
"what to report back" list asks for RSS over time to detect growth; that panel is
blank on this platform, so this report cannot answer it. Worth either
implementing via `task_info`/`proc_pidinfo` or documenting as Linux-only.

Postgres settings used for §1, for reproducibility:

```
shared_buffers = 2GB
effective_cache_size = 5GB
maintenance_work_mem = 512MB
work_mem = 64MB
synchronous_commit = off
wal_compression = zstd
wal_buffers = 64MB
max_wal_size = 16GB
min_wal_size = 2GB
checkpoint_completion_target = 0.9
checkpoint_timeout = 30min
random_page_cost = 1.1
effective_io_concurrency = 0
autovacuum_vacuum_cost_limit = 3000
autovacuum_naptime = 30s
```

## 7. Consensus

**Zero `consensus rejected block` events** across both runs, through height
~335,000 (2021-era chain). Nothing to report against the divergence ledger from
this segment; the run continues and any wall will be reported separately with
height and verbatim error.

## 8. Silent sync stall from a tracing span panic — FIXED UPSTREAM

> **Resolved in `3cfb80d`** (and `/health` added in `68a6f40`) while this report
> was open. The section is kept as an independent third-party reproduction on
> non-cluster hardware. **The root-cause attribution in an earlier revision of
> this section was wrong** — see "Correction" below.

Reproduced once on this host, independently of the cluster sightings the fix
commit describes on the x86 deployments.

At height 466,303 the node stopped advancing while remaining, by every external
signal, healthy: process alive, 8 peers connected, Postgres accepting
connections, Prometheus target `up`, `/metrics` serving. Only
`fullnode_peak_height` revealed it — flat for the whole window.

Two tokio worker threads had panicked:

```
thread 'tokio-rt-worker' panicked at
  tracing-subscriber-0.3.23/src/registry/sharded.rs:317:9:
assertion `left != right` failed:
  tried to clone a span (Id(7023364443518009349)) that already closed
  left: 0
 right: 0
```

The panic unwinds the worker but not the process, so the sync pipeline dies while
the daemon lives on.

**Observed correlate: span volume.** At `RUST_LOG=info` the run produced
**1,598,441 span-close events in 368 MB of log**, i.e. 99.99% of all log lines
were `"message":"close"`.

Stall window, from the Prometheus series (`fullnode_peak_height` flat):

```
STALL: 08:16 -> 08:56 local  (height 466,303, ~40 min until manual restart)
```

**Mitigation used here: `RUST_LOG=warn`.** After restart the node resumed at 20.5
blocks/s with no blocks lost — Postgres had committed through 466,303 and the
sync resumed from the stored peak.

### Correction

An earlier revision of this section stated the panic was "a bug in the logging
library, not in dg_xch consensus code", and proposed demoting the per-connection
`close` span to DEBUG. **Both were wrong.** Per `3cfb80d`, the actual cause is
documented tracing misuse in this repo:

> an `Entered` span guard (`let _e = span.enter()`) held across `.await` points
> under the multi-thread tokio runtime. Work-stealing moves the suspended future
> to another worker while the span is entered, racing the sharded registry's span
> clone/close. This is documented tracing usage guidance, not a crate bug.

Twelve sites across `engine.rs` and `sync/mod.rs`, fixed with
`Future::instrument` and `Span::in_scope`.

That reframes the evidence recorded above. Span volume was a **correlate, not the
trigger** — high INFO throughput widens the window for a race that exists at any
log level. `RUST_LOG=warn` therefore made the stall less likely, it did not
remove it, and it should not be treated as a fix on builds predating `3cfb80d`.

### What survives the correction

**The monitoring finding is unaffected, and is the durable lesson:** no liveness
check detected this. Process checks, port checks, peer counts, and Prometheus
target health all reported healthy for the full 40 minutes. Only a *progress*
check — `increase(fullnode_peak_height[15m]) == 0` — caught it.

`68a6f40` now ships exactly this as `GET /health` on the metrics server, returning
503 on the silent-stall signature. Verified working on this host after rebuild:

```
$ curl -w " [%{http_code}]" localhost:9100/health
ok: boot grace [200]
```

Operators on cluster deployments get it via `livenessProbe`. Anyone running the
binary directly should poll `/health` or alert on the Prometheus expression above;
a process check is not sufficient.

## Open items

- Sync is ongoing past 470k; rates for transaction-dense eras (2021 dust storm,
  2022 peak) are not yet measured on this host. `fullnode_window_tx_blocks` is
  13/32 at 335k, so body cost is only beginning to appear.
- No SQLite persist figure is published in the docs; §2 suggests one is worth
  adding, since SQLite is the default profile in the `Dockerfile`.
- The §8 stall was seen once here and is fixed upstream in `3cfb80d`; this host
  has been rebuilt onto that fix. No recurrence observed since, but the run is
  not long enough to be evidence either way.
- A companion report on `coin_record` index cost during genesis sync is in
  [`coin-record-index-cost.md`](coin-record-index-cost.md).
