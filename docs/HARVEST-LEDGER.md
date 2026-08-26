# Chia Test-Harvest Reconciliation Ledger

File-by-file reconciliation of chia-blockchain's full-node-relevant test corpus against this
repo's test suite. The harvest campaign ported chia's tests **by cluster** (consensus rules,
header/body validation, mempool, SF9, producer differential, weight proofs, reorg suites, era
corpora); this ledger is the per-file accounting that the cluster approach never produced.

**Reference:** chia-blockchain @ `a95a2c6d6` (chia 2.7.1 lineage, `git describe` = `2.7.1-2`),
`chia/_tests/`.
**Ours:** branch `pr-parity-clean` @ `e92bd80` (test-file inventory as of this commit; the
ledger cites tests by `file::test_name` so line drift is harmless).

## Disposition key

| Disposition | Meaning |
|---|---|
| **COVERED** | The chia test's behavior is pinned by a named test of ours, one of three ways: **ported** (direct Rust port), **corpus** (real-mainnet-corpus replay supersedes the synthetic chia test — the oracle is the mainnet wire itself), or **oracle-differential** (byte/hash-differential against chia_rs / clvmr / a wire capture). |
| **PARTIAL** | The core behavior is covered but a named arm of the chia test is not; the uncovered arm is stated. |
| **GAP** | Applicable to our node, not covered. One line on what a harvest would pin. |
| **N/A** | Not applicable: python-daemon/simulator plumbing, wallet-internal logic, chia-internal component we deliberately do not have (each with the reason). |

Counts are **test functions** (`def test_*`), counted per file with
`grep -cE "^(async )?def test_|^    (async )?def test_"`. Monster files are grouped by class or
by behavioral family; every group lists its member count and (for non-obvious groupings) its
member names, so grouping loses nothing. Honest counts: a GAP row is a finding, not a failure.

Total in scope: **848 test functions** across `chia/_tests/blockchain/`,
`chia/_tests/core/full_node/` (incl. `stores/`, `full_sync/`, `dos/`), `chia/_tests/core/mempool/`,
`chia/_tests/generator/`, the consensus-relevant subset of `chia/_tests/util/` and
`chia/_tests/core/` (+ `core/util/`, `core/consensus/`, `core/custom_types/`), and the node-side
wallet-protocol-serving tests in `chia/_tests/wallet/`.

---

## 1. `chia/_tests/blockchain/` — 170 tests

Helpers (no tests, inventoried for completeness): `__init__.py`, `config.py`,
`blockchain_test_utils.py` (the `_validate_and_add_block` harness — our analog is the engine
test fixtures in `node/tests/common/`).

### `test_augmented_chain.py` — 8 tests — PARTIAL (8)

Tests chia's `AugmentedBlockchain` overlay (batch-prevalidation view: `test_augmented_chain`,
`_contains_block`, `_sequential`, `_validation_first_block_prev_hash`,
`test_fork_ancestry_populated_on_first_add`, 3 × `read_only_snapshot_*`). Our engine has its own
overlay (the `staged_deltas`/`pending` staging layer + store-backed fork view). Overlay behavior
under staging/reorg/restart is covered by `node/tests/reorg_fork_view_from_store.rs`,
`node/tests/staging_commit_granularity.rs`, `node/tests/restart_resume.rs::stage_walk_falls_back_to_store_after_missing_record_backfill`.
**Uncovered:** the explicit contains/sequential/snapshot-isolation contract (a read-only view
that rejects mutation and is isolated from the writer) is not pinned as its own test surface.

### `test_blockchain_transactions.py` — 15 tests — COVERED 11, PARTIAL 4

| Group | n | Disposition | Ours |
|---|---|---|---|
| basic tx / double spend / duplicate output | 3 | COVERED | `node/tests/t070_body_coin_rules.rs` (`honest_synthetic_spend_is_accepted`, `spending_an_already_spent_coin_is_rejected`), `core/tests/block_generator.rs::spend_bundle_rejects_double_spend` |
| reorg double-spend + spend-reorg-coin variants (`test_validate_blockchain_with_reorg_double_spend`, `_spend_reorg_coin`, `_spend_reorg_cb_coin`, `_spend_reorg_since_genesis`) | 4 | PARTIAL | fork-branch double spend: `t070_body_coin_rules.rs::fork_branch_double_spend_is_rejected`; coin-store-equals-replay: `node/tests/reorg.rs::heavier_branch_reorg_coin_store_equals_replay`. **Uncovered:** spendability of a coin created on the winning branch (incl. coinbase / since-genesis variants) immediately after the reorg. |
| coin/puzzle announcement consumed | 2 | COVERED | `core/tests/block_generator.rs::matching_coin_announcement_assertion_validates` + the paired send/receive message family |
| height/seconds absolute+relative asserts | 4 | COVERED | `core/tests/block_generator.rs` boundary suite (`assert_*_boundary_matches_chia`, 8 tests) — direct port of the chia parametrization |
| `test_assert_my_coin_id` | 1 | COVERED | `core/tests/block_generator.rs::assert_my_coin_id_valid_and_invalid` |
| `test_assert_fee_condition` | 1 | COVERED | `node/tests/t070_body_coin_rules.rs::tampered_fee_amount_is_rejected`, `tests/tests/coin_spend.rs::test_coinspend_reserved_fee` |

### `test_blockchain.py` — 114 tests — COVERED 69, PARTIAL 16, GAP 29

**`TestGenesisBlock` (8): COVERED 7, PARTIAL 1.** Genesis-era validation is proven on the real
chain: `node/tests/genesis_wall_36.rs::genesis_follow_confirms_the_first_thousand_blocks`
(corpus; the DIVERGENCE-19 fix's gate) covers non-overflow/overflow/empty-slot genesis shapes
that the corpus's real genesis era contains. PARTIAL: `test_genesis_validate_1` (mutation matrix
on the genesis block itself) — only the prev-hash arm is pinned
(`node/tests/add_block.rs::block_with_unknown_prev_hash_is_rejected`).

**`TestBlockHeaderValidation` (61): COVERED 30, PARTIAL 3, GAP 28.**

