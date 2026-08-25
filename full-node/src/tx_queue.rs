// The gossip-transaction admission queue with a trusted priority lane — chia `TransactionQueue`
// (chia/full_node/tx_processing_queue.py, post-9491c6ee3). This is the bounded inbox
// `on_transaction` fills off the websocket read loop and `spawn_tx_validator` drains.
//
// Chia priority semantics (mirrored here): `TransactionQueue.put(tx, peer_id, high_priority)`
// routes a high-priority (trusted, or local `peer_id=None`) entry into `_high_priority_queue` —
// an UNBOUNDED SimpleQueue — and `pop()` drains that queue ENTIRELY before it ever touches the
// per-peer untrusted queues.
//
// UNTRUSTED-LANE ORDERING — chia CHIA-3856 (9491c6ee3, cherry-picked as 04b9d010b): an adapted
// DEFICIT ROUND ROBIN across peers, by advertised CLVM cost.
//   - Each untrusted peer has its own queue ordered by advertised fee-per-cost, highest first
//     (chia's per-peer `PriorityQueue` keyed on `-fee_per_cost`; a no-cost-info entry sorts last
//     at `+inf`).
//   - `pop()` walks the peers round-robin from a cursor. A peer may send its TOP transaction
//     only when its cost DEFICIT covers the transaction's advertised cost (fallback
//     `max_tx_clvm_cost` — chia passes `MAX_BLOCK_COST_CLVM // 2` — when the peer advertised no
//     cost); the pop spends the deficit, resets it to zero when the peer's queue empties, and
//     advances the cursor to the NEXT peer. When no peer can afford its top transaction, the
//     LOWEST top-cost among peers with queued transactions is added to every such peer's
//     deficit and the walk repeats (tx_processing_queue.py:159-205).
//   This decreases the effect of one peer spamming the node: a peer's high-fpc stream can no
//   longer monopolize validation order — service interleaves across peers by cost.
//
// Bounds: chia enforces a per-peer `peer_size_limit` (put raises TransactionQueueFull); we keep
// that per-peer cap AND the pre-existing aggregate cap on the whole untrusted backlog (ours,
// documented — chia's aggregate bound is implicit in its connection cap). The advertised
// fee/cost affect ONLY service order, never admission — every bundle is validated at its TRUE
// fee downstream.
//
// Cleanup delta (documented): chia prunes empty per-peer queues every 100 pops
// (`_cleanup_peer_queues`). Our drain is batch-total — `drain_batch` empties every lane — so
// the equivalent cleanup is a full reset of the per-peer state at the end of the drain (chia
// resets each peer's deficit to zero the moment its queue empties, so a post-drain reset is
// state-identical).

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use std::cmp::Ordering;
use std::collections::{HashMap, VecDeque};

// One untrusted-lane entry: the bundle and the ADVERTISED fee + cost (from the peer's
// `NewTransaction`, chia's `PeerWithTx`) the lane orders on.
struct UntrustedTx {
    bundle: SpendBundle,
    advertised_fee: u64,
    advertised_cost: u64,
}

// Order two untrusted entries by advertised fee-per-cost, HIGHEST first (chia `-fee_per_cost` in
// a lowest-first PriorityQueue). fpc = fee/cost compared without division via cross-multiplication
// in u128 (advertised_fee ≤ ~2^50, advertised_cost < 2^33, so the product never overflows u128).
// An entry with zero/unknown advertised cost has no fpc and sorts LAST (chia's `+inf` priority).
fn fee_per_cost_desc(a: &UntrustedTx, b: &UntrustedTx) -> Ordering {
    match (a.advertised_cost, b.advertised_cost) {
        (0, 0) => Ordering::Equal,
        (0, _) => Ordering::Greater, // a has no fpc → sorts after b
        (_, 0) => Ordering::Less,    // b has no fpc → a sorts before b
        (ac, bc) => {
            let lhs = u128::from(a.advertised_fee) * u128::from(bc);
            let rhs = u128::from(b.advertised_fee) * u128::from(ac);
            // DESC: the higher fpc compares Less so it lands first.
            rhs.cmp(&lhs)
        }
    }
}

