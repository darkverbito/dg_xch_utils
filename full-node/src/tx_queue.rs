// The gossip-transaction admission queue with a trusted priority lane — the bounded inbox
// `on_transaction` fills off the websocket read loop and `spawn_tx_validator` drains.
//
// A high-priority (trusted, or local) entry lands in an unbounded lane that drains ENTIRELY
// before the untrusted backlog — high priority is a separate lane serviced first, not a
// reordering within one lane. The untrusted lane orders by advertised fee-per-cost (highest
// first, unknown-cost last) under aggregate + per-peer caps.
//
// There is no cross-peer round-robin: a peer that fills its share with high-fpc bundles is
// validated ahead of a peer with lower-fpc bundles. The advertised fpc affects ONLY queue order,
// never admission (every bundle is validated at its TRUE fee) — a fairness-ordering delta, not a
// correctness one.

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::spend_bundle::SpendBundle;
use std::cmp::Ordering;
use std::collections::VecDeque;

// One untrusted-lane entry: the peer it came from, the bundle, and the ADVERTISED fee + cost
// (from the peer's `NewTransaction`) the lane orders on. Advertised values order the queue only —
// admission always re-validates at the bundle's true fee.
struct UntrustedTx {
    peer: Bytes32,
    bundle: SpendBundle,
    advertised_fee: u64,
    advertised_cost: u64,
}

// Order two untrusted entries by advertised fee-per-cost, HIGHEST first. fpc = fee/cost compared
// without division via cross-multiplication in u128 (advertised_fee ≤ ~2^50, advertised_cost
// < 2^33, so the product never overflows u128). An entry with zero/unknown advertised cost has
// no fpc and sorts LAST.
fn fee_per_cost_desc(a: &UntrustedTx, b: &UntrustedTx) -> Ordering {
    match (a.advertised_cost, b.advertised_cost) {
        (0, 0) => Ordering::Equal,
        (0, _) => Ordering::Greater, // a has no fpc → sorts after b
        (_, 0) => Ordering::Less,    // b has no fpc → a sorts before b
        (ac, bc) => {
            let lhs = u128::from(a.advertised_fee) * u128::from(bc);
            let rhs = u128::from(b.advertised_fee) * u128::from(ac);
            // DESC: the higher fpc compares Less so it lands first after sort.
            rhs.cmp(&lhs)
        }
    }
}

/// A bounded gossip-transaction inbox with a trusted high-priority lane. Trusted-peer bundles
/// land in the unbounded `high` lane (drained first); untrusted-peer bundles land in the capped
/// `normal` lane under an aggregate + per-peer bound and drain highest-fee-per-cost first.
pub struct TxQueue {
    // Trusted (and local) transactions, drained in full before `normal`. Unbounded — trust is
    // the admission control, so a trusted peer is not throttled here. FIFO.
    high: VecDeque<(Bytes32, SpendBundle)>,
    // The untrusted backlog under `cap` (aggregate) + `per_peer` (anti-spam) bounds, drained by
    // advertised fee-per-cost (highest first).
    normal: Vec<UntrustedTx>,
    // Aggregate cap on the untrusted lane (`TX_INBOX_CAP`).
    cap: usize,
    // Per-peer cap on the untrusted lane — a single flooding peer cannot fill the backlog.
    per_peer: usize,
}

impl TxQueue {
    /// A queue with the given untrusted-lane bounds. The high-priority lane is unbounded.
    #[must_use]
    pub fn new(cap: usize, per_peer: usize) -> Self {
        Self {
            high: VecDeque::new(),
            normal: Vec::new(),
            cap,
            per_peer,
        }
    }