| Group | n | Disposition | Notes |
|---|---|---|---|
| chain-shape positives (`test_long_chain`, `unfinished_blocks`, `empty_genesis`, `empty_slots_non_genesis`, `one_sb_per_slot`, `all_overflow`, `unf_block_overflow`, `one_sb_per_two_slots`, `one_sb_per_five_slots`, `basic_chain_overflow`, `one_sb_per_two_slots_force_overflow`, `is_transaction_block`, `empty_sub_slots_epoch`) | 13 | COVERED (corpus) | `node/tests/nightly_conformance.rs::full_mainnet_slice_replays_and_every_header_validates` replays the full committed mainnet corpus with full single-header PoW/VDF validation at every height; `node/tests/header_validation.rs`, `node/tests/t050_headers_first.rs`, `node/tests/boundary_sweeps.rs` (offset-class sweeps at 384/4608 boundaries), `node/tests/slot_state.rs` (slot chaining). Real mainnet contains every one of these shapes (dust eras = empty-slot-heavy, overflow blocks throughout). |
| structural/linkage negatives (`invalid_prev`, `genesis_bad_prev_block`, `bad_prev_block_non_genesis`, `invalid_sub_slot_challenge_hash_genesis`/`_non_genesis`/`_empty_ss`, `genesis_no_icc`, `empty_slot_no_ses`, `genesis_has_ses`, `no_ses_if_no_se`, `bad_filter_hash`, `foliage_data_presence`, `foliage_transaction_block_hash`) | 13 | COVERED | `node/tests/add_block.rs` (unknown prev, height, weight rules); `node/tests/slot_state.rs` (`eos_with_wrong_challenge_rejected_and_never_cached`, `empty_slot_eos_with_ses_rejected`, `empty_slot_eos_with_icc_rejected`, chaining rules); `node/tests/t070_body_coin_rules.rs` (`tampered_filter_hash_is_rejected`, `tampered_transactions_info_hash_is_rejected`); `core/tests/block_generator.rs::transaction_block_validation_checks_metadata_and_roots` |
| height/weight getters (`test_height`, `test_height_genesis`, `test_weight`, `test_weight_genesis`) | 4 | COVERED | `node/tests/add_block.rs::block_height_must_extend_parent_by_one`, `::block_weight_must_strictly_increase_over_parent` + corpus |
| sp-index / iters negatives (`bad_signage_point_index`, `bad_total_iters`, `reset_deficit`) | 3 | PARTIAL | slot-level analogs exist (`slot_state.rs::signage_point_index_bounds`, `::signage_point_claimed_iters_must_match_its_index`; `core/src/consensus/deficit.rs` unit) — the add_block-level rejection of a block carrying the bad value is not pinned |
| per-field proof/signature mutation negatives (`invalid_pospace`, `bad_pos`, `invalid_icc_sub_slot_vdf`, `invalid_icc_into_cc`, `wrong_cc_hash_rc`, `invalid_cc_sub_slot_vdf`, `invalid_rc_sub_slot_vdf`, `bad_rc_sp_vdf`, `bad_rc_sp_sig`, `bad_cc_sp_vdf`, `bad_cc_sp_sig`, `bad_foliage_sb_sig`, `bad_foliage_transaction_block_sig`, `bad_cc_ip_vdf`, `bad_rc_ip_vdf`, `bad_icc_ip_vdf`, `unfinished_reward_chain_sb_hash`, `reward_block_hash`, `reward_block_hash_2`, `bad_timestamp`, `sp_0_no_sp`, `epoch_overflows`) | 22 | GAP | The *primitives* are tamper-proven (`weight-proof/tests/weight_proof_parity.rs` phase-5 tamper family, `full-node/tests/compact_vdf.rs::validate_accepts_the_real_compact_proof_and_rejects_a_tampered_one`, `node/tests/t055_assume_valid.rs`), but the **add_block-level rejection of each individually mutated header field is not pinned**. Blocked on a block-forge harness (we have no `block_tools` analog — see `test_build_chains.py` below). A harvest would pin: mutate one field of a real corpus block, re-cohere the containers that are not under test, assert the chia error code. |
| deficit/quantity negatives (`genesis_bad_deficit`, `too_many_blocks`) | 2 | GAP | same forge blocker; would pin deficit init and the 32-blocks-per-slot cap |
| pool-target family (`pool_target_height`, `pool_target_pre_farm`, `pool_target_signature`, `pool_target_contract`) | 4 | GAP | pool-target/pre-farm validation rejection is untested (construction side is pinned byte-identical by `node/tests/producer_differential.rs`) |
| unfinished-overflow arm (`test_unf_block_overflow` counted in positives) | — | — | — |

**`TestPreValidation` (2): PARTIAL 2.** Batch prevalidation with one bad block in the batch
(`test_pre_validation_fails_bad_blocks`, `test_pre_validation`). Ours validates per block through
the same engine on every path and rejects hostile batches before write-through
(`node/tests/download_validation.rs`), but there is no batch-prevalidation-with-poison-block
test as such.

**`TestBodyValidation` (25): COVERED 22, PARTIAL 3.**

| Group | n | Disposition | Ours |
|---|---|---|---|
| `test_conditions`, `test_timelock_conditions`, `test_ephemeral_timelock` | 3 | COVERED | `core/tests/block_generator.rs` boundary suite (ported chia parametrization, incl. negative/overflow args — the DIVERGENCE-4 family), `assert_ephemeral_created_in_block_validates`/`assert_ephemeral_without_parent_in_block_fails_140`; prev-tx-block frame per DIVERGENCE-14 (`fix` commit `4296559` tests) |
| tx-block data-presence rules (`not_tx_block_but_has_data`, `tx_block_missing_data`) | 2 | PARTIAL | non-tx arms: `node/tests/unfinished_body.rs::non_tx_unfinished_block_passes_untouched`, `::non_tx_unfinished_block_with_generator_is_rejected`; the full presence/absence matrix over all foliage fields is not swept |
| identity/root/hash tampering (`invalid_transactions_info_hash`, `invalid_transactions_block_hash`, `invalid_reward_claims`, `invalid_transactions_generator_hash`, `invalid_transactions_ref_list`, `invalid_merkle_roots`, `invalid_filter`) | 7 | COVERED | `node/tests/t070_body_coin_rules.rs` tamper family (roots, filter hash, tx-info hash, reward claims, fee); `core/tests/respond_blocks_generator_root.rs` (generator root + refs root on real back-referenced mainnet generators — the DIVERGENCE-10 gate); `node/tests/generator_ref_resolution.rs` + `core/tests/block_generator.rs::future_generator_reference_is_invalid` |
| cost rules (`cost_exceeds_max`, `clvm_must_not_fail`, `invalid_cost_in_block`) | 3 | COVERED | `node/tests/unfinished_body.rs` (over/understated cost claims, exact cost on real block 5,000,004), `node/tests/cost_wall_9179161.rs::mainnet_9179155_9179200_costs_are_exact` (corpus), `core/tests/respond_blocks_generator_execution.rs` (exact cost over ten real tx blocks), `core/tests/block_generator.rs::cost_limit_failure_is_reported` |
| coin rules (`max_coin_amount`, `duplicate_outputs`, `duplicate_removals`, `double_spent_in_coin_store`, `double_spent_in_reorg`, `minting_coin`, `invalid_fees_in_block`) | 7 | COVERED | `node/tests/t070_body_coin_rules.rs` (oversized amount, minting, already-spent, nonexistent, fork-branch double spend, tampered fee) — the DIVERGENCE-26 suite |
| signatures (`invalid_agg_sig`) | 1 | COVERED | `core/tests/block_generator.rs::bad_aggregate_signature_fails`; bundle-level AGG_SIG_UNSAFE folding per DIVERGENCE-20; `node/tests/t055_assume_valid.rs` proves the check is live above the milestone |
| `test_aggsig_garbage` (garbage trailing args per agg-sig opcode) | 1 | PARTIAL | infinity-pubkey rejection is ported (`agg_sig_infinity_pubkey_rejected_for_every_agg_sig_opcode`); the garbage-extra-arg parametrization per opcode is not |
| `test_max_coin_amount_fee` | 1 | PARTIAL | fee overflow at the coin-amount bound not separately pinned |

