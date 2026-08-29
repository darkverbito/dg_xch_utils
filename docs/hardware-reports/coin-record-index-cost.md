# `coin_record` index cost during genesis sync — measured A/B

Measured on a live `--genesis-sync` Postgres node at height ~1,080,000,
`coin_record` at 26.5M rows, 13 GB database. Companion to
[`apple-m1-8gb-macos.md`](apple-m1-8gb-macos.md).

**Three store-side changes took this host from 0.44 to 9.56 blocks/s — 22x — and
dropped window confirm cost from 59 s to 0.69 s (86x). The single largest
contributor was dropping five `coin_record` indexes that had never been scanned
once.**

The node was compute-starved throughout: CPU idle at 1-2% of 8 cores while
Postgres backends sat in `IO / DataFileRead`. Nothing was CPU- or
bandwidth-bound. It was random-read latency on the store, and the indexes were
manufacturing most of it.

## The A/B

Same host, same binary (`872660e`), same drive, no node restart between steps.
Rate measured over 180-300 s windows; `window_confirm_micros` is per 32-block
window.

| Step | Rate | confirm/window | heap cache hit |
|---|---|---|---|
| Baseline | 0.44 blk/s | 59.4 s | 90.01% |
| **1. Drop 5 unused indexes** | **1.86 blk/s** (4.2x) | 9.3 s | 89.85% |
| **2. `VACUUM ANALYZE`** | **5.33 blk/s** (2.9x) | 2.39 s | — |
| **3. `shared_buffers` 2 GB -> 4 GB** | **9.56 blk/s** (1.8x) | **0.69 s** | **97.69%** |

After step 3, VDF is the dominant cost again (0.82 s vs confirm's 0.69 s) — the
state the pipeline is designed for. Before step 1, confirm was 98% of the window.

## Step 1: the five indexes

`pg_stat_user_indexes`, cumulative since database creation, immediately before
the drop:

| Index | Scans | Size | Defined in |
|---|---|---|---|
| `coin_record_pkey` (coin_name) | **23,573,637** | 927 MB | `0001` (PK) |
| `coin_record_coin_parent` | **0** | 461 MB | `0003_service_indexes.sql:2` |
| `coin_record_puzzle_hash` | **0** | 318 MB | `0003_service_indexes.sql:1` |
| `coin_record_unspent_by_ph` | **0** | 160 MB | `0003_service_indexes.sql:3` |
| `coin_record_confirmed_index` | **0** | 144 MB | `0001_coin_record.sql:12` |
| `coin_record_spent_index` | **0** | 125 MB | `0001_coin_record.sql:13` |

```
seq_scan = 10   seq_tup_read = 3,726   idx_scan = 23,573,637
```

Every read on this workload is a PK lookup by `coin_name`. The three statements
the node flags as slow — `SELECT ... WHERE coin_name IN ($1)`,
`INSERT INTO coin_record`, `UPDATE coin_record SET spent_index` — are all
PK-keyed. None of the five secondary indexes can serve any of them, but the two
that write must maintain all five.

Dropping them (`DROP INDEX CONCURRENTLY`, no table lock, node kept syncing)
shrank the database 15 GB -> 13 GB and cut confirm cost 6.4x.

## Step 2: autovacuum cannot keep up

At the time of the drop `coin_record` held **604,549 dead tuples** despite
per-table `autovacuum_vacuum_scale_factor = 0.02` already being set on this host
(the default 0.2 is far too loose at this table size — `block_record` reached
122k dead against 767k live before triggering).

A manual `VACUUM (ANALYZE)` took **9m07s** on `coin_record` and reclaimed 91%
(604,549 -> 53,586 dead); `block_record` took 1m07s (5,434 -> 322). Worth
1.9 -> 5.3 blocks/s on its own.

Note the ordering matters: vacuum must clean every index, so dropping the five
first made the vacuum dramatically cheaper. Vacuuming with six indexes on 26.5M
rows under starved I/O would have taken hours.

## Step 3: `shared_buffers`

Heap cache hit ratio degrades as the table outgrows the buffer pool — it read
98.33% at 11.9M rows and 90.01% at 26.5M. Each miss is a random read from the
store before an `UPDATE` can proceed, which is what put the backends in
`DataFileRead`.

2 GB -> 4 GB restored it to **97.69%** and bought the final 1.8x.

**Do not go further on an 8 GB host.** At 4 GB this machine was already using
1.6 GB of swap; 6 GB would trade saved random reads for swap I/O on the same
device. Postgres double-caches (buffer pool + OS page cache), so the usual 40%
ceiling applies.

## The wrinkle: migrations recreate the indexes on every startup

`PostgresStore::new` calls `migrate()` on every connect, and both files use
`CREATE INDEX IF NOT EXISTS`. So **a node restart silently rebuilds all five
indexes** — undoing step 1, and rebuilding five indexes over 26.5M rows before
the sync can proceed.

That makes the drop unrepeatable as an operational workaround: it survives only
until the next restart, which is also exactly when an operator is least likely to
notice the regression.

## Suggestions

1. **Create service indexes after bulk sync, not before.** The largest available
   win and the fix for the wrinkle above. A genesis sync pays index maintenance
   on tens of millions of inserts to build structures it never reads, then a
   tip-following node needs them. A `--defer-service-indexes` flag, or creating
   them on the transition to tip-follow, removes the cost from the phase that
   cannot afford it and pays it once, sequentially, when it is cheap.
2. **Let `postgres` build without `coin-index`/`hint`.**
   `full-node/Cargo.toml` has `postgres = ["coin-index", "hint",
   "dg_xch_stores/postgres"]`, so there is no Postgres-backed *pure validating*
   build. The mmap profile has exactly this opt-out, documented in
   `running-a-full-node.md` as "a pure validating node (deliberately no
   coin-index/hint service tier)". A validating leg on Postgres cannot make the
   same choice.