// A peer's untrusted lane: its fee-ordered queue plus its DRR cost deficit (chia
// `PeerTransactionsQueue`).
#[derive(Default)]
struct PeerLane {
    // fpc-descending; FIFO among equal fpc (insertion keeps arrival order within a priority —
    // chia's tie order is incidental bytes32 ordering; stable-FIFO is our documented choice).
    queue: VecDeque<UntrustedTx>,
    // The peer's deficit, in CLVM cost units (chia `PeerTransactionsQueue.deficit`).
    deficit: u64,
}

/// A bounded gossip-transaction inbox with a trusted high-priority lane and a deficit-round-robin
/// untrusted lane (chia `TransactionQueue`, post-CHIA-3856).
pub struct TxQueue {
    // chia `_high_priority_queue`: trusted (and local) transactions, drained in full before the
    // untrusted lanes. Unbounded, exactly as chia — trust is the admission control. FIFO.
    high: VecDeque<(Bytes32, SpendBundle)>,
    // Per-peer untrusted lanes (chia `_peers_transactions_queues`).
    lanes: HashMap<Bytes32, PeerLane>,
    // Round-robin order + cursor (chia `_index_to_peer_map` / `_list_cursor`).
    order: Vec<Bytes32>,
    cursor: usize,
    // Total entries across all untrusted lanes (the aggregate bound's accounting).
    total_untrusted: usize,
    // Aggregate cap on the untrusted lane (ours; chia bounds per peer only).
    cap: usize,
    // Per-peer cap (chia `peer_size_limit` → TransactionQueueFull).
    per_peer: usize,
    // The DRR cost fallback for entries with no advertised cost — chia `max_tx_clvm_cost`
    // (full_node.py passes `MAX_BLOCK_COST_CLVM // 2`).
    max_tx_clvm_cost: u64,
}

impl TxQueue {
    /// A queue with the given untrusted-lane bounds and the DRR cost fallback (chia
    /// `TransactionQueue(peer_size_limit, log, max_tx_clvm_cost=MAX_BLOCK_COST_CLVM // 2)`).
    /// The high-priority lane is unbounded (chia).
    #[must_use]
    pub fn new(cap: usize, per_peer: usize, max_tx_clvm_cost: u64) -> Self {
        Self {
            high: VecDeque::new(),
            lanes: HashMap::new(),
            order: Vec::new(),
            cursor: 0,
            total_untrusted: 0,
            cap,
            per_peer,
            // A zero fallback would let a costless entry pop for free forever; chia's value is
            // always positive (MAX_BLOCK_COST_CLVM // 2).
            max_tx_clvm_cost: max_tx_clvm_cost.max(1),
        }
    }

    /// Enqueue `bundle` from `peer`. `high_priority` (a trusted peer) routes to the unbounded
    /// priority lane and always succeeds; an untrusted bundle is admitted to its peer's lane only
    /// if BOTH the aggregate and the per-peer bound have room — otherwise it is dropped on the
    /// floor (chia `TransactionQueueFull`, mapped to a silent drop as before).
    /// `advertised_fee`/`advertised_cost` are the peer's `NewTransaction` values; they order the
    /// lane and price the DRR pop, never admission. Returns whether it was admitted.
    pub fn push(
        &mut self,
        peer: Bytes32,
        bundle: SpendBundle,
        high_priority: bool,
        advertised_fee: u64,
        advertised_cost: u64,
    ) -> bool {
        if high_priority {
            self.high.push_back((peer, bundle));
            return true;
        }
        if self.total_untrusted >= self.cap {
            return false;
        }
        if !self.lanes.contains_key(&peer) {
            self.order.push(peer);
        }
        let lane = self.lanes.entry(peer).or_default();
        if lane.queue.len() >= self.per_peer {
            return false;
        }
        let entry = UntrustedTx {
            bundle,
            advertised_fee,
            advertised_cost,
        };
        // Stable fpc-desc insert: after every entry with fpc >= the new one.
        let pos = lane
            .queue
            .iter()
            .position(|e| fee_per_cost_desc(e, &entry) == Ordering::Greater)
            .unwrap_or(lane.queue.len());
        lane.queue.insert(pos, entry);
        self.total_untrusted += 1;
        true
    }