**`TestReorgs` (9): COVERED 6, PARTIAL 3.**
COVERED: `test_basic_reorg` (`node/tests/reorg.rs::heavier_branch_reorg_coin_store_equals_replay`),
`test_reorg_transaction` (single-tx reorg on all three backends — DIVERGENCE-29),
`test_get_blocks_at` (`stores/tests/block_store.rs::get_by_height_returns_the_confirmed_record`),
`test_get_header_blocks_in_range_tx_filter` (daemon `request_block_headers` filter tests,
`full-node/src/daemon.rs` unit "request_block_headers(return_filter=true) serves the same filter"),
`test_overlong_generator_encoding` (`core/tests/clvm_ported.rs::serialized_atom_overflow` +
strict-decode wire tests),
`test_long_reorg` (Q5 — `node/tests/long_reorg_scale.rs`: depths 100 and 1000 against a
pre-built heavier branch on SQLite, mmap, AND Postgres — weight-only flip with the whole branch
parked as orphans, exact per-height coin unwind byte-equal to a winning-chain replay,
single-transaction atomicity + cold-reopen recoverability under an injected peak-flip fault,
post-commit-only reorg reporting; the confirm-pipeline arm at bulk-entry depth is
`node/tests/deep_fork_bulk_entry.rs`).
PARTIAL: `test_reorg_from_genesis` (torn-peak/backtrack cover shallow shapes;
genesis-depth reorg not pinned), `test_get_tx_peak_reorg` (tx-peak tracked and consumed by the
mempool frame but its reorg transition is not asserted), `test_long_compact_blockchain`
(compact-proof validation is pinned in `full-node/tests/compact_vdf.rs`; a long fully-compact
chain replay is not).

**Module-level (9): COVERED 5, PARTIAL 4.**
COVERED: `test_reorg_new_ref` (generator refs across a reorg —
`node/tests/seed_generator_overlay_bounded.rs` + `node/tests/generator_ref_resolution.rs`),
`test_chain_failed_rollback` (`node/tests/reorg.rs` crash-mid-reorg family + mmap journal tests),
`test_lookup_block_generators` (`node/tests/generator_ref_resolution.rs`),
`test_get_header_blocks_in_range_tx_filter_non_tx_block` (daemon serving tests),
`test_reorg_stale_fork_height` (stale fork-context refresh — store-backed fork view rebuild,
`node/tests/reorg_fork_view_from_store.rs`).
PARTIAL: `test_reorg_flip_flop` (A/B/A/B alternating peaks — `node/tests/torn_peak.rs` +
`node/tests/backtrack.rs::tip_step_falls_back_to_backtrack_on_a_real_reorg` cover single flips,
not the sustained alternation), `test_get_tx_peak` (see above),
`test_include_spends_same_as_parent` + `test_include_block_same_as_parent_coins` (ForkInfo
identical-spends edge — the staged-delta overlay handles it on the corpus but no dedicated pin).

### `test_build_chains.py` — 18 tests — N/A (18)

Builders/validators for chia's cached `block_tools` test chains (default_400/1000/10000,
compact, long-reorg variants). We deliberately have no synthetic chain forge; our equivalent
fixture base is the committed **real mainnet corpus** (wire-captured `RespondBlocks` windows,
weight proof, era corpora). Note: this absence is what blocks the per-field header-mutation
sweep above — flagged in the ranked gaps.

### `test_get_block_generator.py` — 3 tests — COVERED 3 (ported)

Direct port: `core/tests/get_block_generator.rs` (header states the port) +
`node/tests/generator_ref_resolution.rs` (`resolve_generator_refs_preserves_ref_list_order`,
`resolve_generator_refs_missing_height_is_validation_failure` — chia's
`GENERATOR_REF_HAS_NO_GENERATOR`). DIVERGENCE-5's harvest.

### `test_lookup_fork_chain.py` — 12 tests — PARTIAL (12)

Chia's `lookup_fork_chain` shared-ancestor matrix (fork left/right/short, linear, no-shared,
root-shared, no-left-chain). Ours resolves fork points two ways, both tested on realistic
shapes: SES-positional (`node/tests/wp_fork_point.rs`, 5 tests incl. no-agreement and clamp
cases) and store-walk (`node/tests/reorg_fork_view_from_store.rs`). **Uncovered:** the
exhaustive 12-case ancestor-topology matrix as a unit surface for the store walk.

---

## 2. `chia/_tests/core/full_node/` (root) — 186 tests

### `test_add_prevalidated_blocks.py` — 2 — N/A (2)
Python API error-shape tests (prevalidation error returns `Err` not `assert`). Our engine's
equivalent paths are typed `NodeError` returns, exercised throughout the add_block/reject tests.

### `test_address_manager.py` — 19 — N/A (19)
Chia's bucketed `AddressManager` (tried/new buckets, collisions, eviction, serialization) is a
component we deliberately redesigned: a capped ring store with reservation/reclaim
(`p2p/src/address_manager.rs`, 8 invariant tests: dedup on intake, junk-flood cap, violation
forget, staleness age-out, persistence round-trip, bounded random fetch). The chia tests pin
chia's internal bucket math, which does not exist here.

### `test_block_height_map.py` — 21 — N/A (21)
Standalone height→hash + SES cache component absent by design (DIVERGENCE-9: subsumed by the
block store). The equivalent queries are pinned at the store:
`stores/tests/block_store.rs::height_map_contiguity_via_block_store`,
`::get_by_height_returns_the_confirmed_record`, rollback/reorg index tests, and
`stores/tests/contract.rs`.

### `test_conditions.py` — 15 — COVERED 14, PARTIAL 1
Unknown conditions with cost (`node/tests/opcode_coverage.rs::unknown_opcode_decodes_to_unknown_not_panic`,
`::every_opcode_is_enforced_or_noop`), softfork condition (`core/tests/clvm.rs` SF9 suite),
the per-opcode condition table (`core/tests/block_generator.rs` boundary suite), my_id
valid/invalid, coin/puzzle announcements valid/invalid, announcement cap
(`announcement_limit_is_mempool_only_consensus_accepts_1025` — DIVERGENCE-21), message
conditions (paired/unpaired/mismatched — codes 66/67), `agg_sig_infinity`
(`agg_sig_infinity_pubkey_rejected_for_every_agg_sig_opcode` — DIVERGENCE-6).
PARTIAL: `test_agg_sig_illegal_suffix` (the domain-separator suffix forgery arm is not pinned;
the additional-data suffixes themselves are exercised by every real-block agg-sig verification).

### `test_full_node_api_rate_hardening.py` — 9 — COVERED 5, PARTIAL 4
Wallet-serving hardening: budget caps and throttle-full rejection are pinned
(`full-node/tests/wallet.rs::limited_semaphore_rejects_beyond_active_plus_waiting`,
subscription caps, DIVERGENCE-33 response budget; oversized-response refusal in the daemon
serving tests). PARTIAL: the reorg-after-db-query race arms (`request_additions_rejects_on_reorg_after_db_query`,
`request_removals_rejects_on_reorg_after_db_query`), `request_removals_rejects_when_block_not_found`,
and the merkle-proof-with-unknown-coin arm are not race-pinned (the reorg-mismatch *request*
path is: `full-node/tests/puzzle_state.rs::mismatched_previous_header_hash_rejects_reorg`).

### `test_full_node.py` — 79 — COVERED 43, PARTIAL 28, GAP 2, N/A 6

