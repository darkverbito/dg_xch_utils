# The mmap backend vs. libbitcoin's store — design record

Grounded in the libbitcoin-database source (version3 branch, read 2026-08-14):
`primitives/slab_hash_table.hpp`, `primitives/record_hash_table.hpp`, `memory/memory_map.cpp`.

## What libbitcoin's store actually is

1. **Read-write memory mapping, everywhere.** Every file is `mmap(PROT_READ|PROT_WRITE,
   MAP_SHARED)`. Reads AND writes go through the map — no positioned read/write syscalls on the
   hot path. Growth reserves `size * (100 + expansion%) / 100`, resized with `mremap`
   (`MREMAP_MAYMOVE`) where available, else unmap/truncate/remap, serialized by a remap mutex
   with reader coalescing.

2. **Bucket-array header + chained slabs — never rehashed.** A table is a fixed bucket array
   (chain-head offsets) plus a body of entries `[key][next:8][value]`. Collisions are linked
   chains; insert is append-entry + link-at-head. Capacity pressure lengthens chains instead of
   triggering a rebuild.

3. **Record manager vs slab manager.** Fixed-size values use a record manager (index arithmetic,
   no per-entry length); variable-size values use a slab manager (byte offsets). Chia analogs:
   coin entries are fixed-size records; block bodies are slabs.

4. **The crash-ordering rule.** From `slab_hash_table.hpp`: *"If we run manager.sync() before
   the link() step then we ensure data can be lost but the hashtable is never corrupted."*
   Durability points: body/slab data first, then the 8-byte head link. Cross-file integrity is
   handled by flush ordering, not journaling — with ONE deliberate exception: a reorg's publish
   mutates existing entries in place (fork sweep + branch re-applies + peak flip), which flush
   ordering alone cannot make crash-atomic, so it is bracketed by `reorg.journal` (T0-4): the
   intent record lands durably before the first published mutation and comes off after the peak
   meta write; a journal found at open converges the store back to the fork (mod.rs module note).
   Everything else (per-block appends, links, spends under an unmoved peak) stays pure ordering.

## What this module implements today (the delta)

The current implementation serves the identical store trait contract and passes the shared
contract test, but is NOT yet the libbitcoin design:

- Lookup tables are open-addressing slot files accessed with positioned I/O (a syscall per
  probe) — not read-write mmap, not bucket+chain.
- Growth is rebuild-and-rename (libbitcoin never rehashes).
- Data logs are append-only via `write()`, mmap'd for reads only; in-place updates (spend
  state, status, main-chain flags) live in the slot files rather than written through a map.

## The convergence plan

Rework the internals to the studied design, keeping the trait impls and the contract test:

1. `MmapMut` region per file with reserve-ahead growth (expansion factor) behind a remap lock.
2. Tables become: fixed bucket-array header file + chain entries embedded in the data logs
   (`[key][next:8][payload]`); lookups traverse chains through the map; inserts append, sync,
   then link the bucket head (libbitcoin's ordering, verbatim).
3. Coin/spend/status updates write in place through the map (`msync` batched per block).
4. Fixed-size entries (coin index) move to record-manager arithmetic; bodies stay slabs.
5. Bench against SQLite on the Pi-profile rig (dm-delay + io.max) — the three-backend
   write-amplification comparison the storage go/no-go decision calls for.