    // The DRR cost of a lane's top entry — the advertised cost, or `max_tx_clvm_cost` when the
    // peer sent no cost info (chia: "If we don't know the cost information ... fallback to the
    // highest cost").
    fn top_cost(&self, lane: &PeerLane) -> Option<u64> {
        lane.queue.front().map(|e| {
            if e.advertised_cost > 0 {
                e.advertised_cost
            } else {
                self.max_tx_clvm_cost
            }
        })
    }

    // One deficit-round-robin pop from the untrusted lanes — the direct port of chia
    // `TransactionQueue.pop()`'s normal-queue walk (tx_processing_queue.py:159-205).
    fn pop_untrusted(&mut self) -> Option<(Bytes32, SpendBundle)> {
        if self.total_untrusted == 0 {
            return None;
        }
        loop {
            let n = self.order.len();
            debug_assert!(n > 0, "total_untrusted > 0 implies peers in the order map");
            let mut lowest_top_cost: Option<u64> = None;
            for offset in 0..n {
                let idx = (self.cursor + offset) % n;
                let peer = self.order[idx];
                let Some(lane) = self.lanes.get(&peer) else {
                    continue;
                };
                let Some(cost) = self.top_cost(lane) else {
                    continue; // empty lane
                };
                lowest_top_cost = Some(lowest_top_cost.map_or(cost, |m: u64| m.min(cost)));
                if lane.deficit >= cost {
                    // This peer can afford its top transaction.
                    let lane = self.lanes.get_mut(&peer).expect("lane exists");
                    let entry = lane.queue.pop_front().expect("top exists");
                    lane.deficit -= cost;
                    if lane.queue.is_empty() {
                        lane.deficit = 0;
                    }
                    self.cursor = (idx + 1) % n;
                    self.total_untrusted -= 1;
                    return Some((peer, entry.bundle));
                }
            }
            // No peer could afford its top transaction: add the lowest top-cost to every peer
            // with queued transactions and try again (chia's deficit replenishment).
            let add = lowest_top_cost?;
            for peer in &self.order {
                if let Some(lane) = self.lanes.get_mut(peer)
                    && !lane.queue.is_empty()
                {
                    lane.deficit = lane.deficit.saturating_add(add);
                }
            }
        }
    }

    /// Drain the whole queue for the validator worker's batch pass — the high-priority lane first
    /// (FIFO), then the untrusted lanes in deficit-round-robin order (repeated chia `pop()`).
    /// The per-peer state is reset afterwards (see the cleanup delta note in the module docs).
    pub fn drain_batch(&mut self) -> Vec<(Bytes32, SpendBundle)> {
        let mut out = Vec::with_capacity(self.high.len() + self.total_untrusted);
        out.extend(self.high.drain(..));
        while let Some(entry) = self.pop_untrusted() {
            out.push(entry);
        }
        // Every lane is empty now (deficits already zeroed per chia's on-empty reset); drop the
        // bookkeeping — chia's `_cleanup_peer_queues` outcome after a full drain.
        self.lanes.clear();
        self.order.clear();
        self.cursor = 0;
        out
    }

    /// Drop every queued bundle — the not-synced transition flush (chia
    /// `NO_TRANSACTIONS_WHILE_SYNCING`, the worker clears the inbox rather than validate stale
    /// spends).
    pub fn clear(&mut self) {
        self.high.clear();
        self.lanes.clear();
        self.order.clear();
        self.cursor = 0;
        self.total_untrusted = 0;
    }