| Group | n | C/P/G/N-A | Notes |
|---|---|---|---|
| wire shapes (`pre_validation_result`, `spendbundle_serialization`) | 2 | C2 | `node/tests/wire_roundtrip.rs` (corpus round-trip + hostile decode) |
| sync/reorg orchestration (`sync_no_farmer`, `basic_chain`, `new_peak`, `shallow_reorg_nodes`, `corrupt_blockchain`, `node_start_with_existing_blocks`, `add_block_missing_prev_record`, `long_reorg`, `long_reorg_nodes`, 2× `sync_from_fork_point_logs_*`, 2× `wallet_sync_task_failure*`) | 13 | C8 P1 N/A4 | COVERED: t053/t054/t056 sync suite, `full-node/tests/peer_loss_recovery.rs`, `node/tests/restart_resume.rs`, `node/tests/add_block.rs`, reorg suites; `long_reorg` at scale — `node/tests/long_reorg_scale.rs` (depths 100/1000, three backends) + `node/tests/deep_fork_bulk_entry.rs` (the batch-path convergence arm). PARTIAL: `long_reorg_nodes` — the single-node convergence arms (backtrack, bulk entry, reland) are pinned in-process; the two-live-node p2p sim is not. N/A: log-assertion plumbing + wallet-sync-task python task management. |
| connection/infra (`inbound_connection_limit`, `timelord_inbound_connection`, `request_peers`, `malformed_peer_version_on_connect`, `invalid_capability_can_connect`, `node_type_message_typechecking`, `node_types_inbound_connections_limit`) | 7 | C5 P2 | `p2p/tests/t040_handlers.rs` (type checking), `t041_sessions.rs` (handshake), `t043_resilience.rs`/`t044_defense.rs` (limits), `t047_on_connect.rs`, address-manager fetch. PARTIAL: per-node-type inbound limit matrix; invalid-capability tolerance arm. |
| sub-slot / signage points (`respond_end_of_sub_slot` ×3 variants, `new_signage_point_or_end_of_sub_slot`, `new_signage_point_caching`, `slot_catch_up_genesis`, `sp_catchup_*` ×4) | 10 | C4 P6 | `node/tests/slot_state.rs` (12 structural rules), the slot corpus gate (live-arrival predicate over real mainnet VDFs), `full-node/tests/announce_pull.rs::client_link_pulls_announced_signage_point`. PARTIAL: the EOS race/no-reorg arms and the 4-step SP-catchup recovery ladder (semaphore-full, invalid response, diverged peer, loop exhausted). |
| unfinished blocks (`respond_unfinished`, `new_unfinished_block`(2), `forward_limit`, `replaced_generator`, `double_blocks_same_pospace`, `request_unfinished_block`(2), `add_unfinished_block_with_generator_refs`, `farmed_behind_current_head`) | 10 | C2 P8 | COVERED: the generator-must-run gate (`node/tests/unfinished_body.rs` — DIVERGENCE-27) and generator-ref resolution. PARTIAL: unfinished-block store semantics (rank/better-block replacement, dedup of same-pospace doubles, forward limit, request/serve arms) live in `full-node/src/daemon.rs` unit tests but the chia arms are not mirrored 1:1 — see the FullNodeStore gap. |
| tx handling (`new_transaction_and_mempool`, `request_respond_transaction`, `respond_transaction_fail`, `unsolicited_transaction_ignored`, `pending_tx_cache_retry_on_new_peak`, `ban_for_mismatched_tx_cost_fee`, `new_tx_zero_cost`, `send_transaction_peer_tx_queue_full`, `add_transaction_sync_mode`, `add_transaction_no_peak`, 4× seen-cache lifecycle, 2× `tx_request_and_timeout_*` (+2 nested)) | 18 | C10 P8 | COVERED: `full-node/tests/tx_gossip.rs` (announce→pull→validate→admit, unsolicited dropped, zero-cost ban, mismatched-cost ban, syncing gate, full-pool skip, origin-excluded re-broadcast), `t060_mempool.rs` conflict/pending caches (DIVERGENCE-42), `full-node/src/tx_queue.rs` units, `send_transaction.rs` (not-synced ack). PARTIAL: the seen-cache add/remove-on-error lifecycle arms and the request-timeout/api-exception arms. |
| block serving (`request_block`, `request_blocks`, `request_header_blocks_non_tx`) | 3 | C3 | DIVERGENCE-31 suite: range cap, headers-only strip, unsolicited-reply close (daemon units + `p2p` rate-limit tests + `full-node/tests/rpc.rs::get_blocks_serves_the_range`) |
| compact VDF (`compact_protocol`, `compact_protocol_invalid_messages`, `unsolicited_compact_vdf`, `respond_compact_proof_message_limit`) | 4 | C3 P1 | `full-node/tests/compact_vdf.rs` (16 tests: serve/validate/accept/replace/solicit — DIVERGENCE-48). PARTIAL: the inbound message-rate limit arm. |
| declare proof of space (`no_overflow`, `overflow`, `late_unfinished_block`, `empty_block_no_new_tx_window_yet`, `unfinished_block_includes_block_generator`) | 5 | C3 P2 | `node/tests/declare_proof_of_space.rs` (99 real mainnet proofs → RequestSignedValues byte-identical), `node/tests/t080_block_assembly.rs` (generator inclusion + empty coercion), `node/tests/producer_differential.rs`. PARTIAL: the overflow-declare arm and late-declare-after-unfinished arm. |
| wallet serving (`register_for_coin_updates`, `request_puzzle_state_rejects_before_peak`, `request_puzzle_state_responds_normally`) | 3 | C2 P1 | `full-node/tests/wallet.rs`, `full-node/tests/puzzle_state.rs`. PARTIAL/delta: chia rejects `request_puzzle_state` before a peak exists; ours serves while unsynced (`puzzle_state.rs::unsynced_node_still_serves_puzzle_state`) — behavioral delta worth a deliberate decision. |
| misc (`block_compression`, `hard_fork_version_enforcement`, `eviction_from_bls_cache`, `hard_fork2_capability_on_release_branch`) | 4 | C2 N/A2 | COVERED: back-reference generator compression end-to-end (`t080_block_assembly.rs::backref_compression_packs_extra_spend_over_plain_limit` — DIVERGENCE-36 residual) and the flag ladder (`opcode_coverage.rs::hard_fork_height_selects_the_rom_not_the_condition_set`, `core/tests/clvm.rs` SF9). N/A: we keep no long-lived BLS pairing cache (per-block aggregate verify; within-block dedup via the chia_rs TreeCache mirror `core/tests/tree_hash_dedup.rs`); release-branch capability pinning is chia release engineering. |

