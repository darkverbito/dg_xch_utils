# Sync performance by mainnet era

What a healthy node is EXPECTED to do at each stretch of the mainnet chain, measured on this
fleet. Consult this before treating a throughput collapse as a defect: most "the node got slow"
reports are the chain getting heavy, and the fastest diagnosis is comparing the observed rate at
the observed HEIGHT against this table. Every number here was measured (fleet Prometheus,
`fullnode_peak_height` differentiated against wall clock, bucketed by height); re-derive with the
queries in §6.

Dates are approximate (height ÷ 4,608 blocks/day from 2021-03-19). The chain's own production
rate is **3.2 blocks/min** — that is the tip-following floor, and any node at or above it while
at the tip is keeping up, whatever its bulk-sync rate was.

## 1. The era table

| height range | ~date | character | measured sync rates |
|---|---|---|---|
| 0 – ~0.2M | 2021 | near-empty blocks | Threadripper ~12,000 blk/min (peak) |
| ~0.2M – 5.496M | 2021–2024 | mixed: dust storms (~1.1–1.4M), CAT/NFT waves; moderate tx density | Threadripper 4,829 @1.47M; not yet profiled per-era on this fleet |
| **5,496,000** | 2024-06 | **hard-fork boundary** (the `--sync-from=5,490,000` legs start just below it; expect a short ramp-up transient after start) | — |
| 5.5M – 6.75M | 2024-06 – 2025-03 | steady moderate density | Xeon/SQLite 1,425–1,500; Xeon/Postgres 630–730; Pi 4 ~300–340 |
| 6.75M – 8.9M | 2025-03 – 2026-07 | as above, **punctuated by heavy-region bursts** (§2) | between bursts: Xeon/SQLite ~1,450; Xeon/Postgres 670–940 (rises with height); Pi 4 300–340 |
| **~9.10M – tip** | 2026-08 → ongoing | **the cost-maxed era**: sustained near-cap-cost blocks (compute grinders). This is the live present — every tip-follower processes these now. | Xeon/Postgres: 930 → 393 @9.10M → **49 @9.15M** (7.5 h measured); Pi 4: 300 → 23 @9.1M → **6 @9.2M** (min 2 — barely above the 3.2 floor) |

Hardware classes on this fleet: Threadripper (Zen, SHA-NI), Xeon E5-2690 v2 (Ivy Bridge, **no
SHA-NI** — the class the cost-maxed era punishes hardest), EPYC (Zen, SHA-NI), Raspberry Pi 4
(Cortex-A72, no ARMv8 crypto extensions). Backend matters ~2× in the mid eras: the SQLite leg
holds ~1,450 where the Postgres leg holds ~700–900 on the same CPU class.

## 2. The heavy-region catalog (measured bursts)

Short ranges where block cost jumps an order of magnitude and every leg slows at the SAME
heights, whatever day it crosses them. Verified content, not node state: e.g. heights 6,760,003
and 6,760,012 carry 5.36B and 5.58B cost (half the 11B consensus cap) in ~12 KiB generators —
compute-dense CLVM — against ~200–270M typical for their neighborhoods.

| range | measured effect (Xeon class) | cross-checked |
|---|---|---|
| **6.754M – 6.786M** | 254 blk/min for ~105 min (Postgres leg) / 375 blk/min for ~75 min, worst single body window 13.5 s (SQLite leg, six days later) | two legs, two days, two backends — identical heights |
| 6.865M – 6.874M | ~289 blk/min, ~30 min | |
| 6.943M – 6.947M | ~258 blk/min, ~15 min | |
| 7.217M – 7.242M | ~330 blk/min, ~30 min | |
| 8.634M – 8.639M | ~348 blk/min, ~15 min | |

A burst's signature on the node (all of these together, and all transient):

- `fullnode_window_body_micros` spikes to seconds (up to ~13.5 s observed) while confirm /
  stage / vdf stay flat;
- the node's **own CPU drops** (observed 14.4 → 7.4 cores) — body validation is serial per
  block, so one giant generator on one core starves the parallel pipeline; this is the opposite
  of what co-tenant contention looks like;
- live allocation, live-bytes-per-block, and RSS billow while the staging queue backs up, and
  return fully to baseline on exit — backpressure, not a leak.

## 3. The cost-maxed era (~9.10M onward)

Sustained rather than bursty: block after block near the cost cap. Measured onsets agree across
legs — the Postgres Xeon fell 930 → 49 blk/min across 9.10M–9.15M; the Pi 4 fell 300 → 23 → 6
across 9.0M–9.2M. On SHA-NI-less hardware a single cost-maxed block takes 16–34 s of body time,
so **bulk-sync throughput through this era is tens of blocks per minute on the Xeon class and
single digits on the A72** — while tip-following stays viable anywhere above 3.2 blk/min
(measured: both the Postgres leg and the EPYC leg hold the tip through this era; the Pi 4's
9.2M rate of 2–6 blk/min brackets the floor). The engine's CLVM runs these blocks within ~1.6×
of chia_rs (see docs/algorithmic-finality.md); the era is expensive for every implementation.

## 4. Diagnosis playbook

When a throughput collapse is reported, in order:

1. **Read the height, not the clock.** Get the peak-height range covered during the slow window
   (§6 queries). If it overlaps §2 or §3, expect the numbers above — that is the chain.
2. **Content vs contention, one query**: per-pod CPU on the node across the window. Content =
   the node's own CPU *falls* with no co-tenant rising. Contention = another pod's CPU rises
   while the node's falls (then look at scheduling, not the node).
3. **Content vs defect**: content recovers exactly when the height range is exited, reproduces
   on another leg at the same heights, and leaves memory at baseline. A defect follows the
   clock or the build (correlate with rollout times), does not recover on its own, or shows
   monotonic memory growth.
4. **Verify the content directly** when in doubt: fetch block records for the suspect range
   from any public RPC and compare `transactions_info.cost` against neighboring ranges. An
   order-of-magnitude jump settles it.
5. Restart-shaped artifacts: a single all-metrics-to-zero notch with instant recovery and no
   container restart is a missed scrape (the exporter can time out during a heavy body); a
   gauge that resets after a rollout is the new process, not data loss.

## 5. Worked example

2026-08-31, SQLite Xeon leg: body windows spiked to 12–13.5 s, blocks/min fell 1,443 → ~450 for
90 minutes, RSS peaked 5.12 GiB, then everything recovered. Height range covered: 6.755M–6.795M
— the first row of §2. Own CPU fell 14.4 → 7.4 cores; no co-tenant above 1.8; zero restarts;
the Postgres leg had produced the same dip at the same heights six days earlier; block costs in
the range verified at 5.4–5.6B. Verdict in four queries: content, benign, expected on every
future crossing.

## 6. Re-deriving the numbers

Against the fleet Prometheus (job names `dg-xch-node-{clvm,pg,pg-epyc,pi}`):

```text
# height at a moment (bracket a slow window to get its height range)
fullnode_peak_height{job="dg-xch-node-clvm"}                       @ instant queries

# era curve: range-query fullnode_peak_height at 15m steps, difference successive
# samples into blk/min, bucket by height — this file's tables are exactly that

# content-vs-contention discriminator
topk(6, sum by (pod) (rate(container_cpu_usage_seconds_total{node="<node>", pod!=""}[10m])))

# the phase split during an incident
fullnode_window_{body,confirm,stage,vdf}_micros{job="..."}
```

Blocks/min are medians per 250k-height bucket unless a range is called out; minima in the same
buckets are what a §2 burst looks like from the outside. When adding a row: two legs crossing
the same heights on different days is the bar for calling a region content.