    /// Total queued across both lanes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.high.len() + self.total_untrusted
    }

    /// Whether both lanes are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.high.is_empty() && self.total_untrusted == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::blockchain::sized_bytes::Bytes96;

    // The chia test fallback: `TEST_MAX_TX_CLVM_COST` in test_tx_processing_queue.py uses
    // MAX_BLOCK_COST_CLVM // 2; the DRR vector below uses the small 20 chia's own DRR test uses.
    const BIG_COST: u64 = 11_000_000_000 / 2;

    fn empty_bundle() -> SpendBundle {
        SpendBundle {
            coin_spends: vec![],
            aggregated_signature: Bytes96::from([0u8; 96]),
        }
    }

    fn peer(byte: u8) -> Bytes32 {
        Bytes32::from([byte; 32])
    }

    // Gate 3: a trusted peer's bundle jumps an already-queued untrusted bundle — chia
    // high_priority routes to the separate lane pop() drains first.
    #[test]
    fn trusted_bundle_jumps_untrusted_backlog() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        let untrusted = peer(0x11);
        let trusted = peer(0x22);
        assert!(q.push(untrusted, empty_bundle(), false, 100, 1000));
        assert!(q.push(trusted, empty_bundle(), true, 0, 0));
        let batch = q.drain_batch();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].0, trusted, "trusted (high-priority) drains first");
        assert_eq!(batch[1].0, untrusted, "untrusted backlog follows");
        assert!(q.is_empty());
    }

    // WITHIN one peer's lane the queue drains by advertised fee-per-cost, highest FIRST (chia's
    // per-peer PriorityQueue). Low-fpc inserted first, high-fpc second — the high one pops first.
    #[test]
    fn within_a_peer_lane_highest_fee_per_cost_drains_first() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        let p = peer(0x11);
        let low = SpendBundle {
            coin_spends: vec![],
            aggregated_signature: Bytes96::from([1u8; 96]),
        };
        let high = SpendBundle {
            coin_spends: vec![],
            aggregated_signature: Bytes96::from([2u8; 96]),
        };
        assert!(q.push(p, low, false, 100, 1000));
        assert!(q.push(p, high.clone(), false, 900, 1000));
        let batch = q.drain_batch();
        assert_eq!(batch.len(), 2);
        assert_eq!(
            batch[0].1.aggregated_signature, high.aggregated_signature,
            "the peer's higher-fpc bundle validates first"
        );
    }

    // chia 9491c6ee3 (CHIA-3856): validation order round-robins ACROSS peers by CLVM-cost
    // deficit — one peer's high-fpc stream must NOT be serviced ahead of every other peer's
    // backlog. Two peers, three equal-cost bundles each, one peer advertising 90x the fee:
    // the drain must interleave A,B,A,B,A,B (each pop spends the peer's deficit and the cursor
    // moves on), not A,A,A,B,B,B.
    #[test]
    fn drain_interleaves_peers_by_cost_deficit_round_robin() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        let rich = peer(0xAA);
        let poor = peer(0xBB);
        for _ in 0..3 {
            assert!(q.push(rich, empty_bundle(), false, 900, 10));
        }
        for _ in 0..3 {
            assert!(q.push(poor, empty_bundle(), false, 10, 10));
        }
        let batch = q.drain_batch();
        let order: Vec<Bytes32> = batch.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            order,
            vec![rich, poor, rich, poor, rich, poor],
            "equal-cost backlogs from two peers must interleave (deficit round robin), \
             not drain the high-fee peer to exhaustion first"
        );
    }

    // The direct port of chia's `test_normal_queue_deficit_round_robin` vector
    // (test_tx_processing_queue.py, added in 9491c6ee3): four peers with top costs 15 / 5 / 10 /
    // no-cost-info (fallback max_tx_clvm_cost = 20), equal fee 42. Deficit replenishment picks
    // the LOWEST top cost each round, so the service order is peer2 (cost 5), peer3 (10),
    // peer1 (15), peer4 (fallback 20).
    #[test]
    fn chia_deficit_round_robin_vector_orders_by_affordability() {
        let mut q = TxQueue::new(256, 32, 20);
        let p1 = peer(1);
        let p2 = peer(2);
        let p3 = peer(3);
        let p4 = peer(4);
        assert!(q.push(p1, empty_bundle(), false, 42, 15));
        assert!(q.push(p2, empty_bundle(), false, 42, 5));
        assert!(q.push(p3, empty_bundle(), false, 42, 10));
        assert!(q.push(p4, empty_bundle(), false, 42, 0)); // no cost info → fallback 20
        let order: Vec<Bytes32> = q.drain_batch().iter().map(|(p, _)| *p).collect();
        assert_eq!(
            order,
            vec![p2, p3, p1, p4],
            "chia's DRR vector: cheapest affordable top transaction services first, \
             the no-cost-info peer prices at max_tx_clvm_cost and goes last"
        );
    }

    // A zero/unknown advertised cost prices at the max_tx_clvm_cost fallback, so it drains after
    // a known-cost entry (chia's `+inf` lane priority + highest-cost DRR fallback).
    #[test]
    fn unknown_cost_untrusted_entry_drains_last() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        let unknown = peer(0x11);
        let known = peer(0x22);
        assert!(q.push(unknown, empty_bundle(), false, 0, 0));
        assert!(q.push(known, empty_bundle(), false, 10, 1000));
        let batch = q.drain_batch();
        assert_eq!(
            batch[0].0, known,
            "known fee-per-cost drains before unknown-cost"
        );
        assert_eq!(batch[1].0, unknown, "unknown-cost drains last");
    }

    // Equal fee-per-cost and equal cost across two peers: the round robin services them in
    // arrival (registration) order — first-registered peer first.
    #[test]
    fn equal_fee_per_cost_keeps_insertion_order() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        let first = peer(0x11);
        let second = peer(0x22);
        assert!(q.push(first, empty_bundle(), false, 500, 1000));
        assert!(q.push(second, empty_bundle(), false, 500, 1000));
        let batch = q.drain_batch();
        assert_eq!(
            batch[0].0, first,
            "equal fpc: first-registered drains first"
        );
        assert_eq!(batch[1].0, second);
    }

    // The untrusted lane keeps the anti-spam bounds; the high lane does not.
    #[test]
    fn untrusted_lane_bounds_hold_high_lane_is_unbounded() {
        let mut q = TxQueue::new(3, 2, BIG_COST);
        let spammer = peer(0x11);
        // per-peer cap = 2: the third untrusted push from one peer is dropped.
        assert!(q.push(spammer, empty_bundle(), false, 1, 1000));
        assert!(q.push(spammer, empty_bundle(), false, 1, 1000));
        assert!(
            !q.push(spammer, empty_bundle(), false, 1, 1000),
            "per-peer cap holds"
        );
        // A trusted peer is never throttled — well past the aggregate cap.
        for _ in 0..10 {
            assert!(q.push(peer(0x22), empty_bundle(), true, 0, 0));
        }
        assert_eq!(q.len(), 12);
    }

    // Multiple high-priority entries drain in FIFO order among themselves (chia SimpleQueue).
    #[test]
    fn high_priority_lane_is_fifo() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        assert!(q.push(peer(0x01), empty_bundle(), true, 0, 0));
        assert!(q.push(peer(0x02), empty_bundle(), true, 0, 0));
        let batch = q.drain_batch();
        assert_eq!(batch[0].0, peer(0x01));
        assert_eq!(batch[1].0, peer(0x02));
    }

    // Draining resets the round-robin bookkeeping; a fresh backlog starts from a clean cursor
    // and clean deficits (chia's on-empty deficit reset + queue cleanup).
    #[test]
    fn state_resets_between_batches() {
        let mut q = TxQueue::new(256, 32, BIG_COST);
        let a = peer(0x0A);
        let b = peer(0x0B);
        assert!(q.push(a, empty_bundle(), false, 1, 10));
        assert!(q.push(b, empty_bundle(), false, 1, 10));
        let first = q.drain_batch();
        assert_eq!(first.len(), 2);
        assert!(q.is_empty());
        // Second round, reversed registration order: b registers first now and services first.
        assert!(q.push(b, empty_bundle(), false, 1, 10));
        assert!(q.push(a, empty_bundle(), false, 1, 10));
        let second = q.drain_batch();
        assert_eq!(
            second.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![b, a],
            "a drained queue carries no cursor/deficit residue into the next batch"
        );
    }
}
