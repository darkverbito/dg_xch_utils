# Confirm batching: the serial-residue attack — measured decomposition and fixes

A live EPYC 9015 host (managed Postgres 17 on SAN storage,
`shared_buffers=4GB`, `max_wal_size=2GB`), height ~1.69M (dust era, measured 868
additions/block), 2026-08-18. Companion to
[`coin-record-index-cost.md`](coin-record-index-cost.md).

**Headline: window confirm was 595 ms/block — ~95% of the per-block budget — and the
bytes are not statement round-trips, not parse/bind, not commits, and not index probes.
They are `LWLock/WALWrite` stalls inside the multi-row `INSERT INTO coin_record` upsert,
driven by full-page-image WAL amplification under forced-checkpoint pressure.** One
per-database config change (`wal_compression=zstd`) took the live node from ~3.7 to
6.7 blocks/s sustained; the code changes below remove the mechanism from the confirm
path itself.

## 1. The decomposition (measurement before optimization)

Per-statement server-side durations (`log_min_duration_statement=0`, 65 s capture) —
totals across the capture window **[CITED]**:

| Statement | Server time | Calls | Per call |
|---|---|---|---|
| `INSERT INTO coin_record …` (multi-row upsert, execute) | **44,359 ms** | 51 | **870 ms** |
| `INSERT INTO block_record …` (stage path) | 5,142 ms | 86 | 60 ms |
| `INSERT INTO block_body …` (stage path) | 3,779 ms | 87 | 43 ms |
| coin INSERT parse+bind | 224 ms | 97 | 2.3 ms |
| `UPDATE coin_record SET spent_index` | 25 ms | 46 | 0.5 ms |
| set_peak walk (SELECT+UPDATE chains) | ~7 ms | ~500 | ~µs |
| `COMMIT` (async commit) | ~µs each | — | — |

Total server-side: 53.6 s of a 65 s wall — the confirm cost is server *execution*, and
one statement shape owns 83% of it. At 868 additions/block that is **1.0 ms per coin
row**.

Wait-event attribution (600 samples of `pg_stat_activity` at 100 ms) **[CITED]**:
of all `active` samples inside the coin INSERT, **95% were `LWLock/WALWrite`**
(416/438); `IO/DataFileRead` — the coin-record-index-cost report's mechanism — was
absent (the 4GB shared_buffers step already fixed reads; spends target hot recent
pages, hence the cheap UPDATE).

The amplification, from the server's own counters **[CITED]**:
- `pg_stat_wal`: 708M wal_records, **139.9M full-page images**, **990 GB WAL written**
  for a 23 GB table (16 GB heap + 6.9 GB PK) — **1.4 KB WAL per row average**.
- `pg_stat_checkpointer`: **614 WAL-pressure-forced checkpoints** vs 124 timed —
  `max_wal_size=2GB` forces a checkpoint every few seconds at this write rate, so
  nearly every randomly-touched btree leaf is re-imaged (8 KB FPI) almost every window,
  and the checkpoint's own dirty-buffer flood contends with WAL flushes on the same
  volume.

## 2. Causal probe: `wal_compression=zstd` (live A/B)

`ALTER DATABASE dgxch SET wal_compression='zstd'` + backend recycle (per-database
setting — no restart, survives the operator's config reconciliation). Same node, same
era, tx-density 8–15 tx-blocks/window on both sides **[CITED]**:

| | Before | After (10 min) | After (35 min) | After (92 min, spans a vacuum cycle) |
|---|---|---|---|---|
| Confirm (window gauge) | 595 ms/blk (10-min mean); 3.2–7.0 s/window sampled | 73 ms/blk mean; 17/30 windows < 1 s | 7.6 ms/blk (spot, 11-tx window) | vacuum-collision windows 42–66 s (see §4) |
| Block rate | ~3.7 blk/s (222/min) | 6.18 blk/s (371/min) | 6.74 blk/s (404/min) | **5.2 blk/s (313/min) incl. the collision** |

The confirm collapse under FPI compression alone confirms the WAL-volume mechanism
decisively. Remaining slow windows are checkpoint-coincident (still every ~10 s at
2 GB `max_wal_size`) plus the periodic autovacuum collision (§4). VDF (~14–16 ms/blk)
is the dominant window phase between those events. **The 500 blk/min milestone is NOT
yet met sustained**: 313/min spanning a vacuum cycle, 404/min between cycles. What
remains is the §4 config roll (checkpoint stretch + vacuum cost limit + buffers), after
which the residual serial term should be re-measured before reaching for the pipeline
confirm/validation overlap.

## 3. Code fix landed (this change): mmap batched sync-before-link

`apply_block` fsynced the coin log **per new coin** and `ChainedTable::insert` msynced
the whole table map per insert — at ~100 ms/fsync on iSCSI and dust-era addition
counts, that is the measured ~1.8 s/blk mmap-node confirm almost exactly. The libbitcoin
crash rule ("data syncs before the link lands", `stores/src/mmap/DESIGN.md`) is now
applied at the **batch boundary**: `apply_block_in` appends log frames and stages the
table links in the `BatchHandle`; `set_peak_in` (before the peak walk, so the meta
write stays last) or `commit` drains them behind ONE log fsync + ONE ordering msync +
ONE link msync for the whole window — sync calls per window drop from
O(new coins + new records) to O(1). `add_block_records` / `append_many` get the same
two-phase shape per call (one ordering sync per call instead of one per new entry).
Standalone `apply_block` keeps per-block durability with the same two-phase ordering.
New contract tests pin the changed semantics: staged links are invisible before the
batch's durability point, publish at `set_peak_in`/`commit` (peak never lands over
unlinked coins), and a dropped batch loses its staged coins wholly.

