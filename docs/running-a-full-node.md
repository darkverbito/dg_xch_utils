# Running a dg_xch Full Node

How to build and run the `full-node` daemon in this repo and have it sync
Chia mainnet. Written so you can go from a bare machine to a syncing node in
about fifteen minutes of setup plus a few minutes of sync.

## What you're standing up

A standalone Chia full node (spec 031): it dials peers over the standard Chia
wire protocol, fast-syncs to the network tip by fetching and verifying a
weight proof (the same trust model chia's own fast sync uses), fully validates
every block from that checkpoint forward (proof of space, VDFs, CLVM
execution, coin set, signatures), serves blocks to other peers, and exposes a
small RPC plus Prometheus metrics.

It is not a farmer and has no wallet keys. It holds no funds. It just syncs,
validates, and serves.

## Deployment modes

The node supports several distinct deployment shapes. Pick the one that
matches your trust posture, hardware, and patience. Every mode below is
supported on all three storage backends unless noted.

| Mode | How | Trust model | Time to useful | Who it's for |
|---|---|---|---|---|
| **Checkpoint sync** (default) | no extra flags | Weight proof attests the chain up to a recent checkpoint; full validation from there forward. Same model as chia's own fast sync. | Minutes | Most operators. |
| **Genesis validation** | `--genesis-sync` | Trust-minimal: every proof of space, VDF, and CLVM program since block 0 is checked on your own hardware. | Days to weeks, hardware-dependent (see OPTIMIZATIONS.md section 5) | Operators who want zero attestation. This mode is permanent and first-class; nothing about the newer modes degrades it. |
| **Era anchor** | `--sync-from H` | Weight proof attests to H; full validation H to tip; assume-valid below H. | Minutes to anchor, then era-speed | Splitting history across several nodes; targeted validation of a specific era. |
| **Tip-first + backward backfill** | `--sync-mode tip-first` (SPEC, see [spec-tip-first-backfill.md](spec-tip-first-backfill.md)) | Anchor at tip, farm-ready immediately (empty blocks), history backfills backward by hash linkage; full tx validation switches on when the coin set completes. Optional prune horizon for small disks. | Under an hour to farming | Farmers, Pi-class hardware, fresh-genesis fork launches. Not yet implemented. |
| **Offline corpus replay** | `--capture-dir` + the replay harness | No network at all; replays captured `RespondBlocks` blobs deterministically. | Immediate | Development, regression gates, hardware characterization. |

Storage backends: `sqlite://` (embedded, the default), `postgres://`
(industrial, `--features postgres`), `mmap://` (libbitcoin-style, the
Raspberry Pi profile, `--features mmap`). Hardware profiles and the
benchmarks to run before choosing are in the last section of this document.

## Prerequisites

- Linux or macOS, x86_64 or arm64. 4+ cores and 8 GB RAM are comfortable;
  it has run on far less (a Raspberry Pi 4 is the design floor).
- ~10 GB free disk for the checkpoint-sync profile (the default). A full
  from-genesis sync is a different story — see the last section.
- Rust (stable) via https://rustup.rs, plus `cmake` and a C compiler
  (`build-essential` on Debian/Ubuntu, Xcode CLT on macOS).
- Outbound internet on TCP 8444. Nothing inbound is required.

## Build

Clone the repo:

```bash
git clone https://github.com/GalactechsLLC/dg_xch_utils.git
cd dg_xch_utils
cargo build --release -p full-node --features sqlite,coin-index,hint
```

The binary lands at `target/release/full-node`. The feature set above is the
embedded profile: SQLite storage plus the coin-index/hint service tier so the
RPC can answer coin and puzzle-hash queries.

## Run

```bash
mkdir -p ~/dg-xch-data
./target/release/full-node \
  --listen 0.0.0.0:8444 \
  --rpc 127.0.0.1:8555 \
  --introducer introducer.chia.net:8444 \
  --db sqlite://$HOME/dg-xch-data/chain.db \
  --network mainnet \
  --metrics 127.0.0.1:9100
```

Flags worth knowing:

| Flag | What it does |
|---|---|
| `--listen` | P2P listener. Other peers pull blocks from you here. |
| `--rpc` | Local RPC (block/coin queries, push_tx), chia-compatible: every response is the chia envelope (`{"<key>": ..., "success": true}`; errors are HTTP-200 `success: false`) and the listener speaks chia's 8555 TLS posture — server cert from the private CA chain (`PRIVATE_CA_CRT`/`PRIVATE_CA_KEY` env, else the embedded public chia CA), **client certificate required** and verified against that CA, exactly like a stock chia node. Chia tooling (`chia rpc`, `FullNodeRpcClient`, this repo's `FullnodeClient`) connects unchanged with its usual certs. For bare in-cluster tooling without certs set `DG_XCH_RPC_ALLOW_ANY_CLIENT_CERT=1`. Liveness/metrics probes stay on the plain-HTTP `--metrics` port. Keep 8555 on localhost unless you mean it. |
| `--introducer` | Bootstrap seed. `introducer.chia.net:8444` hands you live mainnet peers. |
| `--peer host:port` | Manual peer, repeatable. Dialed directly and persistently re-dialed if dropped. With trusted fast peers you can skip the introducer entirely. |
| `--advertise ip:port` | Your WAN address, gossiped so peers can dial back. Only useful with a port forward; omit it otherwise. |
| `--db` | `sqlite://<path>`, `postgres://user:pass@host:5432/db`, or `mmap://<dir>`. Postgres needs `--features postgres`; mmap needs `--features mmap`. |
| `--network` | `mainnet` (default) or `testnet11`. |
| `--metrics` | Prometheus `/metrics` address, or `off`. |
| `--genesis-sync` | Validate the historical chain from block 0 instead of fast-syncing. See below. |
| `--sync-from H` | Anchor mid-chain at height H (weight-proof-verified) and fully validate H→tip. Lets several nodes split the chain: node A runs `--genesis-sync`, node B `--sync-from 4575000`, and together they cover the whole history in half the calendar time. |