3. **Gate or document `confirmed_index` / `spent_index`.**
   `0003_service_indexes.sql` is correctly behind `#[cfg(feature =
   "coin-index")]` (`stores/src/postgres/mod.rs:60`), but
   `0001_coin_record.sql:12-13` creates two more outside any gate. Both measured
   zero scans.
4. **Ship per-table autovacuum settings in the migrations.**
   `ALTER TABLE coin_record SET (autovacuum_vacuum_scale_factor = 0.02,
   autovacuum_vacuum_threshold = 1000)` and the same for `block_record`. The
   default 0.2 lets hundreds of thousands of dead tuples accumulate on a table
   this size, and confirm time degrades with them.
5. **Consider documenting a `shared_buffers` floor** for the Postgres profile.
   The cache-hit cliff is what turns confirm from 0.7 s to 59 s per window, and
   it is invisible until it bites.

Also worth noting: `coin_hint` held **0 rows** throughout, and
`sub_epoch_segments_v3` likewise. The hint tier is compiled in (implied by
`postgres`) with its table and index maintained, but nothing populates it during
`--genesis-sync`.

## Caveats

- Single host, single run, one sample per step. The steps were applied in
  sequence without a restart between them, so each figure is the marginal effect
  in that order — a different order would attribute differently (notably, vacuum
  before the drop would have been far slower and scored worse).
- Scan counts are cumulative for this database only. A node serving RPC would
  show non-zero scans on all five indexes; these are unused *for genesis sync*,
  not unused in general.
- Rate on this workload varies with per-window transaction density independently
  of any tuning; the 15-minute series behind these numbers oscillates roughly
  ±40%. The 22x aggregate is far outside that band, but individual step
  attributions carry it.
- Host is an 8-core / 8 GB M1 with the store on a USB SSD (186 MB/s sequential
  write). Cluster hardware with more RAM would hit the cache cliff much later.