    /// Enqueue `bundle` from `peer`. `high_priority` (a trusted peer) routes to the unbounded
    /// priority lane and always succeeds (advertised fee/cost ignored — the trusted lane is
    /// FIFO); an untrusted bundle is admitted to the capped lane only if BOTH the aggregate and
    /// this peer's share have room — otherwise it is dropped on the floor.
    /// `advertised_fee`/`advertised_cost` are the peer's `NewTransaction` values the untrusted
    /// lane orders on. Returns whether it was admitted.
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
        if self.normal.len() >= self.cap {
            return false;
        }
        if self.normal.iter().filter(|e| e.peer == peer).count() >= self.per_peer {
            return false;
        }
        self.normal.push(UntrustedTx {
            peer,
            bundle,
            advertised_fee,
            advertised_cost,
        });
        true
    }

    /// Drain the whole queue for the validator worker's batch pass — the high-priority lane
    /// first (in FIFO order), then the untrusted backlog ordered by advertised fee-per-cost
    /// (highest first, unknown-cost last). The sort is stable, so equal-fpc untrusted entries
    /// keep insertion (FIFO) order.
    pub fn drain_batch(&mut self) -> Vec<(Bytes32, SpendBundle)> {
        let mut out = Vec::with_capacity(self.high.len() + self.normal.len());
        out.extend(self.high.drain(..));
        let mut normal = std::mem::take(&mut self.normal);
        normal.sort_by(fee_per_cost_desc);
        out.extend(normal.into_iter().map(|e| (e.peer, e.bundle)));
        out
    }

    /// Drop every queued bundle — the not-synced transition flush (the worker clears the inbox
    /// rather than validate stale spends).
    pub fn clear(&mut self) {
        self.high.clear();
        self.normal.clear();
    }

    /// Total queued across both lanes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.high.len() + self.normal.len()
    }

    /// Whether both lanes are empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.high.is_empty() && self.normal.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_xch_core::blockchain::sized_bytes::Bytes96;

    fn empty_bundle() -> SpendBundle {
        SpendBundle {
            coin_spends: vec![],
            aggregated_signature: Bytes96::from([0u8; 96]),
        }
    }

    fn peer(byte: u8) -> Bytes32 {
        Bytes32::from([byte; 32])
    }

    // A trusted peer's bundle jumps an already-queued untrusted bundle. Enqueue untrusted FIRST,
    // trusted SECOND, and assert the trusted one comes back FIRST.
    #[test]
    fn trusted_bundle_jumps_untrusted_backlog() {
        let mut q = TxQueue::new(256, 32);
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

    // The untrusted lane drains by advertised fee-per-cost, highest FIRST. Insert the LOW-fpc
    // bundle first and the HIGH-fpc bundle second — a FIFO lane would drain the low one first;
    // the fee-ordered lane drains the high one first. The trusted lane still jumps both
    // regardless of fee.
    #[test]
    fn untrusted_lane_drains_highest_fee_per_cost_first() {
        let mut q = TxQueue::new(256, 32);
        let low = peer(0x11);
        let high = peer(0x22);
        let trusted = peer(0x33);
        // fpc 0.1 inserted FIRST, fpc 0.9 inserted SECOND.
        assert!(q.push(low, empty_bundle(), false, 100, 1000));
        assert!(q.push(high, empty_bundle(), false, 900, 1000));
        // A trusted bundle enqueued LAST still drains before both untrusted ones.
        assert!(q.push(trusted, empty_bundle(), true, 0, 0));
        let batch = q.drain_batch();
        assert_eq!(batch.len(), 3);
        assert_eq!(
            batch[0].0, trusted,
            "trusted high-priority lane jumps everything"
        );
        assert_eq!(
            batch[1].0, high,
            "untrusted: higher advertised fee-per-cost drains first"
        );
        assert_eq!(batch[2].0, low, "untrusted: lower fee-per-cost follows");
    }

    // A zero/unknown advertised cost has no fee-per-cost and sorts LAST among untrusted entries.
    #[test]
    fn unknown_cost_untrusted_entry_drains_last() {
        let mut q = TxQueue::new(256, 32);
        let unknown = peer(0x11);
        let known = peer(0x22);
        // Unknown-cost inserted FIRST; a known-fpc bundle SECOND must still drain ahead of it.
        assert!(q.push(unknown, empty_bundle(), false, 0, 0));
        assert!(q.push(known, empty_bundle(), false, 10, 1000));
        let batch = q.drain_batch();
        assert_eq!(
            batch[0].0, known,
            "known fee-per-cost drains before unknown-cost"
        );
        assert_eq!(batch[1].0, unknown, "unknown-cost drains last");
    }

    // Equal fee-per-cost preserves insertion (FIFO) order — the stable-sort tie behavior.
    #[test]
    fn equal_fee_per_cost_keeps_insertion_order() {
        let mut q = TxQueue::new(256, 32);
        let first = peer(0x11);
        let second = peer(0x22);
        assert!(q.push(first, empty_bundle(), false, 500, 1000));
        assert!(q.push(second, empty_bundle(), false, 500, 1000));
        let batch = q.drain_batch();
        assert_eq!(batch[0].0, first, "equal fpc: first-inserted drains first");
        assert_eq!(batch[1].0, second);
    }

    // The untrusted lane keeps the pre-tier anti-spam bounds; the high lane does not.
    #[test]
    fn untrusted_lane_bounds_hold_high_lane_is_unbounded() {
        let mut q = TxQueue::new(3, 2);
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

    // Multiple high-priority entries drain in FIFO order among themselves.
    #[test]
    fn high_priority_lane_is_fifo() {
        let mut q = TxQueue::new(256, 32);
        assert!(q.push(peer(0x01), empty_bundle(), true, 0, 0));
        assert!(q.push(peer(0x02), empty_bundle(), true, 0, 0));
        let batch = q.drain_batch();
        assert_eq!(batch[0].0, peer(0x01));
        assert_eq!(batch[1].0, peer(0x02));
    }
}