### Small files

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `test_generator_tools.py` | 4 | COVERED 4 (ported) | `core/tests/generator_tools.rs` (header states the port: removals/additions split + spends-for-block) |
| `test_hard_fork_utils.py` | 2 | COVERED 2 | flag ladder keyed on own height (DIVERGENCE-23 tests; `core/tests/block_generator.rs::height_flags_*`) |
| `test_hint_management.py` | 2 | COVERED 2 | `core/tests/coin_spend_extraction.rs` (`hints_for_conditions` on real mainnet blocks), `stores/tests/coin_store.rs::hinted_coins_join_and_dedup` |
| `test_node_load.py` | 1 | N/A | simulator load harness; our perf rigs are `weight-proof/tests/validate_weight_proof_bench.rs`, `node/tests/readahead_latency.rs`, `node/tests/soak_clvm_memory.rs` |
| `test_performance.py` | 1 | N/A | same |
| `test_prev_tx_block.py` | 3 | COVERED 3 | DIVERGENCE-14 (validate against previous tx block; commit `4296559` tests), `t080_block_assembly.rs` `tx_candidate_prev` frame |
| `test_subscriptions.py` | 15 | COVERED 10, PARTIAL 5 | `full-node/tests/wallet.rs` (caps incl. chia's exact untrusted max, trusted tier, newly-added reporting, remove subset/all), `puzzle_state.rs::remove_subscription_requests_are_answered`. PARTIAL: overlapping coin/ph subscription peer-maps and `peers_for_spent/created_coin` granularity. |
| `test_sync_target_peak_gather.py` | 2 | COVERED 1, PARTIAL 1 | weight-ordered per-peer verified peak selection (`full-node/src/peak_book.rs`, 8 units — DIVERGENCE-28). PARTIAL: skip-on-deserialize-failure without banning arm. |
| `test_transactions.py` | 3 | N/A 3 | multi-node wallet-to-wallet propagation simulator; the node-side propagation contract is `full-node/tests/tx_gossip.rs` |
| `test_tx_processing_queue.py` | 8 | COVERED 8 | `full-node/src/tx_queue.rs` (9 units — DIVERGENCE-37 residual): local-first, per-peer lanes, full-queue, cleanup, per-lane fee-per-cost order, and (Q3 batch) the deficit-round-robin arms — chia's own DRR vector ported (`chia_deficit_round_robin_vector_orders_by_affordability`, incl. the no-cost-info `max_tx_clvm_cost` fallback) plus the cross-peer interleave pin. |

`dos/` contains no tests (config only).

---

## 3. `chia/_tests/core/full_node/stores/` — 70 tests

### `test_block_store.py` — 15 — COVERED 11, PARTIAL 3, N/A 1
COVERED: round-trip/byte-exact reload, blocks-by-height/range/hash, rollback + in-main-chain
flips, generator retrieval, peak/prev-hash (`stores/tests/block_store.rs` 13 tests,
`stores/tests/contract.rs` 7, `stores/tests/mmap_contract.rs` + `postgres_contract.rs` — the
same contract on all three backends, which chia does not do), replace-proof
(`compact_vdf.rs::replace_swaps_only_the_named_field_and_preserves_the_header_hash`),
compactified counts (`compact_vdf.rs::uncompact_fields_*`).
PARTIAL: `test_deadlock` (concurrent reader/writer storm), `test_get_block_bytes_in_range`
raw-bytes arm, `test_get_full_blocks_at` multi-per-height arm.
N/A: `test_unsupported_version` (chia DB schema-version gate; ours is migration-managed).

### `test_coin_store.py` — 19 — COVERED 15, PARTIAL 3, N/A 1
COVERED: basic store/spend/num-unspent/rollback/reorg (`stores/tests/coin_store.rs` — incl.
real-block apply+rollback byte-match, atomic rollback-in-batch, spent-index update),
get-coin-states paging/filters/hints (`batch_coin_states` suites on all three backends),
unspent-lineage (`node/tests/t062_mempool_dedup_ff.rs` FF admission via store lineage lookup),
parent-ids batching (`multi_get_large_batch_returns_present_skips_absent`).
PARTIAL: `test_duplicate_by_hint` dedup arm at scale, `test_batch_many_coin_states` (50k-coin
scale), `test_get_coin_records_by_parent_ids_respects_spent_filter_under_limit` exact-limit arm.
N/A: `test_unsupported_version`.

### `test_full_node_store.py` — 16 — COVERED 3, PARTIAL 5, GAP 8
COVERED: `test_basic_store`'s slot/SP arms (`node/tests/slot_state.rs`), `test_long_chain_slots`
(corpus slot gate), future-SP non-caching for unknown challenges
(`slot_state.rs::signage_point_with_unknown_challenge_not_cached_in_slots`).
PARTIAL: `find_best_block`, `mark_requesting`, future-IP entry basics, EOS per-key drop,
`new_finished_sub_slot` arms — analogous daemon logic exists (`full-node/src/daemon.rs` ub/sp
caches, 64 units) but the chia arms are not mirrored.
GAP: **unfinished-block rank + eviction family** (`unfinished_block_rank`,
`unfinished_block_eviction` ×3(+1), `add_to_future_ip_stores_entry`,
`add_to_future_sp_limits_keys`/`_entries_per_key`, `future_eos/ip_cache_bounds`) — bounded-cache
and rank semantics for unfinished blocks and future SP/IP/EOS entries. A harvest would pin the
cache bounds and the replacement ordering (chia's rank: earlier sp, then lower total_iters).

### `test_hint_store.py` — 10 — COVERED 6, PARTIAL 4
COVERED: basic store, coin-ids-multi, blockchain-integrated hints
(`stores/tests/coin_store.rs::hinted_coins_join_and_dedup`, mmap/pg variants,
`full-node/tests/rpc.rs::coin_records_by_hint_resolves_indexed_coins`,
`full-node/tests/wallet.rs::hinted_puzzle_hash_subscription_receives_create_and_same_block_spend`,
duplicate coins/hints dedup arms). Note DIV-HINT: the index deliberately stores only 32-byte
hints (query behavior identical).
PARTIAL: `test_counts`, `test_limits`, `test_multi_batch_limit` (page-limit exactness at the
store seam), `test_duplicates` at scale.

### `test_sync_store.py` — 10 — COVERED 6, PARTIAL 1, N/A 3
COVERED: peak bookkeeping, heaviest-peak-after-eviction, disconnect cleanup
(`full-node/src/peak_book.rs` 8 units — DIVERGENCE-28).
PARTIAL: `test_basic_store`'s sync-mode flag arms.
N/A: the `backtrack_syncing` reference counters (chia-internal task accounting; our backtrack is
the chaser ladder, pinned in `node/tests/backtrack.rs`).

---

## 4. `chia/_tests/core/full_node/full_sync/` — 10 tests

### `test_full_sync.py` — 10 — COVERED 9, PARTIAL 1
COVERED: `long_sync_from_zero` (`node/tests/t053_fast_sync.rs`,
`t056_fast_sync_advances_from_empty.rs`, `genesis_wall_36.rs`),
`sync_from_fork_point_and_weight_proof` (`node/tests/wp_fork_point.rs` + t053),
`batch_sync` (`t050_headers_first.rs`, `t051_reservation_window.rs`),
`backtrack_sync_1`/`_2` (`node/tests/backtrack.rs` — cap → long-sync escalation),
`close_height_but_big_reorg` (`node/tests/long_sync_reland.rs` — reorg across the gap,
DIVERGENCE-35), `sync_bad_peak_while_synced` + `bad_peak_cache_invalidation` (peak-book
verified/bad-peak handling — DIVERGENCE-28), `block_ses_mismatch`
(`wp_fork_point.rs::divergent_top_summary_reports_the_fork_below_the_peak`).
PARTIAL: `sync_none_wp_response_backward_comp` (peer returns None for a WP — our peer-failure
budget covers exhaustion generally, not this specific back-compat arm).

---

## 5. `chia/_tests/core/mempool/` — 210 tests

### `test_mempool_manager.py` — 69 — COVERED 59, PARTIAL 4, N/A 6

| Group | n | C/P/N-A | Ours |
|---|---|---|---|
| CLVM canonicity (`clvm_canonical`, `clvm_not_canonical`, `atom_canonical`/`_not_canonical`, `bundles_are_canonical`) | 5 | C5 | `core/tests/clvm.rs` SF9 CANONICAL_INTS suite; `node/tests/t062_mempool_dedup_ff.rs::non_canonical_dedup_solution_rejected` |
| harness self-tests (`bundles_fixture`, `coins_mempool_manager_fixture`, `wallet_fixture`, `optional_min`, `optional_max`) | 5 | N/A5 | python fixture plumbing |
| `test_no_peak` | 1 | C1 | `node/tests/t080_block_assembly.rs::no_peak_builds_nothing` |
| timelocks (`TestCheckTimeLocks::test_conditions`, `test_compute_assert_height`) | 2 | C2 | `node/tests/t060_mempool.rs` timelock family (17 tests: park/revive, effective heights, impossible constraints, before-bounds — DIVERGENCE-30) |
| admission basics (`empty_spend_bundle`, `negative_addition_amount`, `valid_addition_amount`, `too_big_addition_amount`, `duplicate_output`, `block_cost_exceeds_max`, `double_spend_prevalidation`, `minting_coin`, `reserve_fee_condition`, `too_many_atoms`, `unknown_unspent`, `validation_timeout`) | 12 | C11 N/A1 | `t060_mempool.rs` (unknown-unspent, fee/cost caps), `core/tests/block_generator.rs` (amount/minting/dup rules), `tests/tests/coin_spend.rs`, decode bounds (`clvm_ported.rs::serialized_atom_overflow`). N/A: async validation-timeout plumbing. |
| dedup / eligible-coin family (`same_sb_twice_with_eligible_coin`, `different_spends_order`, 5× `dedup_info_*`, `coin_spending_different_ways_then_finding_it_spent_in_new_peak`, `bundle_coin_spends`, `identical_spend_aggregation_e2e`, `dedup_not_canonical`) | 11 | C11 | `node/tests/t062_mempool_dedup_ff.rs` (12 tests, real CLVM singleton fixtures, conditions-runner eligibility flags — DIVERGENCE-42) |
| `test_ephemeral_timelock` | 1 | C1 | `t060_mempool.rs::ephemeral_removal_uses_synthesized_peak_record` |
| RBF (`can_replace`, `insufficient/sufficient_fee_increase`, `superset`, `superset_violation`, `total_fpc_decrease`, `sufficient_total_fpc_increase`, `replace_with_extra_eligible_coin`, `replacing_one_with_an_eligible_coin`) | 9 | C9 | `t060_mempool.rs` RBF suite + `t062::replacement_must_preserve_dedup_eligibility` (chia can_replace rules commit) |
| filter/queries (`get_items_not_in_filter`(2), `get_items_by_coin_ids`) | 3 | C2 P1 | BIP158 mempool filter honored (`tx_gossip.rs::request_mempool_transactions_honors_bip158_filter`, on_connect mempool sync). PARTIAL: `get_items_by_coin_ids` max-checked bound. |
| block assembly (`total_mempool_fees`, `create_bundle_from_mempool`(2), `create_block_generator`, `_real_bundles`, `_custom_spend`, `check_removals_with_block_creation`) | 7 | C7 | `node/tests/t080_block_assembly.rs` (17 tests — DIVERGENCE-36: fee-priority, 6000-spend cap, skip/stop heuristics, cost re-run gate, aggregate sig, SF9 form) |
| new-peak / FF / misc (`assert_before_expiration`, `new_peak_ff_eviction`, `multiple_ff`, `advancing_ff`, `spending_singleton_to_invalidate_existing_ff_spends`, `new_peak_deferred_ff_items`, `different_ff_versions`, `new_peak_txs_added`, `mempool_timelocks`, `check_removals`, `height_added_to_mempool`, `fill_rate_block_validation`, `mempool_item_to_spend_bundle`) | 13 | C10 P3 | `t060_mempool.rs` (`item_dropped_when_its_coin_is_spent_by_new_peak`, `new_peak_expires_resident_items_whose_before_bound_passed`), `t062` FF rebase/eviction family, O(delta) new-peak (`new_peak_touches_only_spent_coin_owners_and_reorg_uses_slow_path`). PARTIAL: `height_added_to_mempool`, `fill_rate_block_validation`, `mempool_item_to_spend_bundle` round-trip. |

### `test_mempool.py` — 124 — COVERED 84, PARTIAL 29, GAP 11

| Group | n | C/P/G | Ours |
|---|---|---|---|
| `TestConflictTxCache` (5) + `TestPendingTxCache` (9) | 14 | C12 P2 | `t060_mempool.rs` conflict-cache suite (`conflict_losing_bundle_is_cached_not_dropped`, readmit, cost-bound eviction, drop-on-confirm, independence from pending-height cache — DIVERGENCE-42 residual). PARTIAL: per-cache cost/item-limit exactness arms. |
| `TestMempool::test_basic_mempool` | 1 | C1 | `t060_mempool.rs::valid_bundle_admitted_in_fee_order` |
| `TestMempoolManager` per-condition semantics (announcements, my_id/parent/puzhash/amount, block index/age, time absolute/relative, fee conditions, stealing fee, double spends, agg sig) | 65 | C43 P22 | Happy-path + failure direction per opcode: `core/tests/block_generator.rs` boundary suite + `t060_mempool.rs`. **PARTIAL: the ~22 `*_garbage` / `*_missing_arg` arms** — chia's mempool-only `STRICT_ARGS_COUNT`/garbage-trailing-arg strictness split is not pinned per opcode on our admission path. |
| `TestGeneratorConditions` | 20 | C18 P2 | condition parsing/cost: `core/tests/block_generator.rs` (create-coin cost, agg-sig cost, hints, unknown/softfork conditions, message conditions, duplicate outputs), `opcode_coverage.rs`. PARTIAL: `agg_sig_extra_arg`, `invalid_condition_list_terminator` strict arms. |
| `TestMaliciousGenerators` | 11 | **G11** | The malicious-generator DoS ladder (duplicate large-integer ladders/substr/negative, duplicate reserve-fee, duplicate coin-announce, create_coin duplicates, many-create-coin, invalid coin spend). Adjacent-but-different: `node/tests/soak_clvm_memory.rs`, `core/tests/streamable_alloc_bomb.rs`. A harvest would port the vectors and pin validation-time bounds. |
| module-level (`items_by_feerate`, `full_mempool`, `limit_expiring_transactions`, `dedup_by_fee`, `max_spends_per_block`(2), `create_block_generator`(2+custom), `lineage_cache`, `get_puzzle_and_solution_for_coin_failure`, `timeout`, `get_items_by_coin_ids`, `keccak`) | 13 | C10 P3 | `t060` capacity ceiling (10 blocks) + expiring cap + fee order; `t080` spend caps; `t062` lineage; `rpc.rs::puzzle_and_solution_rejects_unspent_coin`. PARTIAL: `timeout` (create-bundle deadline), `get_items_by_coin_ids`, `keccak` op vectors (op exists in `core/src/clvm`; no pinned vectors). |

### Small files

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `test_mempool_fee_estimator.py` | 2 | COVERED 2 | `node/src/fee_estimator.rs` units + `node/tests/fee_estimator_hooks.rs` (DIVERGENCE-41 residual). Warm-start persistence is a documented DELIBERATE-SKIP. |
| `test_mempool_fee_protocol.py` | 1 | COVERED 1 | `full-node/tests/rpc.rs::get_fee_estimate_*` (3 tests) + `RequestFeeEstimates` wallet handler |
| `test_mempool_item_queries.py` | 5 | COVERED 1, N/A 4 | by-coin-id resident queries via RPC (`rpc.rs::mempool_read_endpoints_serve_the_resident_item`). N/A: the by-puzzle-hash/hint query surface exists to power `MEMPOOL_UPDATES`, a capability we deliberately do not advertise (`full-node/src/daemon.rs` "MEMPOOL_UPDATES initial push is skipped"). Revisit if the capability is adopted. |
| `test_mempool_performance.py` | 1 | N/A | perf harness |
| `test_singleton_fast_forward.py` | 8 | COVERED 8 | `node/tests/t062_mempool_dedup_ff.rs` (FF solo/same-block/different-block, immutability, no-latest-unspent double-spend classification) |

---

## 6. `chia/_tests/generator/` — 13 tests

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `test_compression.py` | 8 | COVERED 5, N/A 3 | Decompression family ported: `core/tests/clvm_ported.rs` (header cites `TestDecompression`), historical compressed block 834,752 vs chia generator fixture (`core/tests/block_generator.rs::historical_compressed_block_834752_matches_chia_generator_fixture`). N/A: the compress-side (`compress_spend_bundle`, `block_program_zero` ×2) — we emit the chia_rs `solution_generator`/back-reference form (pinned by `t080` + `producer_differential`), never the legacy block-program-zero compressor. |
| `test_generator_types.py` | 1 | COVERED 1 | `clvm_ported.rs::make_generator_args_exposes_first_template` |
| `test_rom.py` | 4 | COVERED 4 | `core/tests/block_generator.rs::legacy_generator_mode_executes_bootstrap_rom`, `::legacy_generator_mode_accepts_caller_supplied_reference_generators`, `::executes_block_generator_and_summarizes_conditions` (coin/block extras), historical 834,752/4,671,894 fixtures |

---

## 7. `chia/_tests/core/consensus/` + `custom_types/` — 23 tests

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `consensus/test_block_creation.py` | 1 | COVERED 1 | `compute_block_fee`: `node/tests/producer_differential.rs::reward_claim_reconstruction_round_trips_fixture` + byte-identical producer proof |
| `consensus/test_pot_iterations.py` | 6 | COVERED 5, PARTIAL 1 | direct port: `tests/tests/pot_iterations.rs` (`test_pot_iterations`, `test_calculate_sp_iters`, `test_calculate_ip_iters`, `test_win_percentage`) + `core/src/consensus/pot_iterations.rs` units (8, with divergence-lock comments) + `test_expected_plot_size_v1`. PARTIAL: `test_expected_plot_size_v2` (plot format v2 not adopted; becomes a gap at v2 activation). |
| `consensus/stores/test_coin_store_protocol.py` | 2 | COVERED 2 | `stores/tests/` contract suites (empty/non-empty store) |
| `custom_types/test_coin.py` | 3 | COVERED 3 | `tests/tests/coin.rs` (coin-id over amount ranges), `node/tests/wire_roundtrip.rs` (serialization) |
| `custom_types/test_proof_of_space.py` | 8 | COVERED 3, PARTIAL 2, N/A 3 | COVERED: quality-string verification on 99 real mainnet proofs (`node/tests/declare_proof_of_space.rs`), `required_iters_real_blocks.rs`, DIV-25 BitReader fix tests, `check_plot_param`. PARTIAL: `calculate_prefix_bits_v1` clamp parametrization. N/A: v2 family (`verify_and_get_quality_string_v2`, `calculate_prefix_bits_v2`, `v1_phase_out`) — plot format v2 not adopted; must be harvested when it is. |
| `custom_types/test_spend_bundle.py` | 3 | COVERED 3 | `tests/tests/coin_spend.rs::test_compute_additions_with_cost_*` family (8 tests) |

---

## 8. `chia/_tests/util/` (consensus-relevant subset) — 22 tests

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `test_condition_tools.py` | 14 | COVERED 6, PARTIAL 8 | COVERED: agg-sig pair extraction folded into aggregate verification incl. bundle-level AGG_SIG_UNSAFE (DIVERGENCE-20 tests, real block 2,272,201) and parse-condition strictness (`opcode_coverage.rs` round-trip + unknown-opcode, oversized/empty-op guards in the condition parser). PARTIAL: the `pkm_pairs` unit matrix (empty/no-agg-sigs/mixed/unsafe-restriction) and `parse_sexp_condition` arg-limit family are not mirrored as units. |
| `test_full_block_utils.py` | 2 | COVERED 2 (corpus) | `node/tests/wire_roundtrip.rs::real_corpus_round_trips_byte_identical` (full blocks off the real wire), headers-only strip (DIVERGENCE-31, `generator-less serving` daemon tests) |
| `test_network_protocol_files.py` | 1 | **GAP 1** | chia pins EVERY protocol message type against golden bytes. Ours round-trips real corpus messages but has no per-message-type golden matrix; a new/renumbered field in a rarely-seen message would not be caught. Harvest: generate golden byte fixtures for all served message types. |
| `test_network_protocol_json.py` | 1 | **GAP 1** | same, JSON shape (matters for the RPC envelope surface) |
| `test_network_protocol_test.py` | 4 | COVERED 1, N/A 3 | `test_rate_limits_complete` → composed v2 rate-limit tables mirrored (`p2p/tests/t045_rate_limits.rs` + unit tables — DIVERGENCE-32). N/A: python module-introspection completeness checks (`missing_messages`, `message_ids`, state machine). |

---

## 9. `chia/_tests/core/` (root, consensus-relevant subset) — 40 tests

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `test_coins.py` | 3 | COVERED 3 | `hash_coin_ids` inside the merkle-set port (DIVERGENCE-11: single-coin `hash_coin_ids` leaves), `tests/tests/coin.rs` |
| `test_cost_calculation.py` | 7 | COVERED 5, N/A 2 | exact-cost on real blocks (`cost_wall_9179161.rs`, `respond_blocks_generator_execution.rs`), mempool-mode strictness (SF9/`clvm.rs`), `get_puzzle_and_solution` (`coin_spend_extraction.rs`, `rpc.rs`), clvm max-cost (`block_generator.rs::cost_limit_failure_is_reported`). N/A: the two `*_speed`/`*_performance` arms. |
| `test_merkle_set.py` | 15 | COVERED 15 | `core/src/consensus/merkle_set.rs` units (6: node scheme, hashdown, terminal collapse — DIVERGENCE-11) + `core/tests/merkle_set_proofs.rs` (byte-parity vs real chia_rs 0.42.1 proofs, the exact CNI pin) + `t070` root tamper tests + DIVERGENCE-40 proof serving |
| `test_filter.py` | 1 | COVERED 1 | `core/src/consensus/block_filter.rs` units (6) + served-filter tests (DIVERGENCE-40 G3) |
| `test_program.py` | 1 | COVERED 1 | `core/tests/clvm_ported.rs` deserialization family + curry/uncurry ports (DIVERGENCE-2 `test_uncurry_*` family) |
| `test_db_validation.py` | 5 | N/A 5 | chia's SQLite file-format validation tool; our schemas are migration-managed (`stores/migrations/`) and not chia-DB-compatible by design |
| `test_db_conversion.py` | 1 | N/A 1 | v1→v2 chia DB converter |
| `test_full_node_rpc.py` | 7 | COVERED 6, PARTIAL 1 | `full-node/tests/rpc.rs` (30 tests: chia semantics — end-exclusive ranges, error-not-null, push_tx idempotency, netspace formula, mempool reads) + `rpc_http.rs` (envelope + 8555 TLS + chia-client e2e — DIVERGENCE-41). PARTIAL: `test_signage_points` (farmer-facing get_recent_signage_point RPC arm; the serving structure exists in `full-node/src/rpc.rs::get_recent_signage_point_or_eos` without a chia-mirrored test). |

---

## 10. `chia/_tests/core/util/` (consensus-relevant subset) — 60 tests

| File | n | Disposition | Ours / reason |
|---|---|---|---|
| `test_streamable.py` | 55 | COVERED 20, PARTIAL 5, N/A 30 | COVERED: the wire-parsing half — `parse_bool`/`parse_optional`/`parse_bytes`/`parse_list`/`parse_str`/`parse_tuple`/`uint32` framing, trailing-bytes rejection (DIVERGENCE-8), presence-tag strictness (DIVERGENCE-7) — `core/tests/streamable_wire.rs` (19 tests, header cites the chia oracle functions) + `streamable_alloc_bomb.rs` (bounded pre-allocation, #180) + `node/tests/wire_roundtrip.rs` (corpus round-trip, hostile/truncated/mutated decode never panics). PARTIAL: the rust-types trailing-bytes matrix (the list-limit variants `parse_list_limited_*` / `from_bytes_with_list_limits` are now COVERED — CHIA-4203 CPU half, `core/tests/list_limited_decode.rs` + `p2p/tests/t049_list_limited_decode.rs`). N/A: the ~30 python-side tests (dataclass/decorator rules, `from_json_dict` conversion failures, enum proxies, post-init) — our analog is the type system + serde. |
| `test_block_cache.py` | 1 | N/A 1 | wallet weight-proof-handler helper |
| `test_cached_bls.py` | 2 | N/A 2 | we keep no cross-block BLS cache by design (per-block aggregate verify; within-block dedup via `tree_hash_dedup.rs`) |
| `test_significant_bits.py` | 2 | COVERED 2 | difficulty truncation cross-checked on every real on-chain retarget: `weight-proof/tests/difficulty_adjustment_mainnet.rs::mainnet_retargets_match_significant_bits_and_rounding` |

---

## 11. `chia/_tests/wallet/` (node-serving side only) — 44 tests

### `sync/test_wallet_sync.py` — 22 — COVERED 7, PARTIAL 1, N/A 14
Node-serving arms COVERED: `request_block_headers` (+ transactions filter, + rejected)
(`full-node/src/daemon.rs` request_block_headers units incl. filter parity — DIVERGENCE-40 G3),
`request_additions_errors`/`_success` + `request_removals_too_many_coin_names` (DIVERGENCE-40
G2 MerkleSet proofs + response budget), `get_wp_fork_point` (`node/tests/wp_fork_point.rs`).
PARTIAL: `request_header_blocks_without_block_headers_capability` (capability-branched serving
arm).
N/A (14): wallet-internal sync behavior (`basic_sync_wallet`, `almost_recent`,
`backtrack_sync_wallet`, `short_batch_sync_wallet`, `long_sync_wallet`, `wallet_reorg_sync`,
`wallet_reorg_get_coinbase`, `dusted_wallet`, `retry_store`, `bad_peak_mismatch`,
`long_sync_untrusted_break`, `long_reorg_nodes_and_wallet`, 2× `validate_received_state_from_peer_*`)
— the wallet side of the protocol; our node-side proof is the Sage e2e
(`full-node/tests/puzzle_state.rs::sage_sync_sequence_end_to_end`).

### `test_new_wallet_protocol.py` — 22 — COVERED 12, PARTIAL 1, N/A 9
COVERED: puzzle/coin subscriptions + limits (3), `request_coin_state` family (4, incl. the
reorg-reject arm — `puzzle_state.rs::mismatched_previous_header_hash_rejects_reorg`),
`request_puzzle_state` family (4), `sync_puzzle_state` (the Sage e2e) —
`full-node/tests/puzzle_state.rs` (10 tests, production stacks both ends, DIVERGENCE-39) +
`full-node/tests/wallet.rs` (11 tests, caps + trusted tier).
PARTIAL: `test_cost_info` (`RequestCostInfo` serving unverified).
N/A (9): the 6 `*_mempool_update` tests + 3 `missing_capability_*` tests — all gated on the
`MEMPOOL_UPDATES` capability, which we deliberately do not advertise
(`full-node/src/daemon.rs:1709-1711`); chia itself skips the push for peers without the
capability. Revisit on adoption.

---

## Summary

| Directory | Tests | COVERED | PARTIAL | GAP | N/A |
|---|---|---|---|---|---|
| `blockchain/` | 170 | 83 | 40 | 29 | 18 |
| `core/full_node/` (root) | 186 | 90 | 41 | 2 | 53 |
| `core/full_node/stores/` | 70 | 41 | 16 | 8 | 5 |
| `core/full_node/full_sync/` | 10 | 9 | 1 | 0 | 0 |
| `core/mempool/` | 210 | 155 | 33 | 11 | 11 |
| `generator/` | 13 | 10 | 0 | 0 | 3 |
| `core/consensus/` + `custom_types/` | 23 | 17 | 3 | 0 | 3 |
| `util/` subset | 22 | 9 | 8 | 2 | 3 |
| `core/` root subset | 40 | 31 | 1 | 0 | 8 |
| `core/util/` subset | 60 | 22 | 5 | 0 | 33 |
| `wallet/` node-serving | 44 | 19 | 2 | 0 | 23 |
| **Total** | **848** | **486** | **150** | **52** | **160** |

Of the 688 applicable tests (excluding N/A): **71% COVERED, 22% PARTIAL, 8% GAP**.

## Ranked gaps (consensus-critical first)

1. **Per-field header/proof mutation negatives — 28 tests**
   (`TestBlockHeaderValidation` mutation family + genesis mutation matrix). The VDF/BLS
   *primitives* are tamper-proven; the **add_block-level rejection of each individually mutated
   header field** (pospace, cc/rc/icc sub-slot + ip + sp VDFs, sp/foliage signatures,
   reward-block-hash bindings, pool target, timestamp, deficit, total_iters) is not. A wrongly
   *accepted* forged header is a chain split. Blocked on a block-forge harness (we have no
   `block_tools` analog); the pragmatic harvest is mutate-one-field-of-a-real-corpus-block +
   assert the chia error code.
2. **Malicious-generator DoS ladder — 11 tests** (`TestMaliciousGenerators`). Adversarial
   generators engineered for validation-cost blowup (duplicate large-integer ladders, duplicate
   announces, many-create-coin). Untrusted-input robustness of the mempool/validation path;
   vectors are directly portable.
3. **Long-reorg at scale — CLOSED (Q5)** (`test_long_reorg`, `test_long_reorg_nodes`,
   `TestReorgs::test_long_reorg`, plus the tracked **deep-fork bulk-entry** path):
   `node/tests/long_reorg_scale.rs` (depths 100/1000 on SQLite, mmap, Postgres — weight-only
   flip, exact coin unwind, one-transaction atomicity + crash/reopen recoverability),
   `node/tests/deep_fork_bulk_entry.rs` (the > backtrack-cap batch-path entry: escalation,
   pipeline reorg at depth 26, crash-at-flip + store-rebuilt recovery across a restart),
   `node/tests/reorg_while_shed.rs` (reorg with the service indexes shed, beside a live
   writer). Residual: the two-live-node `long_reorg_nodes` p2p sim (in-process convergence
   arms are pinned).
4. **FullNodeStore unfinished-block rank/eviction + future-cache bounds — 8 tests**
   (`test_full_node_store.py`). Bounded caches for unfinished blocks / future SP/IP/EOS exist in
   the daemon but their bounds and the chia rank/replacement order are unpinned — a
   memory-bound and a farming-correctness surface (which candidate wins).
5. **Golden network-protocol byte/JSON matrix — 2 tests** (`test_network_protocol_files/json`).
   One golden fixture per message type; catches silent wire-format drift that corpus round-trips
   only catch for messages the corpus happens to contain.

Notable PARTIAL clusters (not GAP, but named arms worth closing): the ~22 mempool
`*_garbage`/`*_missing_arg` strict-args arms; the unfinished-block serving arms (rank/forward
limit); the SP-catchup recovery ladder (4); the seen-cache lifecycle arms (4); `pkm_pairs` unit
matrix (8); reorg-created-coin spendability variants (4); reorg-after-db-query race arms;
`keccak` op vectors; `test_cost_info` serving; the chia-vs-ours delta on serving
`request_puzzle_state` before a peak.
