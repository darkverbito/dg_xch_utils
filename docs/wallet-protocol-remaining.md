# Light-wallet ↔ full_node protocol — served surface and remaining gaps

Parity tracking for the `wallet_protocol` message surface (`chia/protocols/wallet_protocol.py`,
handlers in `chia/full_node/full_node_api.py`). Served by `p2p/src/handlers.rs` dispatch +
`FullNodeApi` impls in `full-node/src/daemon.rs`, on the coin/block store primitives in
`dg_xch_stores`.

## Served (parity #2)

| Code | Message | Handler | Notes |
|---|---|---|---|
| 48 | `SendTransaction` | `send_transaction` | the wallet's spend submit, ALWAYS acked with `TransactionAck` (49): chia's `(MempoolInclusionStatus, Err.name)` — idempotent-SUCCESS on duplicates, PENDING for height-parked bundles, FAILED `NO_TRANSACTIONS_WHILE_SYNCING` when not synced. Shares the push_tx admission seam (`tx_admission.rs`); DIVERGENCE-37. This message was previously MISSING from this doc while being silently dropped on the wire (no dispatch arm) — the audit headline that motivated tracking acked/served status here per message |
| 45 | `RequestPuzzleSolution` | `puzzle_solution` | generator re-run (shared with HTTP RPC); post-hard-fork blocks |
| 51 | `RequestBlockHeader` | `block_header` | real BIP158 `transactions_filter` (G3 closed, DIVERGENCE-40) |
| 60 | `RequestHeaderBlocks` | `header_blocks` | real filter per block (G3 closed) |
| 86 | `RequestBlockHeaders` | `block_headers` | real filter; `return_filter=false` → encoded-empty `b"\x00"` (G3 closed) |
| 54 | `RequestRemovals` | `removals` | trusted `None`/`Some([])` path AND the specific-`coin_names` proof path: `MerkleSet` inclusion/exclusion proofs against the foliage `removals_root` (G2 closed, DIVERGENCE-40) |
| 57 | `RequestAdditions` | `additions` | trusted `None` path AND the specific-`puzzle_hashes` proof path (`[ph, hash_coin_ids]` leaf pairs, per-hash proof triples); `Some([])` short-circuit answers `proofs=Some([])` like chia (G2 closed, DIVERGENCE-40) |
| 74 | `RequestChildren` | `children` | parent index |
| 70 | `RegisterForPhUpdates` | `register_for_ph_updates` | initial state (spent+unspent + hint join) + `CoinStateUpdate` forwarder |
| 72 | `RegisterForCoinUpdates` | `register_for_coin_updates` | initial state + forwarder |
| 69 | `CoinStateUpdate` (push) | peak-delta push | `WalletNotifier.on_new_peak` → per-peer bounded channel → wire; matches puzzle hash, coin id, AND the block's create-coin hints against ph subscriptions (chia `full_node.py:1544-1546`; DIVERGENCE-40) |
| 98 | `RequestPuzzleState` | `puzzle_state` | the Sage sync loop: paged spent+unspent+hinted history via `batch_coin_states_by_puzzle_hashes` (whole-height page cuts), previous-peak reorg check (`RejectPuzzleState(REORG)`), subscription cap (`EXCEEDED_SUBSCRIPTION_LIMIT`), subscribe-on-finish; DIVERGENCE-39 |
| 101 | `RequestCoinState` | `coin_state` | coin-id twin: same reorg/cap checks, `get_coin_states_by_ids`, subscribe-on-request; DIVERGENCE-39 |
| 94 | `RequestRemovePuzzleSubscriptions` | `remove_puzzle_subscriptions` | `None` = clear all (returns prior set), `Some` = subset (returns removed); DIVERGENCE-39 |
| 96 | `RequestRemoveCoinSubscriptions` | `remove_coin_subscriptions` | coin-id twin; DIVERGENCE-39 |
| 50 | `NewPeakWallet` (push) | on-connect + peak advance | wallet-handshake greeting (fork_point = peak height; Sage drops silent peers after 2s) + broadcast to wallet-type peers on every confirmed peak (fork_point = height-1); DIVERGENCE-39 |

Initial-state reads use `CoinStore::get_coin_states_by_puzzle_hashes` /
`get_coin_states_by_ids` (spent + unspent from `min_height`, chia
`coin_store.py:486`/`:552`). Disconnect hygiene: `subscription_reaper` reconciles the registry against
the live inbound `PeerMap` every 30s (the servers-crate-free disconnect hook).

## Remaining gaps — each needs a primitive that does not exist yet

Handlers for these are `reject`/empty-stubbed faithfully (never wrong data), pending the primitive:

- **SendTransaction residual deltas** (served; see DIVERGENCE-37 for the full list): no trusted
  tier (no high-priority queueing), no TransactionQueue (synchronous admission — chia's queue-full
  and 45s-timeout PENDING acks cannot arise), no conflict cache (a losing replace-by-fee acks
  chia's PENDING/MEMPOOL_CONFLICT but relies on the wallet's resubmit for retry), no seen/in-flight
  cache (concurrent duplicates ack idempotent SUCCESS, not ALREADY_INCLUDING_TRANSACTION).

- ~~G2 — additions/removals Merkle proofs~~ — **CLOSED** (DIVERGENCE-40): `dg_xch_core::consensus::
  merkle_set::MerkleSet` (a byte-parity port of chia_rs 0.42.1's proof-capable set) now backs the
  specific-`puzzle_hashes`/`coin_names` paths; proofs verify against the foliage roots and are
  byte-equal to chia_rs's (`merkle_proofs_5000000.json` gates).
- ~~G3 — header `transactions_filter` (BIP158)~~ — **CLOSED** (DIVERGENCE-40): the coin-index tier
  serves the real filter (the T0-1 `chia_block_filter` builder over added puzzle hashes + removal
  names, from the added/removed-at-height indexes — chia `full_node_api.py:1644-1652/:1693-1700`);
  `header_block_from_full_block`'s default is now chia's encoded-empty `b"\x00"`, which is what a
  non-coin-index (non-wallet-serving) tier keeps serving.
- **G4 — `RequestSESInfo` (76).** Needs a served `get_ses_heights` / `get_ses` surface over the
  sub-epoch-summary metadata (chia `blockchain.get_ses_heights`/`get_ses`).
- **G5 — `RequestFeeEstimates` (89).** Needs a mempool fee estimator (chia `FeeEstimatorInterface`).
- **Mempool update pushes (104/105 — `MempoolItemsAdded`/`MempoolItemsRemoved`) + the
  initial mempool pushes inside `request_puzzle_state`/`request_coin_state`
  (full_node_api.py:2074/:2137).** Gated in chia on `Capability.MEMPOOL_UPDATES`
  (:2160-2161/:2197-2198), which we do not advertise — a wallet negotiates the capability and
  simply does not expect the pushes from us. In scope only if/when the capability is added.
- ~~Newer wallet-sync surface (94–103)~~ — **served** as of DIVERGENCE-39 (see the table above):
  `RequestPuzzleState`/`RequestCoinState` ride the paginated
  `CoinStore::batch_coin_states_by_puzzle_hashes` (chia `coin_store.py:590` semantics: ordered by
  activity height, whole-height page boundaries, spent/unspent/hinted/min_amount filters) and the
  remove-subscription messages map onto `WalletNotifier` removals. `NewPeakWallet` (50) is pushed
  on wallet connect and on every peak advance — without it Sage drops the connection 2s after the
  handshake and never issues a single sync request.