Logs are JSON on stdout. `RUST_LOG=info` is the default; `RUST_LOG=debug`
gets chatty.

## What a healthy startup looks like

1. `metrics server listening`, then peer connections coming up.
2. Within a minute, a peer announces its tip and you'll see
   `fast-sync: fetching weight proof (racing all peers)` — the proof is
   ~14 MB and is raced across every live peer.
3. `fast-sync: validating weight proof` — two to four minutes of CPU. This is
   the step that makes the checkpoint trustless.
4. Headers-first candidate records, then block bodies confirming:
   `fast-sync landed at recent-chain peak`, and from there tip-follow keeps
   up with the network (one 32-block window per tick).

From process start to holding the live tip is typically **under ten
minutes** on ordinary hardware.

Sanity checks:

```bash
curl -s localhost:9100/metrics | grep fullnode_peak_height
# climbing toward the network tip, then tracking it

curl -s localhost:9100/metrics | grep fullnode_outbound_peers
# > 0
```

Transient `follow step failed, retrying next tick` warnings are normal — a
lagging peer rejecting a range it doesn't have yet; the follow rotates to
another peer next tick.

## Postgres instead of SQLite

Build with the postgres profile and point `--db` at a database that exists
(migrations run automatically on first connect):

```bash
cargo build --release -p full-node --features postgres
./target/release/full-node ... \
  --db "postgres://user:password@localhost:5432/dgxch"
```

Same node, same validation — only the storage engine changes. SQLite is the
embedded default; Postgres is the industrial one.

## The mmap backend (the Pi profile)

The third store is an embedded libbitcoin-style memory-mapped table backend —
a pure validating node (deliberately no coin-index/hint service tier), built
for the Raspberry-Pi floor:

```bash
cargo build --release -p full-node --no-default-features --features mmap
mkdir -p ~/dg-xch-data/chain
./target/release/full-node ... --db mmap://$HOME/dg-xch-data/chain
```

The directory fills with mapped tables (`blocks.tbl`, `bodies.dat`,
`coins.tbl/dat`, `heights.dat`). Two things to know:

- **The directory must be writable by the node's uid.** In Kubernetes that
  means `fsGroup` in the pod securityContext (a fresh PVC mounts root-owned
  and the node exits with `open mmap store: Permission denied` otherwise).
- It is the newest backend: correct (same store contract suite as the other
  two) but the least performance-tuned — current `store.persist` is ~19 ms/
  block vs Postgres-async's 2 ms, and the write-amplification study is open.
  Perfect for finding problems, which is what testing on real hardware is for.

## Docker

The repo `Dockerfile` builds the node image; pick the storage profile with a
build arg:

```bash
docker build -t dg-xch-node .
docker run -d --name dg-xch-node \
  -v dgxch-data:/data \
  -p 8444:8444 -p 127.0.0.1:9100:9100 \
  dg-xch-node \
  --listen 0.0.0.0:8444 --rpc 0.0.0.0:8555 \
  --introducer introducer.chia.net:8444 \
  --db sqlite:///data/chain.db --metrics 0.0.0.0:9100
```

## Syncing from genesis

