# CNI Bug-Fix History Sweep

A systematic disposition of chia-blockchain's bug-fix history against this node, per the
standing directive that parity work must mine the fix history — the fixes name the edge
cases. Prior efforts were targeted pickaxes (mempool caches, CHIA-4170 latency guards, ban
semantics, the sqlite IN-collapse); this is the first full walk.

- **Chia repo:** `/Users/grantcermak/Development/irulast/chia-blockchain`, 14,901 commits,
  HEAD `a95a2c6d6` (2.7.1 + 2 irulast-local commits: `526b21c79` custom RPC surface,
  `a95a2c6d6` sim genesis — both ours, not CNI fixes).
- **Our repo:** this worktree, branch `pr-parity-clean` at `e92bd80`.
- **Date of sweep:** 2026-08-24.

## Enumeration (auditable)

Post-1.8.2 candidates (the hard-fork era, 2023-06-28 → HEAD):

```
git log --oneline --no-merges 1.8.2..HEAD -- chia/full_node chia/consensus chia/types \
  chia/protocols chia/server chia/util/ints.py chia/util/streamable.py \
  chia/util/condition_tools.py chia/util/block_cache.py chia/util/merkle_set.py \
  chia/util/generator_tools.py chia/util/db_wrapper.py
```

→ **666 candidate commits.** Keyword filter
(`fix|bug|CHIA-[0-9]|incorrect|wrong|crash|hang|deadlock|race|leak|overflow|underflow|off.by|regression|revert|security|CVE|dos|ban`)
→ **237 fix-shaped.** The remaining 429 subjects were skimmed one by one; **47** touch a
validation rule / guard without a fix-shaped subject and were pulled into the set.

Pre-1.8.2 consensus-critical pickaxe (`blockchain.py`, `block_body_validation.py`,
`block_header_validation.py`, `mempool_manager.py`, `coin_store.py`, `weight_proof.py`,
`full_node.py`, `mempool.py`, `block_root_validation.py`, `get_block_challenge.py`,
`difficulty_adjustment.py`, `pot_iterations.py`, full history to 1.0):
**376 candidates → 56 fix-shaped.**

**Total dispositioned: 339 unique commits** (237 + 47 + 56, with `45edd24f9` appearing
in two lists). Non-core adjacent commits (feature/refactor neighbors named inside N/A
batch rows for context) are outside this count.

## Two structural facts that shape the dispositions

1. **Our port baseline is chia 2.7.1 FINAL.** `git tag --contains` confirms every 2026-05
   commit in the enumeration (including `560a4ab7a` and `a1b12d321`) is an ancestor of the
   `2.7.1` tag. Where our code is a cited line-for-line port of a 2.7.1 file (mempool,
   slots/FullNodeStore, fast-forward, weight proof, check_time_locks, rate-limit tables),
   the historical fix train for that file is inherited: the fix is *in* the source we
   mirrored. Those rows say COVERED (end-state port) and are verified **by-analogy** unless
   marked individually checked. This argument is NOT assumed blindly — the sweep found one
   place where the port cites a pre-fix shape (`481ccb305`, GAP 1 below), which is exactly
   why every 2025-2026 commit in ported areas got an individual grep.
2. **HF2/PoS2 is pre-activation.** Mainnet `HARD_FORK2_HEIGHT = 0xFFFF_FFFA` (far future);
   our constants pin the same value (`core/src/consensus/constants.rs:208`). Commits that
   implement or fix rules gated on HF2/PoS2/v2-plots are N/A **today** and are collected
   into one standing item (see Boundaries) rather than scattered as gaps.

Disposition legend: **COVERED** (our code guards it — cited), **DESIGN** (cannot occur in
our architecture — one-line why), **GAP** (applicable and unguarded), **UNCLEAR** (needs
deeper analysis), **N/A** (python-runtime / wallet-only / test-only / UI / packaging /
perf-only refactor with no rule content). `[ind]` = individually checked today against our
code or the chia diff; `[ana]` = honestly by-analogy to a verified cluster anchor.

---

## 1. Consensus / validation (blockchain, block body/header, fork handling)