Offline A/B (an offline builder, scratch dir, synthetic dust catch-up: 32-block
windows × 868 additions + 5% recent-coin spends per block, trait-driven exactly like
the engine's confirm loop) **[CITED]**:

| | baseline (768b378) | batched (this change) |
|---|---|---|
| 160 blocks × 868 adds | **did not finish in 1200 s** (>7.5 s/blk) | **1.79 s = 11.2 ms/blk** |
| sync syscalls, one 32-blk window × 200 adds (strace -c) | **12,866** (6,433 fdatasync + 6,433 msync), 73.7 s | **5** (2 fdatasync + 3 msync), 0.44 s |

The baseline's per-new-coin fsync + full-map msync is the mmap node's measured
~1.8 s/blk confirm; the batched path's residual is in-place table writes plus the
O(1) sync points per window the strace row shows directly.

## 3b. Negative result: the Postgres catch-up coin journal — measured OUT

The prescribed "widen the store batch" fix for Postgres — journal coin deltas to an
index-free heap inside the window transaction, fold into `coin_record` in coin_name-
sorted 250k-row merges — was implemented, tested green (drain-on-read/rollback/
near-tip-flip semantics), benched, and **dropped**:

- Offline A/B (an offline builder's scratch Postgres, 640 blocks × 868 adds, trait-driven catch-up
  shape): journal **24.5 ms/blk vs direct 24.0 ms/blk**, and **+63% WAL**
  (577 KB/blk vs 354 KB/blk) — the journal pays every coin row twice (journal append +
  merge insert) **[CITED]**.
- The claimed FPI win is checkpoint-epoch alignment of btree leaf touches, and the
  arithmetic at production scale says it is small: 250k random keys over the ~850k-leaf
  PK touch ~216k distinct leaves in one epoch vs ~246k across nine 10-s epochs on the
  direct path — **~12% fewer FPIs**, roughly cancelled by the journal's own WAL
  **[DERIVED]**. And once `max_wal_size`/`checkpoint_timeout` stretch the epoch to
  30 min (§4), the direct path gets the same alignment for free and the journal's
  margin goes to zero.
- The merge also stays ON the serial confirm path (it fires at a window-commit
  boundary), so it relocates rather than removes the stall.

Revive-condition: only with the merge moved OFF the serial path (the pipeline
confirm-overlap half) *and* evidence of real leaf-reuse at scale would this pay. The
Postgres confirm cost is a WAL-volume/configuration problem (§2, §4), not a statement-
shape problem — parse/bind/round-trips/commits measured ≤3% of it (§1).

**SQLite — deliberately untouched.** The fleet measurement named pg and mmap; the
sqlite backend already has the phase-aware batching, quiet-checkpointer, and page-cache
work from the follow-up work. No fix before a decomposition names its bytes.

## 4. Config recommendations (fleet manifests, operator-coordinated)

For `applications/mke/dg-xch-node-pg/postgres-*.yaml` (bare-metal-k8s-setup):

```yaml
wal_compression: "zstd"        # measured 1.8x block rate on the live EPYC node alone
max_wal_size: "64GB"           # 614 forced checkpoints vs 124 timed at 2GB; stretch to timed-only
checkpoint_timeout: "30min"    # with the above: FPI per leaf once per 30min, not once per ~10s
shared_buffers: "16GB"         # epyc-lower has 131GB allocatable; 4GB was the M1-host number
autovacuum_vacuum_cost_limit: "10000"  # see below
```

**Autovacuum collision (observed live, 09:00Z):** with 2.33M dead tuples accumulated
(spends = updates at ~300 dead/s in this band), an `autovacuum: VACUUM ANALYZE
coin_record` started and was itself sampled in `LWLock/WALWrite` — vacuum WAL competing
with confirm WAL dropped the leg from ~6.7 blk/s to ~0.6 blk/s for the duration and
confirm windows hit 42–66 s. At default cost limits a WAL-starved vacuum of a 76M-row
table dribbles for tens of minutes; raising the cost limit lets it finish fast instead
of colliding long. This recurs roughly every ~2 h at dust-era spend rates — sustained-
rate measurements must span a vacuum cycle to be honest (ours below do).

`max_wal_size` sizing note: the 200 Gi volume holds DB (23 GB @1.69M, growing) + WAL;
64 GB WAL headroom is safe today but should be revisited as the DB grows past ~100 GB.
The `wal_compression` half is already live on the epyc leg (per-database setting);
adding it to the manifest makes it explicit and fleet-wide.
