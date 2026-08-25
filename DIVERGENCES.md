# DIVERGENCES — index of every characterized divergence from chia consensus/behavior

One entry per numbered divergence found while syncing this node against real Chia mainnet and
porting chia-blockchain's test corpus. This is the **index**; the long-form ledger (full
root-cause, chia references, evidence tests per entry) lives at `core/tests/DIVERGENCES.md` on
the `full-node` history branch (this fold carries the rendered `core/tests/DIVERGENCES.pdf`).
`pr-parity-clean` is a curated fold of that history: the per-divergence commits cited below are
the `full-node`-branch SHAs where each fix landed; their content is folded into this branch's
squashed base commits.

Numbering: DIVERGENCE-1 through DIVERGENCE-50 were minted in order of discovery.
**DIVERGENCE-44, -45, -46 were never minted** (the Tier-2 campaign jumped from 43 to 47); no
entries are missing.

Statuses: **FIXED** (closed, with regression tests), **DOCUMENTED-INTENTIONAL** (a deliberate,
recorded delta from chia), **OPEN** (known, not yet closed).

## Numbered divergences

| # | What | Surfaced | Fix commit(s) | Status |
|---|---|---|---|---|
| 1 | CLVM arithmetic ops decoded atoms as unsigned, not signed two's-complement | harvest of `clvm/test_program.py` | `4762955` | FIXED |
| 2 | `Program::uncurry` errored on a non-curried program (chia returns `(self, nil)`) | harvest of `test_program.py::test_uncurry_*` | `bbe904e` | FIXED |
| 3 | Mainnet block generator rejected at height 9,138,874 (`InvalidBlockSolution`): zero-arg REMARK rejected + `coinid` op cost under-counted by 320 | live mainnet stall | `0c7d467` (narrowed by `4762955`) | FIXED |
| 4 | Negative / out-of-range height/seconds condition args not saturated per chia's per-opcode no-op/fail table | harvest of `test_conditions.py::test_condition` | `6554c3e` | FIXED |
| 5 | Block-generator ref-list resolution not wired (fail-closed on any ref) | harvest of `test_get_block_generator.py` | `0556e02` | FIXED |
| 6 | BLS G1 infinity pubkey not rejected in AGG_SIG conditions | harvest | `831273f` | FIXED |
| 7 | `Option` presence tag > 1 accepted (chia rejects) | streamable harvest | `4fc1506` | FIXED |
| 8 | Trailing bytes after a decoded value not rejected | streamable harvest | `4fc1506` | FIXED |
| 9 | No standalone block-height-map component | architecture audit | recorded `01b3337` | DOCUMENTED-INTENTIONAL (subsumed by the block store; equivalent queries tested at the store) |
| 10 | `transactions_generator_root` tree-hashed the generator instead of hashing raw bytes (`InvalidTransactionsGeneratorHash`) | live mainnet, post-DIV-3 batch | `c049bbf` | FIXED |
| 11 | Additions/removals merkle-set root didn't match chia's radix-tree node scheme (`BadAdditionRoot`) | flipping the DIV-10 e2e test to real foliage | `0d4d7a0` | FIXED |
| 12 | Fabricated `required_iters == 0` at the checkpoint boundary poisoned the difficulty retarget | live mainnet | `307d5d7` | FIXED |
| 13 | Anchor body-confirm fabricated `sub_slot_iters_starting` into the block record (sp-interval bound poisoned) | live tip-follow | `a96ff91` (tail closed live at epoch boundary 9,146,880 — `b67072d`) | FIXED |
| 14 | Time-lock conditions validated against the block's OWN height/timestamp instead of the previous transaction block's | audit + harvest | `4296559` | FIXED |
| 15 | Full header validation engaged before the checkpoint window was slot-warm (`INVALID_ICC_VDF` at checkpoint+2) | live sync | `fb161bc` (gate corrected to the counted form in `15e5f57`) | FIXED |
| 16 | Consensus CLVM execution rejected unknown operators (`InvalidBlockSolution` at mainnet 9,143,859) | live tip-follow | `15e5f57` | FIXED |
| 17 | `op_multiply` overcharged the running product held as i128 (`InvalidBlockCost` +456 at mainnet 9,143,994) | live tip-follow | `631fc9f`; SF9 flag-set companion (`SIMPLE_GENERATOR\|CANONICAL_INTS\|LIMIT_SPENDS`) ported in `4ec10f4` | FIXED |
| 18 | Body validation rejected empty transaction blocks (mainnet 9,144,185) | live tip-follow | `3b01142` | FIXED |
| 19 | Genesis confirmed with placeholder `required_iters = 0`, poisoning the era's challenge-block walk (mainnet height 36) | genesis long-sync | `ce306c2` | FIXED |
| 20 | Aggregate verification omitted bundle-level `AGG_SIG_UNSAFE` pairs (`BadAggregateSignature` at mainnet 2,272,201) | genesis long-sync | `a223f68` | FIXED |
| 21 | Consensus enforced the mempool-only 1024-announcement cap (`InvalidCondition` at mainnet 4,693,324) | genesis long-sync | `931c7b9` | FIXED |
| 22 | `bigint_to_bytes` corrupted the sign-pad byte for 4k-data-byte positives (era-c wall 5,494,140, `AssertAnnounceConsumedFailed`) | genesis long-sync | `31abc37` | FIXED |
| 23 | CLVM flag ladder keyed on prev-tx-block height, not the block's own (era-c wall 5,496,002) | genesis long-sync | `15e231f` | FIXED |
| 24 | CLVM `point_add`/`pubkey_for_exp`: invalid-point and cost-timing semantics diverged from clvmr 0.17.7 | blst dependency port characterization (`55ac275`) | `11e8862` (the three chia-semantics tests run un-ignored in `core/tests/bls_ops_differential.rs` on this branch; the long-form ledger's "OPEN" summary line predates the fix) | FIXED |
| 25 | Pospace BitReader under-counted the 64-bit field span for k>32 metadata, rejecting valid proofs (`INVALID_POSPACE`, live mainnet 6,281,496) | live | `ce06090` | FIXED |
| 26 | Live add-block path skipped chia's coin-store body validation (rules 3, 5, 10–21) | Tier-0 audit | `74b19c0` (suite: `node/tests/t070_body_coin_rules.rs`) | FIXED |
| 27 | Unfinished blocks relayed after header-only validation — the generator never ran (live 600s GENERATOR_RUNTIME_ERROR ban) | live, 2026-08-20 | `d6df83a` | FIXED |
| 28 | Peak selection ignored weight: one global height-max slot, unverified, never retracted | Tier-0 audit | `6440c48` (`full-node/src/peak_book.rs`) | FIXED |
| 29 | Reorg was not atomic: N store transactions where chia holds one | Tier-0 audit | `c060796` | FIXED |
| 30 | Mempool timelock admission: heights off by one; seconds/relative locks never checked | Tier-0 audit | `26516e3` | FIXED |
| 31 | `request_blocks` serving: no range cap, headers-only flag ignored, unsolicited block replies tolerated | Tier-1 p2p audit | `69f3656` | FIXED |
| 32 | Inbound rate limiting advertised v2 but enforced a v1-subset; no outbound self-throttle | Tier-1 p2p audit | `6def6f7`, outbound residual `e5368ff` | FIXED |
| 33 | Wallet subscription serving unbounded (response, cap, concurrency) | Tier-1 RPC/wallet audit | `5745f79`; trusted-tier residuals `bd1a503`, `f8883b7` | FIXED |
| 34 | Restart-resume repair: cache-only consensus walks, zero window margin, floor-invisible backfill | Tier-1 sync/stores audit + live mm/pg-b stall | `e428913` | FIXED |
| 35 | No long-sync band for a deep MID-CHAIN gap (far-behind decision only fired from a near-empty store) | Tier-1 sync audit | `0188373` | FIXED |
| 36 | Producer built only EMPTY blocks: no mempool→block-generator path | Tier-2 mempool audit | `833580d`; back-reference compression residual `aa9d9a0`/`c6f1a33` (suite: `node/tests/t080_block_assembly.rs`) | FIXED |
| 37 | Wallet `SendTransaction` (code 48) silently dropped: no dispatch arm, no `TransactionAck` | Tier-1 RPC/wallet audit | `7f2491e`; fee-per-cost untrusted-lane residual `c51105a` | FIXED |
| 38 | CLVM BLS operators 49..=59 unimplemented — `op_unknown` swallowed them for a token cost (live `InvalidBlockCost` wedge at mainnet 9,179,161, exact −5,597,941) | live wedge, node-0 | `748794c` (13 clvmr-pinned vectors + wire-captured wedge-window fixture) | FIXED |
| 39 | Modern wallet-sync surface (codes 94–103) silently dropped + no `NewPeakWallet` push — Sage could not sync | Tier-2 "Sage protocol" | `0a49ad5` (suite: `full-node/tests/puzzle_state.rs`) | FIXED |
| 40 | Wallet correctness residue: no additions/removals MerkleSet proofs, empty served `transactions_filter`, hint-blind CoinStateUpdate | Tier-2 wallet-correctness | `0ed9877` | FIXED |
| 41 | HTTP RPC not chia-tooling-compatible (bare values, HTTP-4xx errors, no 8555 client-cert posture, ~20 endpoints missing) | Tier-2 RPC parity | `5b2f154`; fee estimator `e7f9453`; warm-start persistence residual is a reasoned DELIBERATE-SKIP (`a006cd6`) | FIXED |
| 42 | Tier-2 mempool parity: CNI constants, dedup + singleton fast-forward, pool-full fee policy, admission/gossip gaps | Tier-2 mempool audit | `e1a7329`; ConflictTxCache `80157af`; origin-excluded re-broadcast `d6d487b` | FIXED |
| 43 | Lifecycle gaps: no sync-end transition, no reorg wallet delta, no on-connect greetings, one-shot introducer seed | Tier-2 audit + pi-IV production finding | `40d175e` | FIXED |
| 47 | No timed peer ban list — close sites could not refuse re-connection (the recurring residual across 27/31/32) | Tier-2 audit | `530cd52` (`core/src/protocols/ban.rs`); deliberate deltas documented in the entry (GC-on-touch, no exempt list — partially closed by `f8883b7` host rules) | FIXED |
| 48 | Compact-VDF (bluebox) solicitation was scan-only: bulky VDFs counted but never sent to a timelord | deliberate Phase-1.5 divergence, then closed | `26685bc` | FIXED |
| 49 | Parallel body-download worker accepted a peer's `RespondBlocks` batch without range/connectivity validation (one-peer DoS / wedge; hardening #178) | red-first tests + live stall analysis | `729abc6` (suite: `node/tests/download_validation.rs`); peer posture is drop-and-deprioritize, not chia's host ban — documented delta, follow-up tracked | FIXED |
| 50 | Fabricated "reorg horizon": fork coin context rebuilt from a bounded, volatile in-memory cache instead of the store — a heavier valid branch was refused after restart (epyc stranded at the 1-block equal-weight tie-break, mainnet 9,189,512; tracked as issue #193) | live epyc strand | fixed on `fix/reorg-horizon`, folded into this branch as `44fa227`; second-layer fix (reorg coin indexes built on the reorg path, not only at tip — the 425M-row seq-scan hang) `bf9fb32`, also in `44fa227`. Suites: `node/tests/reorg_fork_view_from_store.rs`, `node/tests/reorg_pipeline_indexes.rs` | FIXED |

| 51 | Malicious-generator DoS: `execute_block_generator_result` materializes the WHOLE generator output before cost-bounding it, so an adversarial block generator that emits a huge integer (`concat`/`substr` ladder, ~268 MB/arg) many times exhausts memory (one 280,000-arg vector OOM-killed a 24 GiB builder) and one emitting a 600,000-deep condition list overflows the native stack in the output walk/drop. chia charges condition cost INCREMENTALLY during the output parse and bails at `MAX_BLOCK_COST_CLVM` / the first duplicate (test_mempool.py:2867-2869). A sub-point: the ladder vector also probes condition-arg byte-length sanitization (leading-zero-padded oversized height atom) where our expected error is unconfirmed against chia_rs `sanitize_uint`. | harvest of `test_mempool.py::TestMaliciousGenerators` (Q2 correctness campaign #195) | not yet fixed — needs a streaming, cost-bounded condition parse (its own rung). Evidence + chia-cited vectors: `core/tests/malicious_generators.rs` (2 active, 8 `#[ignore]`d on this entry) | OPEN |

Unnumbered but ledgered: **DIV-HINT** — the `coin_hint` index stores only 32-byte hints
(chia stores any-length hint blobs); query behavior is identical. DOCUMENTED-INTENTIONAL.
**Hardening #180** — length-prefixed decode pre-allocation bounded
(`core/tests/streamable_alloc_bomb.rs`), FIXED (`70b9cfe`).

## Documented-intentional deltas carried in code (no number minted)

| Where | Delta |
|---|---|
| `core/src/consensus/producer.rs:52` | Foliage `extension_data` derived via sha256 instead of chia's MT19937 draw — consensus-irrelevant (only needs to differ per farmer); divergence note on the fn |
| `core/src/consensus/pot_iterations.rs:116,249` | Flagged non-bug deltas incl. the u64::MAX clamp where chia's `uint64()` would raise — divergence-locked with a test |
| `core/src/protocols/ban.rs:16` | Ban-list GC on touch vs chia's unbounded-between-GC growth; no localhost/exempt list (host trust rules landed separately, `f8883b7`) |
| `full-node/src/daemon.rs:1709-1711` | `MEMPOOL_UPDATES` capability not advertised — the mempool-update push surface (and its item-query tests) deliberately out of scope until adopted |
| `full-node/tests/rpc.rs` / `rpc.rs:json_u128` | RPC big-int JSON representation delta, noted at the fn |

## OPEN

| Item | What | Where tracked / evidence |
|---|---|---|
| **BlockRecord serialization byte-layout** (campaign issue #155) | `BlockRecord.challenge_vdf_output` / `infused_challenge_vdf_output` are `VdfOutput { data: UnsizedBytes }` — a variable-size carrier — where chia_rs `BlockRecord` carries fixed 100-byte `ClassgroupElement`s. Length is guarded at use sites (`TryFrom<&VdfOutput> for ClassgroupElement` rejects non-100-byte), but the serialized `BlockRecord` byte layout is not chia-identical; any surface that hashes or exchanges BlockRecord bytes must not assume parity until this is reconciled and locked with a byte-parity test. | `core/src/blockchain/vdf_output.rs`, `core/src/blockchain/block_record.rs:19-20` |
| **Deep-fork (> backtrack cap, < recent-chain) bulk entry untested** | The band between the short-sync backtrack floor (`node/src/sync/mod.rs` `BACKTRACK_MAX_DEPTH`) and the WP-anchored long-sync band (DIVERGENCE-35) has no test at any scale; chia proves 1500-block reorgs (`test_long_reorg*`). Ranked gap #3 in `docs/HARVEST-LEDGER.md`. | harvest ledger; `node/tests/backtrack.rs::fork_deeper_than_the_backtrack_cap_signals_long_sync` covers the *escalation*, not the entry |
| **`puzzle_state` default-features failure** | `full-node` integration test `tests/puzzle_state.rs` reported failing under a non-default feature combination (default-features build matrix). Not re-verified in this session (builds are cluster-only); needs a feature-matrix CI leg to pin. | campaign tracker |
| **Produce→broadcast e2e is stubbed at four seams** | The emission-contract table (`full-node/tests/emission_contract.rs`) carries four explicit `Ignored` entries: (1) in-process UB acceptance needs a real plot proof + VDF-populated SlotState; (2) `broadcast_new_peak` construction needs a stored FullBlock + outbound-peer capture harness (proven live by the sync sentinel only); (3) the tx-announce queue drain needs the in-process mempool-admit harness; (4) `process_compact_vdf_inbox` consume/swap runs live-only. The assembly halves are pinned (`producer_differential`, `declare_proof_of_space`, `unfinished_to_full_block_reconstruct`); the broadcast halves are not. | `full-node/tests/emission_contract.rs` (`Status::Ignored` entries) |
| **chia_rs `challenge_merkle_root` (6th `SubEpochSummary` field) not adopted node-wide** | Core's `SubEpochSummary` is deliberately the 5-field mainnet-active form (`core/src/consensus/make_sub_epoch_summary.rs:19` — "do not add the 6th field until it activates"), while the weight-proof summary reconstruction needed the 6-field hash form to match mainnet `ses_hash` (`weight-proof/src/lib.rs` phase-2 anchor test note). Adopting the field consistently requires an activation-gated design, not a blind add. | both anchors above |