| commit | date | what it fixed | disposition |
|---|---|---|---|
| 038f5e8fb | 2023-11-15 | deep reorgs: ForkInfo-based fork tracking, rollback at any depth | COVERED `[ind]` — store-backed fork walk, weight-only fork choice: `node/src/engine.rs` `fork_view`/`delta_from_store` (:1828, :1788), `node/tests/reorg_fork_view_from_store.rs` (the #193 fix landed on this branch) |
| 9385c4056 | 2024-03-20 | reorg from height 0 (fork point −1 edge in find_fork_point) | COVERED `[ana]` — same store-backed walk terminates at genesis; `node/tests/reorg.rs`, `reorg_fork_view_from_store.rs` |
| 14da93c58 | 2024-11-20 | fork_info left corrupted when a block fails validation mid-batch | DESIGN `[ind]` — no shared mutable fork_info; fork view is derived per block and the staged overlay is discarded on failure (`engine.rs::clear_staged_overlay`) |
| 777884f3b | 2025-06-27 | ForkInfo include_spends vs include_block divergence on fork peak / reward coins | COVERED `[ana]` — single delta-fold path (`engine.rs::fold_delta_into_view`); reward claims validated in `validate_reward_claims` (:1604); `node/tests/t070_body_coin_rules.rs` |
| bc065b5d5 | 2024-12-10 | validate caller-supplied fork_info in add_block | DESIGN `[ind]` — engine derives fork context internally; no caller-supplied fork_info to mistrust |
| 1a10172a7 | 2023-11-17 | get_block_generator looked up refs on wrong chain (fork vs main) | COVERED `[ind]` — `engine.rs::resolve_generator_refs` (:1197) resolves against the fork branch; `node/tests/generator_ref_resolution.rs`, `seed_generator_overlay_bounded.rs` |
| cf17f20cc | 2025-03-27 | CHIA-2052 compressed blocks (backref generator serving/creation) | COVERED `[ana]` — generator back-references resolved and served; `generator_ref_resolution.rs`, `core/tests/get_block_generator.rs` |
| 788db8bdf | 2023-12-18 | fix compressed-block ref serving | COVERED `[ana]` — same anchor as cf17f20cc |
| b4290a2f2 | 2024-07-09 | remove original (pre-hard-fork) block compression | N/A — removal of a mechanism we never carried |
| 260c7a363 | 2024-07-05 | simplify hard-fork consensus rules (consolidation) | COVERED `[ana]` — post-HF1 rule set ported; `node/tests/required_iters_real_blocks.rs`, `nightly_conformance.rs` validate against real mainnet blocks |
| 690338ef3 | 2024-10-22 | duplicate-output validation shape in validate_block_body | COVERED `[ana]` — body coin rules enforced + tested, `node/tests/t070_body_coin_rules.rs` |
| 4383b6f93 | 2024-09-26 | double-spend validation shape in validate_block_body | COVERED `[ana]` — same anchor |
| 6b8202cd6 | 2026-01-05 | AugmentedBlockchain assert → typed validation error | DESIGN `[ind]` — diff read: replaces python asserts; ours is Result-based throughout |
| 29826679a | 2024-12-12 | wrong blockchain param passed in prevalidate | DESIGN `[ind]` — one-line python aliasing bug; our call sites pass context explicitly |
| d5c33e721 | 2026-04-09 | replace assert with explicit check for unknown parent block | DESIGN `[ind]` — diff read; unknown parent is an error value here, never an assert (`engine.rs::prev_record`) |
| 493d36bb3 | 2023-10-24 | SerializedProgram.from_bytes must be a valid clvm structure | COVERED `[ana]` — our parser rejects malformed structure with `Err` (`core/src/clvm/parser.rs`); round-trip + fuzz coverage in `core/tests/clvm.rs` |
| f9b7ddcfc | 2024-12-06 | soft-fork 6 introduction (chia_rs 0.16) | COVERED `[ana]` — constants + CLVM flag schedule match 2.7.1 (contribution-bar rule: constants mirrored exactly); `node/tests/opcode_coverage.rs` |
| a83b24019 | 2024-05-30 | soft fork 5 | COVERED `[ana]` — same anchor |
| 6ceaec447 | 2024-11-06 | move signature validation into run_block_generator | DESIGN `[ana]` — our pipeline validates aggregate sigs in `verify_sig_window` (`engine.rs:903`); placement is impl detail |
| d1818faaf, 4f00f639f, 8d492e638, d9f7108e9, 06b2447f6, 03a41ac48, 36d7c0146, 0059b5e05, 9109d8926, 33e79e366, 1e8c999c0-era refactors | 2024 | ValidationState/prevalidation plumbing refactors | N/A — python API reshaping, no rule change (batch) |
| deb1d2e6c, f241889b4, ab53414e2, 70b182e9c | 2024-25 | test-infra (ConsensusMode, BlockTools) | N/A — test-only |
| dd7dc85f5 | 2024-12-12 | revert super set rule | COVERED `[ind]` — end-state RBF rules ported (see 6707961ab) |
| 6707961ab | 2025-03-06 | make the superset rule stricter (final form) | COVERED `[ind]` — `node/src/mempool.rs:1456-1459` superset + fee rules; `node/tests/t060_mempool.rs` |
| 80911d475 | 2024-11-08 | ignore ephemeral spends in mempool superset rule | COVERED `[ana]` — RBF end-state port |
| 43c3c6314 | 2025-11-06 | disallow generator block references after HF2 | N/A until HF2 (diff read: gated `prev_tx_height >= HARD_FORK2_HEIGHT`) — standing item |
| 224fb8238 | 2025-06-26 | require canonical CLVM serialization after 3.0 hard fork | N/A until HF2 (diff read: same gate) — standing item |
| 4e3646062 | 2026-01-15 | CHIA-3861 get_flags / pre_sp_tx_block for HF2 transition flags | N/A until HF2 (diff read: flag selection for the HF2 transition period) — standing item |
| d3005e17a | 2026-03-03 | only accept post-HF2 peers that signal HF2 support | N/A until HF2 — standing item |
| f637881f8 | 2023-03-21 | 1.7.1 fixes (blockchain.py 3-line + program) | COVERED `[ana]` — pre-1.8.2, superseded by 2.7.1 end-state port |
| 8833cc351 | 2022-03-31 | reorg fixes (blockchain + height map) | COVERED `[ana]` — superseded; store-backed height→hash, reorg tests |
| a3fd08592 | 2021-04-20 | double-blocks race in add_block | DESIGN `[ana]` — single-writer engine; duplicate add is idempotent (`node/tests/add_block.rs`) |
| 328e4cd27 | 2021-04-21 | blockchain timestamp condition handling | COVERED `[ana]` — `check_time_locks` port (`mempool.rs:233` "the exact port CNI runs") + header-validation timestamp rules (`core/src/consensus/block_header_validation.rs`) |
| 46d6fa417 | 2021-05-28 | header_hash var poisoned when begin_transaction throws | DESIGN `[ana]` — staging/confirm transaction model; no partial state on error (`node/tests/staging_commit_granularity.rs`) |
| 1c808b6c2 | 2021-05-17 | duplicate signage points | COVERED `[ana]` — slots port end-state (`slots.rs::have_newer_signage_point` :204) |
| 62c9a3c3e | 2021-10-04 | full-node-store race | DESIGN `[ana]` — `&mut` single-owner SlotState; no concurrent mutation |
| 69ca8039f | 2021-07-13 | sync with height 0 error | COVERED `[ana]` — `node/tests/t056_fast_sync_advances_from_empty.rs`, `genesis_wall_36.rs` |
| e7627c567 | 2021-04-12 | generator ref error message | N/A — message text |
| 876692631 | 2021-04-07 | timelord service error handling | N/A — timelord service, not the node |
| 45edd24f9 | 2024-01-17 | remove workaround for a ≤1.1.4 peer bug | N/A — informational; we never carried the workaround |

## 2. Mempool

Cluster anchor (verified today): our mempool is a cited port of 2.7.1
`mempool_manager.py` / `eligible_coin_spends.py` — FF+dedup processing
`node/src/mempool.rs:471-620`, RBF `:28-30`/`:1456-1459`, timelocks `:1137-1169`,
pending/conflict caches `:786-830`, assembly `:1695-1912`. `node/tests/t060_mempool.rs`,
`t062_mempool_dedup_ff.rs`, `t080_block_assembly.rs`.

| commit | date | what it fixed | disposition |
|---|---|---|---|
| 481ccb305 | 2026-03-20 | **mempool priority = fee per VIRTUAL cost** (cost + 500k/spend penalty) for eviction, min-fee floor, and block-assembly order | **GAP** `[ind]` — diff read: chia 2.7.1 orders by `fee_per_virtual_cost` (`chia/types/mempool_item.py::virtual_cost`, `mempool.py:438`); ours orders by raw `fee_per_cost` (`mempool.rs:707`, :970 comment cites the pre-fix `ORDER BY fee_per_cost`). Failure: many-spend low-value bundles CNI deprioritizes/evicts stay competitive here — divergent eviction under load and divergent block contents; the anti-spam intent of the penalty is absent. Policy/DoS class, not consensus-splitting. |
| b0bc56dff | 2026-04-16 | pending drain off-by-one delayed exact-match height asserts a block | COVERED `[ind]` — drain is `assert_height <= height` (`mempool.rs:2176`), matching post-fix `drain(new_peak.height)` with `<=` |
| d69c5214b | 2026-03-16 | PendingTxCache eviction crash on empty height buckets | DESIGN `[ind]` — no by-height bucket structure; flat map + scan-max eviction (`mempool.rs:1552-1560`) |
| fb2a93960 | 2023-09-26 | typo in PendingTxCache | DESIGN — same |
| 68e2afae2 | 2024-01-23 | mempool new_peak with non-transaction-block peak (get_tx_peak) | COVERED `[ind]` — `mempool.rs:773` "new_peak should always be the most recent *transaction* block" |
| 19dd1e0d5 | 2024-09-23 | deduct block overhead (quote cost) from mempool block budget | COVERED `[ind]` — `mempool.rs:839-846`, `:1738-1739` |
| 35ab75309 | 2024-02-05 | cap skipped items at 10 in block assembly | COVERED `[ind]` — `MAX_SKIPPED_ITEMS = 10`, break-on-tenth semantics (`mempool.rs:101-105`) |
| 8eb6d6aa5 | 2024-01-25 | look beyond items hitting the cost limit | COVERED `[ana]` — same assembly loop |
| 92cd6eeb7 | 2025-03-07 | count exceptions toward skipped_items | COVERED `[ana]` — `ProcessError::Failed` charges the skip budget |
| 7a643c6f3 | 2025-01-14 | wall-clock timeout adding transactions to blocks | COVERED `[ind]` — assembly `timeout` (chia `block_creation_timeout`) `mempool.rs:1735,1771` |
| 27bfa7e2a | 2025-05-27 | reject transactions that take >2s to validate | DESIGN `[ind]` — diff read: python wall-clock escape; our CLVM run is cost-capped at one block's cost (`mempool.rs:859`), deterministic bound |
| 177404998 | 2026-03-09 | plumb validation_timeout for slow machines | DESIGN — same |
| f9ac89009 | 2021-04-17 | RBF rules (PR #1971: superset, fee bump floor) | COVERED `[ind]` — `MEMPOOL_MIN_FEE_INCREASE` + can_replace port (`mempool.rs:28-30`, `:1456-1459`) |
| eca97fa4c | 2025-09-22 | can_replace simplification (timelock equality clauses) | COVERED `[ind]` — `:123`, `:696` timelock-equality inputs |
| de75f058d | 2024-06-06 | CHIP-0026 mempool updates (assert_before, expiring txs) | COVERED `[ind]` — impossible-constraint rejects + assert_before handling (`mempool.rs:1141-1160`), expiry sweep |
| 88310d614 | 2021-05-05 | mempool sorting + re-accept reverted pending txs on reorg | COVERED `[ind]` — fpc ordering + `revalidate_for_reorg` (`mempool.rs:2208-2222`) |
| 0a2707110 | 2023-02-16 | removing spend bundles on new peak | COVERED `[ana]` — end-state `new_peak` port (`:1989+`) |
| 2b2a1e864 | 2022-10-06 | mark re-added bundles as seen after reinit | COVERED `[ind]` — seen-cache semantics incl. reorg re-entry (`:2155`) |
| 1a84ee8db | 2022-01-25 | allow ephemeral coin + ASSERT_SECONDS_RELATIVE 0 | COVERED `[ana]` — chia_rs-parity `check_time_locks` port |
| d81198384 | 2021-08-18 | mempool TX cache cost accounting | COVERED `[ana]` — superseded by end-state caches (`ConflictTxCache` sizing `:37`, pending cap `:32`) |
| ac59ec52c | 2021-05-20 | pending-tx lock contention | DESIGN — `&mut` mempool, single owner |
| FF train: 0c8f0765d, f9206836b, d2e9df3b5, 5200310a2, c609d0bba, db51f43a9, d4aacea79, 6e542f051, ad755916a, de0b0d8c6, 6358bab4c, cfeeae1f9, fbfd7ca74, b1cc0a5b8, e91335eff, 889f4af35 | 2025-26 | singleton fast-forward hardening: validate-before-add, copy-not-mutate, state update only on success/inclusion, rebase on new peak, same-amount rule, dedup interaction, eviction | COVERED — end-state port of the post-train code. Spot-verified individually: state committed only on inclusion (`mempool.rs:479`) `[ind]`; amount-equality rejects (`core/src/consensus/fast_forward.rs:113-123,173`) `[ind]`; FF/dedup class split mirrored (`mempool.rs:471-472`) `[ind]`; rebase re-validation (`:620`) `[ind]`; rest `[ana]`. `node/tests/t062_mempool_dedup_ff.rs` |
| 8b2b830da, e129562e5, 46bc14b17, 73cb54789, 2b6ff0868, f7b7c66d2, 23ad9619e, 698b2dd75, 7c1a899fa, 0b68fd941, c5e7a0211, 6dceabe2d, 92fdb5848, e5b1e8528, 8141fe40e, 7245d47c7, b6d5afcdc, 531851458, 4d225bed3, 9273a6145, 2ff9c876b | 2023-25 | mempool/NPC plumbing refactors and perf | N/A — no rule content (batch; subjects + stats skimmed) |
| 1577f4aa3 | 2023-03-29 | mempool fee rate calculation | COVERED `[ana]` — fee estimator port, `node/src/fee_estimator.rs`, `node/tests/fee_estimator_hooks.rs` |
| d48e65e74 | 2023-01-02 | fee estimator re-created every block | COVERED `[ana]` — estimator is persistent state on `Mempool`, fed per block (`mempool.rs:2168`) |
| 3cecc27dd | 2024-07-08 | fee estimation fixes | COVERED `[ana]` — same anchor |
| 5522ab551 | 2025-10-31 | get_fee_estimate assertion | COVERED `[ana]` — same anchor |
| 169a2d9f8 | 2024-08-28 | request_fee_estimates | COVERED `[ana]` — served (`p2p/src/handlers.rs:29` RequestFeeEstimates) |
| 5a5cb2e58, c098ee916, b6596f9a9, 58c669164, 11fc35bbc | 2021-23 | estimator interface/rename/docs/log | N/A |
| 1a1209c06 | 2024-12-12 | remove 70% block fill limit when farming | COVERED `[ana]` — assembly fills to the cost budget, no fill-rate cap |
| cbf3d7f25, 1dc157e93 | 2024 | fill-rate bumps (superseded by 1a1209c06) | N/A — superseded config |
| 6821c48d8 | 2025-08-07 | explicit coin_puzzle_hash-index fallback in get_unspent_lineage_info | COVERED `[ana]` — bounded lineage lookup at admission (`mempool.rs:69-70`, `stores` lineage query) |
| 3901126c3 + 5e0a0df94 | 2025-05 | dedup improvements + amendment | COVERED `[ana]` — dedup end-state port (`mempool.rs:471-490`) |
| 33a02129b-adjacent 9ba6c0407 | 2025-03-04 | issue introduced by create_block_generator refactor #19207 | DESIGN `[ana]` — our assembly is not derived from that refactor; `producer_differential.rs` pins behavior |
| 3ac345e34 | 2025-01-09 | postprocessing priority-mutex ordering | DESIGN `[ana]` — python asyncio mutex fix; single-writer engine + `&mut` mempool |
| cdd3ed24b | 2024-11-22 | evict related BLS-cache entries on new peak | DESIGN `[ind]` — no cross-item BLS verification cache exists (grepped), nothing to go stale |
| 605e3b898, f9375a68f | 2024-06 | BLS cache class / Rust BLS cache | N/A — cache infrastructure we don't carry |
| 18134d68b, 7b18217ff | 2024-08 | validate_block_body / Mempool::size simplifications | N/A — refactor, no rule content |

## 3. Sync / peer selection

Our sync is a from-scratch design (chaser/driver, health-scored `peer_manager`, leases) —
chia's `SyncStore` bugs mostly cannot occur; the *edge cases they name* were checked
against our arms.

| commit | date | what it fixed | disposition |
|---|---|---|---|
| 81d11cd91 | 2026-04-07 | SyncStore: stale-peak flood evicts legit peak → assert on empty | DESIGN `[ind]` — diff read: chia's `peak_to_peer` flood map; we have no such structure — peer targets live in the bounded, health-scored `node/src/sync/peer_manager.rs` and selection returns `Option` |
| 5a1c15e20 | 2023-11-08 | peak height race (restore _peak_height on failure) | DESIGN `[ind]` — diff read; single-writer engine sets peak only after commit (`node/tests/t160_per_block_confirm.rs`) |
| 66d83db23 | 2023-11-09 | short-sync request bounds (peer peak below ours → invalid request) | DESIGN `[ind]` — diff read; our follow is weight-driven and a heavier-but-lower claim resolves through the backtrack recovery arm (`full-node/src/daemon.rs:3255-3292`) |
| fe9eb677a | 2024-12-04 | short_sync_backtrack reused wrong fork_info across the batch | COVERED `[ind]` — diff read; fork context derived per block from the store; `node/tests/backtrack.rs` |
| 3ab83f0ee | 2024-12-11 | use height_to_hash for the short-sync connect check | COVERED `[ind]` — `sync/mod.rs:1718` mirrors `height_to_hash(start-1)` (full_node.py:645-651 post-fix) |
| 483ec6077 | 2024-08-26 | long-sync cache warmup (missing block records in cache) | COVERED `[ind]` — `engine.rs::warm_cache_from_store` (:443), `preload_stage_context` (:789); `node/tests/long_sync_reland.rs` |
| 0ada9453a | 2023-10-10 | invalid sub-epoch-summaries cache recovery (height-to-hash file) | DESIGN `[ind]` — no persistent height-map/SES cache files; both derived from the store |
| f73564325 | 2025-03-17 | per-network height-to-hash / SES cache files | DESIGN — same |
| 2682be128 | 2023-09-08 | partial flush of height-to-hash cache file | DESIGN — same |
| d966f3f9e | 2023-08-30 | bad-peak cache (don't re-long-sync to a peak that failed) | DESIGN `[ana]` — failed syncs demote the serving peer (3-strike hysteresis, `peer_manager.rs`); no equivalent repeated-resync loop |
| 569a47c84 | 2024-11-08 | IndexError choosing from empty peers-with-peak in WP sync | DESIGN `[ind]` — diff read; lease acquisition returns `Option`, no `random.choice` |
| 12a089f5f | 2024-10-25 | pace block requests to stay under the peer's inbound rate limit | COVERED `[ind]` — outbound self-throttle paces frequency-capped sends against the PEER's budget (`core/src/protocols/mod.rs:711-714`); per-peer in-flight cap 16 (`peer_manager.rs:28`) |
| f2ef329cd | 2024-11-21 | sync timeouts scale with peer count | DESIGN `[ana]` — RTT-adaptive (RFC 6298 EWMA) per-peer timeouts instead of a fixed 30s |
| fe3799b1e | 2023-11-09 | request blocks in batches of 32 not 33 (serving cap off-by-one) | COVERED `[ind]` — the off-by-one is documented and matched (`p2p/src/handlers.rs:75-82`) |
| d387716f4 | 2023-10-12 | remove height-synchronized clearing of peak_to_peer | N/A — chia SyncStore internal |
| 2df931bf8 | 2024-10-25 | pipeline block validation in sync_from_fork_point | N/A — perf; our pipeline is its own design (`node/src/sync/prefetch.rs`, readahead tests) |
| 6701f77bc, 2a342493b, c21c2d673, 53d8e2c51, ce8b1d43c, 83192be41, 3a81506dc-era | 2023 | testnet10-specific sync/constants fixes | N/A — testnet10 dropped; we pin mainnet constants |
| d3f2dfaa1, da01437b0, 1ff7e09a5-era task refs | 2023-24 | logging / task-reference bookkeeping | N/A — python-runtime |
| 1bbc98717 | 2021-10-25 | subscription iteration bug in full_node.py | DESIGN `[ana]` — wallet-serving registry is from-scratch (`full-node/src/wallet.rs`) |
| c353048ef | 2021-09-07 | task queue buildup | DESIGN `[ana]` — every inbox in the daemon is bounded and drops excess |
| ca4881ff3 | 2022-04-20 | incorrect return in CoinStore.rollback_to_block | COVERED `[ana]` — from-scratch rollback with contract tests (`stores/tests/coin_store.rs`, reorg pipeline tests) |
| 0bf842cde | 2024-01-23 | undo BlockRecord cache insert when the DB write fails | DESIGN `[ind]` — staged overlay merges into the cache only on confirm (`staging_commit_granularity.rs`) |
| 9063e2981, a420d8ea0, 3b6f547d4, d2abc99f9, 8ebb31e11, c158330ae | 2023-24 | add_block_batch / SyncStore hygiene refactors | N/A — no rule content |

## 4. Wire protocol / server / rate limits

| commit | date | what it fixed | disposition |
|---|---|---|---|
| a1b12d321 | 2026-05-01 | **RATE_LIMITS_V3: window-based rate limits capability + ConfigureWindowSizes** | **GAP** `[ind]` — in tag 2.7.1; our `core/src/protocols/shared.rs` tops out at `RateLimitsV2 = 3` and `rate_limits.rs` implements v1+v2 only. Interop is safe (capability-negotiated fallback), but the tightened per-window budgets CNI 2.7.1 peers enforce between themselves don't protect us, and we can't negotiate the smaller windows. DoS-hardening parity. |
| b483e59f2 | 2026-04-30 | **CHIA-4203 list-limited deserialization** (1.2M coin_ids parsed ~6s on a Pi4 before the handler truncates) | COVERED `[ind]` — both halves. Memory: no pre-allocation from the count (#180, `serialize/src/lib.rs` + `core/tests/streamable_alloc_bomb.rs`). CPU (closed this sweep): `dg_xch_serialize::parse_vec_limited` mirrors chia `parse_list_limited` (truncate during decode, O(1) seek past the fixed-size tail), wired into the same four handlers chia wires (`core/src/protocols/wallet.rs::from_bytes_limited` × 4, `p2p/src/handlers.rs` dispatch arms, caps resolved from the trust policy in `full-node/src/daemon.rs`). Tests: `core/tests/list_limited_decode.rs`, `p2p/tests/t049_list_limited_decode.rs` (red→green over the loopback). |
| e57358aea | 2025-10-22 | minimum TLS 1.3 | COVERED `[ind]` — all three server-side rustls configs (p2p WS `servers/src/websocket/mod.rs::init`, RPC `servers/src/rpc/mod.rs::init`, 8555 `full-node/src/rpc.rs`) now build with `builder_with_protocol_versions(&[&TLS13])`, matching chia's server-side-only floor (`ssl_context_for_server`; chia's client context keeps defaults, as do ours). The verifier traits' `verify_tls12_signature` impls remain — rustls requires them on the trait, and they are unreachable with 1.2 disabled. Test: `servers/tests/tls13_floor.rs` (1.2-pinned client refused, 1.3 client + RSA-cert upgrade path still serve). |
| 0046a3a4e | 2026-03-26 | inbound TIMELORD connections only from localhost/exempt networks | **GAP (minor)** `[ind]` — diff read; we accept timelord handshakes and their VDF inboxes from any peer (`p2p/src/handlers.rs:898`, timelord inboxes in `daemon.rs:230-233`). Mitigations already present: inboxes bounded, and infusion results are assembled into a FullBlock that the engine fully validates (junk VDFs fail); residual risk is wasted verify CPU. |
| b1b68072a | 2026-03-17 | disconnect + short ban on unknown protocol message type | COVERED `[ind]` — the read loop now closes with a PROTOCOL_ERROR (1002) frame and enters the host into the timed ban list (`BanCause::InternalProtocolError`, chia's INTERNAL_PROTOCOL_ERROR_BAN_SECONDS) before the rate limiter or dispatch sees the message (`core/src/protocols/mod.rs` ReadStream). Prerequisite completeness (ede354c58 row) pins our recognized-code set equal to chia 2.7.1's, so a conforming CNI peer can never trip it. Test: `p2p/tests/t050_unknown_and_error_frames.rs` (evict + ban + refused reconnect; error frame 255 still tolerated). |
| 3461286e8 | 2025-12-15 | no rate limits / bans for exempt peer networks | **GAP (minor)** `[ind]` — no exempt-network bypass in `rate_limits.rs`; a trusted co-located peer (own farmer/wallet infra) can be throttled or banned by us. Operational, not security. |
| 9491c6ee3 + 04b9d010b | 2026-01-12 | deficit round robin across peers in TransactionQueue.pop | **GAP (documented)** `[ind]` — `full-node/src/tx_queue.rs:20-23` records the delta: per-peer share caps exist, cross-peer round-robin of validation order does not. Gossip fairness under adversarial load. |
| 0d706d591 | 2026-04-28 | nonced timed-out request handling in _send_message | COVERED `[ind]` — correlation-waiter table drops dead waiters so the table never leaks (`core/src/protocols/mod.rs:656-680`), reserved ids released on timeout (`:858`) |
| fc79ba0c3 | 2026-04-02 | active requests tracking | DESIGN `[ana]` — same waiter-table lifecycle |
| c4e714eea | 2024-12-11 | outbound rate limiter must not drop response messages | COVERED `[ind]` — `Unlimited` serve types admitted instantly by the outbound self-throttle; responses never dropped (`protocols/mod.rs:718-723`) |
| d2f6750b0 | 2026-03-31 | close connections on bad data | COVERED `[ind]` — read-loop enforces inbound limits and closes on violation (`handlers.rs:1850-1853` comment + ban registry `protocols/mod.rs:706-710`); `node/tests/silent_fallback_ban.rs` |
| 540747fae | 2023-09-14 | 10-minute ban for consensus-rule violations | COVERED `[ind]` — `BanCause` + timed ban registry + immediate eviction (`handlers.rs:757-761`) — the prior ban-semantics pickaxe |
| cb82283cf | 2026-03-24 | DON'T ban peers for txs that fail mempool checks | DESIGN `[ind]` — mempool failures ack FAILED and never reach the ban path (`full-node/src/tx_admission.rs:37-61`); ban is reserved for protocol violations |
| 480a060f2 | 2025-10-17 | ban when a re-announced tx's cost/fee mismatch our validated item | COVERED `[ind]` — `TransactionAnnounceAction::Ban` (`handlers.rs:60-63`) |
| bc7b6ebc5 + 6c570a5ce | 2026-02 | tolerate the quote-cost mismatch from ≤2.4.3 peers (avoid banning legit old nodes) | COVERED `[ind]` — exact tolerance ported: `daemon.rs:423-436` (`QUOTE_BYTES*COST_PER_BYTE + QUOTE_EXECUTION_COST`) |
| 8f5a7e0a0 | 2025-11-15 | ban zero-cost NewTransaction announcements | COVERED `[ind]` — same Ban arm (`handlers.rs:60`) |
| bae4615a5 | 2025-10-29 | NewTransaction fee/cost must match validated values end-to-end | COVERED `[ind]` — announces carry validated cost (`tx_admission.rs:158-177`); pulls carry advertised fee/cost for queue ordering (`daemon.rs:525-533`) |
| 3b8a33da9 | 2026-03-20 | ignore unsolicited RespondTransaction | COVERED `[ind]` — pending-pull set consumed on receipt; unsolicited bodies dropped before validation (`daemon.rs:494-505`) |
| 5be65623b | 2026-03-27 | in-flight dedup when requesting advertised txs | COVERED `[ind]` — duplicate in-flight pull → Ignore (`handlers.rs:55-57`) |
| 1e0bc4d15 | 2025-12-04 | answer RequestMempoolTransactions with advertisements, not bundles | COVERED `[ind]` — serves up to 100 resident items as `NewTransaction` (`handlers.rs:184`) |
| 8ab0975bd | 2022-10-19 | serve the 100 highest-fpc items in request_mempool_transactions | COVERED `[ana]` — same serve path |
| ca789cd86, 3a53b86db, 45d8ecd9d, e17e65707, 25154a104, 075adb251 | 2024-26 | logging improvements | N/A |
| f39392912 | 2026-04-09 | outbound handshake timeout in start_client | COVERED `[ind]` — `p2p/src/sessions/mod.rs:216` handshake_timeout |
| f1556b4dc | 2026-02-25 | inbound message typechecking | DESIGN `[ana]` — per-type typed decode; a mismatched payload is a decode error, unhandled types never dispatch |
| 0a2536f80 | 2026-02-27 | accept_inbound_connections hygiene | DESIGN `[ind]` — inbound cap + duplicate-endpoint reserve with tests (`p2p/src/peer.rs:39`, `:211-227`) |
| 88213ea88, b6929316d, a332befda, c3d9dbcf2, cbd7a5c87, 338a13ae3, 000e2a74d, 78b7904fd, cd78dbafd | 2023-26 | read-loop / TransactionQueue plumbing refactors | N/A — python-shape only |
| 811ecc9b2 | 2025-12-05 | order peer tx queue by fee-per-cost | COVERED `[ind]` — capped lane orders by advertised fpc (`daemon.rs:531-533`) |
| de04bacd6 + 20f86e15d | 2026-01 | forward wallet-submitted txs to trusted peers first | N/A — forwarding-order policy; our announce drain broadcasts uniformly (excluding origin) |
| d23ea4df3 | 2023-06-07 | store protocol version for incoming connections | COVERED `[ana]` — inbound handshake records negotiated version (`servers/src/websocket/harvester/handshake.rs:29-33`) |
| ede354c58 | 2023-07-13 | introduce the `error` protocol message | COVERED `[ind]` — resolved from UNCLEAR. Verified sender side: CNI ≥ protocol 0.0.35 (`ws_connection.py error_response_version`) answers a handler `ApiError` with an `error` (255) frame, and its receiver decodes + WARN-logs it, no ban (`_api_call`:490-493). Ours now mirrors the receive side: code 255 modeled (`ProtocolMessageTypes::Error`), body as `shared::ErrorMessage` (byte-parity test `core/tests/protocol_message_codes.rs`), tolerant dispatch arm logs and keeps the connection (`p2p/tests/t050_unknown_and_error_frames.rs`). Deliberate delta: we do not EMIT Error frames (our rejects are the typed Reject* bodies chia also accepts). Codes 108-111 (solution_response/solve/partial_proofs/configure_window_sizes) modeled with chia's exact rate-limit rows so the unknown-type set equals chia's. |
| e57358aea dup row omitted; TLS above | | | |
| 11971e40c | 2026-04-06 | compact-VDF request handling hardening | COVERED `[ana]` — port of current full_node.py guards (`node/src/compact_vdf.rs:108-166`) |
| f8d7efa7b | 2026-02-20 | reject unsolicited RespondCompactVDF | COVERED `[ind]` — consume path is pull-correlated; solicits ledgered with cap+TTL (`compact_vdf.rs::SolicitLedger` :279-332) |
| 6e37b7474 | 2022-01-28 | change compact block protocol | COVERED `[ana]` — superseded by end-state port |
| e6dbaef4c | 2024-07-05 | bluebox uncompact bucket distribution | N/A — timelord-side scheduling |
| a9e8e4b91 | 2024-06-04 | split capabilities per service | DESIGN `[ana]` — we advertise full-node caps only |
| a19365ac4, 0a084139b, 9af0b7c97-era, 7a08fcaff, 362414c22, ca85e485e, 508087251, d0c00937b-era | 2023-26 | streamable feature work (enums, dicts, ordering hacks) | N/A — wallet/serialization features we don't consume; our wire types round-trip-tested (`node/tests/wire_roundtrip.rs`) |
| ede354c58 handled above; d29d31692 | 2023-11-29 | typo in deep-reorg perf fix | N/A — in code we don't share |
| 25154a104 above; 206b5c518 | 2023-11-08 | wallet node discovery | N/A — wallet |
| 3584b3ba7, 3a1f70a9b, ef98949a3, 82f3b0605, dafd8b41d, 8f4b0b6a-era peers | 2023-24 | introducer/peer-list config features | N/A — feature work; our discovery arm covered by t040 handler tests |
| 12948b837 | 2025-08-13 | run compute-heavy RPC jobs in a thread pool | UNCLEAR `[ind]` — only the weight-proof verify is `spawn_blocking` (`daemon.rs:3915-3917`); CLVM-running RPC endpoints (`get_block_spends`-class) may execute on runtime workers and add tail latency under RPC load. Resolve: audit `full-node/src/rpc.rs` CLVM call sites; offload if hot. Latency, not correctness. |
| a10df3b61 | 2025-07-21 | remove exception catching from node RPC | N/A — python error-shape |
| 387d8073a | 2025-07-07 | drop unserializable spends, not the whole request | DESIGN `[ana]` — serialization of stored types is infallible in Rust |

## 5. Producer / farmer path

| commit | date | what it fixed | disposition |
|---|---|---|---|
| 560a4ab7a | 2026-05-19 | revert SP tri-state (INVALID_VDF ban) back to bool + future-SP cache | COVERED `[ind]` — in tag 2.7.1; ours matches the post-revert shape: `new_signage_point → bool` (`slots.rs:625`), VDF-fail caches to future SPs (`:765,:778`) |
| 216331028 | 2026-04-29 | CHIA-4170/4168 unfinished-block PoS handling latency | COVERED `[ind]` — the prior pickaxe; `daemon.rs:988` cites post-CHIA-4170 guard; `node/tests/unfinished_body.rs` |
| 64a52b9a1 | 2026-02-23 | harden full node store | COVERED `[ana]` — slots port is of 2.7.1 end-state (bounded future caches `slots.rs:49-73`) |
| c0c64d7e0 | 2026-03-19 | early proof-of-space check | COVERED `[ana]` — early cheap PoSpace verify on the read path (`daemon.rs:877`) |
| dbd67001f | 2025-09-16 | SP lookup at genesis | COVERED `[ind]` — genesis-challenge arms (`slots.rs:142-148`, `:168-172`) |
| 8e3f9db89 | 2023-11-22 | forward only 4 most recent cached SPs | COVERED `[ana]` — slots end-state port |
| a7724e038 | 2024-03-13 | tighten duplicate-UnfinishedBlock request check | COVERED `[ana]` — announce dedup + request tracking (`daemon.rs:226-228` UnfinishedCache) |
| 41b9d0266 | 2024-01-05 | evict seen_unfinished_blocks continuously | COVERED `[ind]` — bounded caches proven by `node/tests/cache_bounds.rs` |
| bdfffca5a | 2024-01-26 | UnfinishedBlock handling improvements | COVERED `[ana]` — end-state port + unfinished tests |
| 9376f9944, 52614cb24 | 2026-04 | avoid recomputing tx peak in declare_proof_of_space | N/A — perf-only; ours computes prev-tx context once (`engine.rs::prev_tx_height_for`) |
| 38d2d9f2d | 2025-11-14 | pass the correct previous transaction block height | COVERED `[ana]` — `prev_tx_height_for` (`engine.rs:859`), `node/tests/declare_proof_of_space.rs` |
| 6df066fa0 | 2025-09-04 | incorrect assert in create_block_generator2 | DESIGN `[ana]` — no asserts; Result paths; `node/tests/producer_differential.rs` |
| 889f4af35 | 2025-04-08 | (also in FF train) assembly dedups on FF'd spend data | COVERED `[ana]` — assembly consumes processed FF'd spends |
| 7aa76bc11 | 2025-04-02 | compute block cost at creation | COVERED `[ana]` — generator cost computed directly; `node/tests/cost_wall_9179161.rs` |
| 6824b11ba | 2025-04-24 | synchronous block generator creation | N/A — python execution model |
| 73dbe1e5e | 2026-03-02 | default block-creation impl toggle | N/A — config default; our assembly is the create_block_generator2-shape |
| 88d3fefa6 | 2025-04-21 | validate the debug-RPC block generator | N/A — debug RPC |
| afd5427ce | 2025-05-30 | move SP broadcast | N/A — ordering refactor |
| 9134f43bc | 2024-05-29 | unfinished block in state-change event | N/A — UI/metrics event |
| 9f04cd351 | 2022-11-19 | farmer response timer for SP 0 | N/A — farmer-service UI timer |
| 3519265e3, 3a53b86db | 2025-26 | overflow-block logging | N/A |
| 4ec439f78, 320d88159, 0ae3b2823, c5ca0978f, 85d14f561, 15fa5f376, 23288e632d-era | 2023-24 | harvester/farmer protocol features (third-party harvesters, compression UI) | N/A — farmer/harvester service features outside the node |
| 12948b837 listed in §4 | | | |

## 6. Stores

Our stores (sqlite/mmap/postgres behind `stores/src/traits.rs`) are from-scratch with
contract tests; chia's aiosqlite bugs largely cannot occur.

| commit | date | what it fixed | disposition |
|---|---|---|---|
| cfc9429bc | 2026-02-23 | dangling SAVEPOINTs on asyncio cancellation | DESIGN `[ind]` — no cancellable-await-inside-transaction pattern; writes are synchronous transactions committed by the single writer (`node/tests/t160_per_block_confirm.rs`, `staging_commit_granularity.rs`) |
| c9e6c9d5c | 2023-08-11 | bug in a chia BlockStore optimization patch | N/A — bug in a patch we never carried |
| 31f48b915 | 2025-01-08 | sqlite cached_statements workaround | N/A — aiosqlite-specific |
| 2d28d62ab | 2023-07-14 | sqlite host parameter limit (999-var IN) | COVERED `[ind]` — the prior IN-collapse pickaxe: point-gets over PK instead of dynamic `IN` (`stores/src/sqlite/coin.rs:90-92`) |
| df52ee6a5 | 2024-12-03 | add in_main_chain=1 to height queries | COVERED `[ana]` — main-chain flag first-class in our schema; `stores/tests/coin_store.rs`, `node/tests/reorg_pipeline_indexes.rs` |
| 59583e75e, 7e265e3da | 2025-02 | contains_block / main-chain checks via height_to_hash | COVERED `[ana]` — store-backed height→hash is the main-chain oracle (engine fork walk) |
| 242ef4f5a, cc00bf55b, 3e199c130, 39fd1b688, ab61a0dc8, 91e4c16d2, 696986701, b32128949, b3fe7c470, 1d8d47605, aacdc0318, ed786e2f6, 3caa0754c, 442bad9f2, 581aa707a, 93d967525, 6d60bfd58, 1910c3891 | 2021-26 | CoinStore/BlockStore perf, sqlite pragmas, DB-wrapper features, schema-v1 drop | N/A — chia-storage-specific; our schema/perf tracked by our own store contract + perf tests (batch) |
| 46d6fa417, ca4881ff3, 0bf842cde in §1/§3 | | | |

## 7. Weight proof

| commit | date | what it fixed | disposition |
|---|---|---|---|
| 2fcaf4520 | 2026-04-02 | SEC-615: overflow block at segment index 0 → python negative-index read | COVERED `[ind]` — exact guard ported: `if idx < 1 { return Ok(None) }` (`weight-proof/src/lib.rs:1113-1115`); plus `weight-proof/tests/weight_proof_parity.rs` |
| 0ada9453a | §3 | | |
| 4b2f9f0c7 | 2024-06-21 | revert rust WP types (python-side breakage) | N/A — python packaging of chia_rs types |
| dcd296975 | 2025-12-02 | height-agnostic PoS validation in WPs (v1 phase-out / v2 gating) | N/A until HF2 `[ind]` — diff read: gates only the v1-phase-out/v2 checks, both far-future on mainnet — standing item |
| 3f0d5c070 | 2024-04-03 | rust types for WP (later reverted) | N/A |
| f6e08ba7e | 2024-12-11 | track weight proof tasks | N/A — python task bookkeeping; our WP verify is one `spawn_blocking` (`daemon.rs:3915`) |
| 569a47c84 | §3 | | |
| aefefb416 | 2025-12-15 | WEIGHT_PROOF_RECENT_BLOCKS in tests | N/A — test |

## 8. Wallet-protocol serving (full node side)

Anchor: from-scratch bounded registry (`full-node/src/wallet.rs` — hard caps on
subscribers/items/channel, TrustPolicy tiers) + `full-node/tests/puzzle_state.rs`.

| commit | date | what it fixed | disposition |
|---|---|---|---|
| d7594688e | 2026-02-17 | asserts crash serving reorged wallet requests | DESIGN `[ind]` — diff read (2 asserts removed); ours is Result-based, with explicit reorg guards on the serving reads (`daemon.rs:1326`, `:1472`) |
| 7ddc900bf | 2026-02-27 | return RejectAdditionsRequest instead of raising | DESIGN `[ana]` — reject messages are values, not exceptions |
| 7fa27cbad | 2024-05-01 | duplicate coins in RequestPuzzleState reply | COVERED `[ana]` — store-distinct reads; `puzzle_state.rs` |
| 7e1af564b | 2026-03-16 | register_for_coin_updates hygiene | COVERED `[ana]` — capped registration (`wallet.rs:375`) |
| f85e68ee2 | 2023-06-21 | duplicates in register_interest_in_puzzle_hash reply | COVERED `[ana]` — same |
| c6a6c0bbc | 2024-12-18 | request_block_headers returned wrong filter | COVERED `[ana]` — filter from the ported `core/src/consensus/block_filter.rs`; serving in `handlers.rs` |
| 11d0dd9ae, 922892e2e, 8c38c5fb0, 12d1176d7, 816462d57 | 2024-26 | transactions-filter computation clarifications/perf | COVERED `[ana]` / N/A-perf — same filter anchor |
| b2498d291 | 2023-06-16 | max_height in get_coin_states_by_ids | COVERED `[ana]` — bounded state queries (`stores/src/traits.rs`) |
| ee024b873, f742e4e51, 0cafc8319, 23e88632d, e3beca2d0, 7fa27cbad | 2023-24 | subscription/batching protocol features | N/A-feature / COVERED `[ana]` — v2 subscription surface implemented and tested (`puzzle_state.rs`, `full-node/tests/rpc.rs`) |
| 61703ce09, ad8847618, 4fddb5cdf, 5766a8d36, 8129b4147, c25eeb08f, 55165b116, 0a2692468, ad46ee230, 3118c1802, a457fa28e, d26303cb0, 326b1084c, 03861b260, d17-era wallet | 2021-26 | wallet-side (client) fixes | N/A — we ship no wallet client (batch) |

## 9. Remaining N/A (lint / typing / typos / packaging / test-only)

fcdb8e492, a803dc0fa, 604fc1feb, 2808ee244, 73e4a5bbe, 6084112d4, 6050235bd, 631215624,
09f48746d, 743cfdfff, 3d916bd25, 6b678168b, 57d1974ef, 39dc20156, 7b0bea404, 3a0efbd46,
536a96241, e4ccb8024, 6c2c13a2d, bbe507744, d678131de, 74536ba78, 1654de1a4, c4708072f,
bea492874, 5fe03a069, bbd032e1c, 890c7d375, a2490e076, 656d7d94d, b1e7b8d0a, 073dc941f,
44b4d69ad, cd2f9d9d0, 925bb2b64, 370444a7f, 2335beff1, 5e4c1a1f6, bdcf25977, 7fda9596a,
376701290, b1dd36325, d928ac0be, 45edd24f9 (dup-listed §1), 9179353dd, 6354ea3ae,
5552485ff (reorg perf optimization — code we don't share). All subject-verified as
lint/typing/logging/test/packaging with no rule content; `b1dd36325`/`d928ac0be` are
python `uint64`-construction discipline that Rust's type system enforces. Also N/A:
a4da91a86 (DataLayer plugin), a5be6ebd5 (make_spend wallet util), a745cb034 (drop
testnet10), d1844b6b1 (BlockGenerator type update), f4ff9d621 (cancel_task_safe dedup).

## 10. HF2 / PoS2 pre-activation bucket (standing item, not today-gaps)

dfc750b15, d5d886b49, e4961d9dd, 52cd1086a, bcdc0c6f8, b8c0e7712, ece6de2af, 6c8ccd246,
b7723252d, df45f393a, 9d8c170b1, 9289a05f1, 794cce782, 18fd65d13, 3491f1aea, ecb54a587,
6bd9360f5, 5492f82db, b1c9beed3, 64b2c8d03, 428e26cf5, 8beb3ee8d, 3a43a87ec, 8f8d27aca,
43c3c6314, 224fb8238, 4e3646062, d3005e17a, dcd296975, 54e915b72, 5664516dd-era chia_rs
bumps. All gated on `HARD_FORK2_HEIGHT` / v2-plot constants that are far-future on
mainnet (our constants pin the same sentinel). **Standing item:** before HF2 activates,
this entire bucket must be swept again as implementation work (block-ref ban, canonical
CLVM, get_flags transition, v2 PoS + iterations v2, v1 phase-out, HF2 peer gating,
RATE_LIMITS_V3 interplay).

---

## Counts

Counts are mechanical over the 339-commit core set (ledger reconciled hash-by-hash
against the three enumeration lists; zero unassigned, zero extra):

| disposition | count |
|---|---|
| COVERED | 103 |
| COVERED-BY-DESIGN | 39 |
| GAP | 9 (8 distinct findings; `04b9d010b` is the cherry-pick of `9491c6ee3`) |
| UNCLEAR | 2 |
| N/A (incl. the 14-commit HF2/PoS2 standing bucket) | 186 |
| **Total** | **339** |

## Ranked gap list

Ranking: consensus-split risk > node-stranding > DoS > perf. **No consensus-split or
node-stranding gaps were found** — every validation-rule fix in the range is either
ported, tested, or architecturally excluded; the store-backed fork walk (#193) closed the
one stranding-class divergence before this sweep.

1. **`481ccb305` — mempool fee-per-virtual-cost priority (DoS/policy).** CNI 2.7.1
   penalizes each spend 500k virtual cost in eviction, min-fee, and block-assembly order;
   we use raw fee-per-cost. Under a many-spend spam load our mempool retains and mines
   what CNI evicts. Fix is contained: a `virtual_cost` accessor + swapping the three
   ordering sites in `node/src/mempool.rs`.
2. **`b483e59f2` — CHIA-4203 list-limited deserialization (DoS/CPU, Pi-4 floor).** CLOSED —
   decode-time list caps on the four wallet-protocol request handlers (see the §4 row).
3. **`a1b12d321` — RATE_LIMITS_V3 window-based limits (DoS-hardening parity).**
   Capability, table, and `ConfigureWindowSizes` message are absent. Interop-safe today;
   schedule with the HF2 bucket (same protocol-era).
4. **`0046a3a4e` — inbound timelord gating (DoS).** Accept TIMELORD handshakes only from
   localhost/exempt networks. Mitigated by bounded inboxes + full validation; still free
   verify-CPU for a remote attacker.
5. **`9491c6ee3` — cross-peer deficit round robin in the tx queue (fairness/DoS,
   documented delta).** Per-peer share caps exist; validation order can still be dominated
   by one peer's high-fpc stream.
6. **`3461286e8` — exempt-network rate-limit bypass (operational).** Own-infra peers can
   be throttled/banned by us.
7. **`e57358aea` — TLS 1.3 minimum (hardening parity).** CLOSED — server configs pinned to
   TLS 1.3 (see the §4 row).
8. **`b1b68072a` — close on unknown message type (hygiene).** CLOSED — disconnect + short
   host ban in the read loop (see the §4 row).

UNCLEAR (2): `ede354c58` (tolerant parse for the `error` protocol message — verify CNI
sender behavior), `12948b837` (audit RPC CLVM call sites for runtime-thread stalls).

## Sweep boundaries (honest statement)

- **Date range:** post-1.8.2 (2023-06-28) → HEAD for the path set above; full history
  (to 1.0) for the twelve consensus-critical files listed in Enumeration. Pre-1.8.2
  history of *other* paths (server, protocols, stores, non-core full_node modules) was
  NOT swept — those subsystems are from-scratch here and their chia-era bugs are
  implementation-specific; the residual risk is edge cases named only in old commits to
  files outside the pickaxe set.
- **Paths excluded:** `chia/wallet` (except where a fix-shaped commit in the included
  paths touched wallet serving), `chia/cmds`, `chia/daemon`, `chia/rpc` (server plumbing;
  RPC fixes surfaced via full_node paths were dispositioned), `chia/timelord`,
  `chia/harvester`, `chia/farmer`, `chia/introducer`, `chia/seeder`, `chia/data_layer`,
  `chia/plotting`, `chia/pools` — we do not ship those services.
- **chia_rs is out of scope.** Consensus fixes that landed in chia_rs (not
  chia-blockchain) are not enumerated here. Our CLVM/conditions/validation core mirrors
  chia_rs 0.42.1 with differential tests, but a dedicated chia_rs fix-history sweep is a
  separate (recommended) exercise.
- **Lighter pass, as scale note allows:** subsystems 6–9 (stores, weight-proof serving,
  wallet-serving, N/A batches) received terser dispositions; every commit hash still
  appears above. Batched N/A rows were verified by subject + `--stat` skim, not full
  diff reads. All GAP and UNCLEAR rows had their chia diffs read individually.
- **By-analogy honesty:** rows marked `[ana]` lean on the verified 2.7.1-end-state port
  argument for their file; the one place that argument failed under individual checking
  (`481ccb305`) is GAP 1, which is precisely the residual risk class for `[ana]` rows.
  Spot-check density was highest (every row individual) for 2025–2026 commits in ported
  areas.
