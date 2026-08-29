# Derived coin-set roots (mainnet, layout v1)

**Status: DERIVED artifact — not consensus.** This directory publishes coin-set commitment
roots for chia mainnet at sub-epoch-summary boundary heights, derived independently by
multiple independently genesis-synced dg_xch nodes. No consensus rule, wire message, or
sync path reads these values today; phase 2 (order-independent backfill, SwiftSync-style)
will consume them fail-closed. Design record:

## Trust model

The trust class is **quorum-of-independent-syncs** (the same class as Bitcoin's reviewed
`assumeutxo` hashes, but continuously reproducible and machine-checked):

- Each root in `mainnet-v1.json` was derived by streaming the coin set out of a store that
  a dg_xch full node built by **genesis-validating mainnet itself** (no `--sync-from`
  anchor, full validation).
- A root is published only when **at least two legs that synced independently — on
  different store backends where noted — produced the identical value.** The derivation
  tool and every input (height, header hash) are recorded per entry.
- Anyone with a synced chia node can reproduce every value: the accumulator below is
  deterministic and fully specified; disagreement with this table is a bug report we want.

**Fail-closed usage rule (SwiftSync's trust shape):** a consumer must treat these roots
the way SwiftSync treats hints — an aid whose corruption can only make a sync *fail*,
never make invalid state *accepted*. Concretely: a node that syncs against a published
root must verify its own derived root matches at the boundary and abort on mismatch;
it must never skip validation it would otherwise perform on the strength of this file.

## Accumulator spec (v1) — normative

The reference implementation with the identical text is `roots/src/lib.rs`
(`dg_xch_roots`); the derivation tool is `roots/src/bin/coin_root_derive.rs`
(`coin-root-derive`). An independent implementation of the rules below reproduces every
root byte-for-byte.

### Inputs

The coin set as of a main-chain height `H`: every coin with
`confirmed_height <= H`, each carrying `(coin_id, confirmed_height, timestamp,
spent_index)` with chia `coin_record` semantics (`timestamp` = confirming transaction
block's timestamp; `spent_index` = spend height, 0 if unspent), plus the main-chain
`header_hash` at `H`.

### Canonical order

Coins are sequenced ascending by `(confirmed_height, coin_id)`; `coin_id` compares
bytewise (unsigned lexicographic, 32 bytes). Intra-block insertion order is deliberately
NOT used: no store backend guarantees it, while bytewise coin-id order is reproducible
from block data alone (sort each block's additions by coin id). Leaf hashes commit to the
leaf index, so this order is load-bearing — a permutation changes the root (tested
red-first in `roots/tests/accumulator.rs`).

### Hashes

All SHA-256. Every hash is domain-separated with an ASCII prefix carrying the layout
version. `LE32`/`LE64` are little-endian fixed-width integers.

```text
coin leaf   L_i   = H("dgxch.coinroot.v1.coin-leaf" || LE64(i) || coin_id[32]
                      || LE32(confirmed_height) || LE64(timestamp))
interior    N     = H("dgxch.coinroot.v1.node" || left[32] || right[32])
peak bag          = H("dgxch.coinroot.v1.bag" || peak[32] || acc[32])
coin MMR root     = H("dgxch.coinroot.v1.mmr-root" || LE64(n) || bagged[32])
bitmap leaf B_j   = H("dgxch.coinroot.v1.bitmap-leaf" || LE64(j) || chunk[128])
bitmap root       = H("dgxch.coinroot.v1.bitmap-root" || LE64(n) || bagged[32])
empty tree        = H("dgxch.coinroot.v1.empty")
root_v1           = H("dgxch.coinroot.v1.root" || mmr_root[32] || bitmap_root[32]
                      || LE32(H) || header_hash[32])
```

### Structure

- **Coin MMR** (FlyClient/Grin lineage; Merkle Mountain Range): standard binary-counter
  construction over the coin leaves in canonical order — one perfect-subtree peak per set
  bit of the leaf count `n`; two equal-size peaks merge into an interior node `N`. Peaks
  are bagged right-to-left (`acc` = rightmost peak, fold toward the leftmost), then the
  leaf count is bound: `H(mmr-root-domain || LE64(n) || acc)`. `n = 0` yields the empty
  constant.
- **Spent bitmap** (Grin TXO-MMR pattern: append-only output MMR + spentness bitmap;
  Grin's bitmap accumulator chunks at 1024 bits): bit `i` corresponds to coin leaf `i`,
  LSB-first within each byte (byte `i / 8`, mask `1 << (i % 8)`), set iff
  `0 < spent_index <= H`. The bitmap splits into 1024-bit (128-byte) chunks, final chunk
  zero-padded; chunk hashes `B_j` are leaves of a second MMR with the same node/bag/count
  rules (count bound is the coin count `n`).
- **Combined root**: `root_v1` binds both sub-roots to the boundary height and the
  main-chain header hash at that height — the header a weight proof attests, which is
  what makes the artifact citable against the chain.

Because the coin MMR is append-only in exactly confirmation order, the accumulator state
at any `H' > H` extends the state at `H` (peaks only grow); the bitmap is recomputed per
boundary. The derivation tool streams the coin set once and emits every requested
boundary in that single pass.

### Versioning

The layout version lives in every domain string. Any change to byte layout, order,
chunking, or domain text is a new version (`dgxch.coinroot.v2.*`) with a new ledger file;
v1 values are frozen forever (pinned vectors in `roots/tests/`).

## The ledger — `mainnet-v1.json`

One entry per derived boundary: `height`, `header_hash`, `coin_count`, `spent_count`,
`root_v1` (plus the two sub-roots), and derivation metadata: which legs agreed, their
store backends and sync modes, the derivation date, and the tool commit. Boundary heights
are SES-carrying main-chain blocks (the greatest SES block at or below each 100k-block
target), so each root sits on a height weight proofs already attest.

## Reproducing

```bash
# Boundary heights (SQL store of any synced leg):
coin-root-derive find-boundaries --db-url postgres://… --every 100000 --max <peak>

# Derive (read-only; REPEATABLE READ snapshot on postgres, WAL snapshot on sqlite,
# copied file set for mmap — copy coins.tbl BEFORE coins.dat):
coin-root-derive derive --db-url postgres://… --height 100180 --height 200008 …
```

The tool never writes to the store: postgres runs inside one `REPEATABLE READ, READ ONLY`
transaction; sqlite opens `mode=ro`; the mmap reader parses copied files with a
standalone parser (`roots/src/derive.rs`) that resolves entries through the bucket chains
exactly as the store's own `find` does.

## Sources

- SwiftSync (Ruben Somsen): https://gist.github.com/RubenSomsen/a61a37d14182ccd78760e477c78133cd
- FlyClient (Bünz, Kiffer, Luu, Zamani): MMR chain commitments.
- Grin TXO-MMR + spent bitmap: grin `doc/mmr.md`; bitmap accumulator (1024-bit chunks)
  in grin `core/src/core/pmmr`.
- Bitcoin `assumeutxo` trust discussion: bitcoin/bitcoin `doc/design/assumeutxo.md`.