`--genesis-sync` turns off the weight-proof fast path and validates the
entire historical chain block by block from height 0 — every proof of space,
every VDF, every CLVM program since 2021. Point it at peers that hold the
full history (any long-running standard Chia node does; a checkpoint-synced
dg_xch node does not).

State of play: this path is no longer the frontier it was. Offline corpus
replays are green across every era band we hold — the 2021 dust storm, the
2022 transaction peak, the heavy modern era, and straight through the
5,496,000 hard fork — and two cluster nodes are live-validating the real
chain from both ends (genesis-up and a `--sync-from 4575000` anchor). The
divergence ledger (`core/tests/DIVERGENCES.md`) stands at 23 found-and-fixed,
each proven against the chia reference. Walls are still possible in bands no
corpus covers — if your node stops advancing with `consensus rejected block`,
the height and error message are exactly what we want to hear about.

Expected rates vary hugely by era (this is normal): hundreds of blocks/s in
empty stretches, ~15/s in tx-dense eras on a 10-core Xeon, ~5/s in the
heaviest modern bands. The full arithmetic of what your hardware should
achieve is in `docs/OPTIMIZATIONS.md` §5.

## Testing on your own hardware (including a real Pi 4)

Before running the node, characterize the machine — two benchmarks, no chain
or network needed, a few minutes total:

```bash
# 1. Class-group squaring: the single number that predicts VDF-bound sync rate
cargo test --release -p dg_xch_vdf --test square_bench -- --ignored --nocapture decompose_squaring_cost

# 2. Where the squaring time goes on this silicon (Lehmer loops vs composition)
cargo test --release -p dg_xch_vdf --features phase-profile --test phase_profile -- --ignored --nocapture
```

Reference points for the `SQUARE:` number so far:

| Machine | µs/op |
|---|---|
| AMD EPYC 9015 (Zen 5) | 7.97 |
| Apple M-series | 8.68 |
| Xeon E5-2690 v2 (Ivy Bridge, 2013) | 24.5 |
| Raspberry Pi 4 (Cortex-A72) | **unmeasured — yours is the first** |

Predicting your sync rate from it: VDF-bound eras cost ~3,200 group-ops per
block on average, so `blocks/s ≈ cores / (3200 × t_op)`. A Pi 4 at a guessed
40–60 µs/op would do ~4–7 blocks/s in VDF-bound stretches on all four cores —
the bench replaces the guess with a number in one minute.

Pi-4 specifics:

- **Build on-device or cross-compile** (`aarch64-unknown-linux-gnu`).
  On-device release build takes a while on 4 cores; it works. 64-bit OS
  required (the arithmetic is built on 64-bit limbs).
- **Backend**: `--features mmap` with `--db mmap://...` is the intended Pi
  profile. SQLite also works. Put the store on USB-attached SSD if you can;
  an SD card will work but is the write-amplification worst case — which is
  itself a measurement we want (SD wear and stall behavior under sustained
  sync).
- **Memory**: 4 GB is the design floor and the checkpoint-sync profile fits
  comfortably (the cluster's Pi-envelope node idles ~tens of MB and the sync
  pipeline holds O(window) memory by design). Genesis-sync also fits — the
  store, not RAM, is the reorder buffer.
- **Thermals**: sustained sync is an all-core integer workload; an
  unheatsinked Pi will throttle and your blocks/s will sag. Note whether you
  have a heatsink/fan when reporting numbers.
- **Start with checkpoint sync** (the default) to prove the machine holds
  tip-follow, then try `--genesis-sync` for the long-haul characterization.

What to report back (this is the valuable output of hardware testing):

1. The two bench outputs (`SQUARE:` and the phase-profile table) plus
   `/proc/cpuinfo` model name.
2. `curl -s localhost:9100/metrics | grep -E 'peak_height|outbound_peers'`
   snapshots over time — i.e. your blocks/s per era band.
3. RSS over time (`ps -o rss= -p <pid>` hourly is fine) — it should stay flat;
   growth is a bug we want.
4. Any `consensus rejected block` height + error, verbatim.
5. On SD cards: any stalls, and the card's wear/health before and after.

## If something goes wrong

- **No peers**: check outbound 8444 isn't blocked; try an explicit
  `--peer` to a node you know.
- **Weight-proof fetch times out**: usually one slow peer; it retries and
  races all peers each tick. Add more peers.
- **`postgres:// --db requires a binary built with --features postgres`**:
  rebuild with that feature.
- **Wall at a specific height** (`consensus rejected block ...`): report the
  height, the error, and a few log lines. Divergences from chia consensus are
  tracked and fixed in `core/tests/DIVERGENCES.md` — 23 closed so far, each
  proven against the chia reference implementation.
