use std::collections::{BTreeSet, HashMap, HashSet};

// One splittable reservation: a contiguous run of candidate heights handed to a single peer. Holds
// identifiers (~a u32 each), never blocks — the whole point of the O(W·id) bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub id: u64,
    pub heights: Vec<u32>,
}
impl Reservation {
    #[must_use]
    pub fn start(&self) -> u32 {
        self.heights[0]
    }
    #[must_use]
    pub fn end(&self) -> u32 {
        self.heights[self.heights.len() - 1]
    }
}

/// The bounded reservation window. Pending candidate heights are split into per-peer
/// contiguous reservations; a stalled peer's reservation is reclaimed to the pool with no gap. The window
/// holds only identifiers, capped at `capacity`, so peak RAM is flat in chain height.
pub struct ReservationWindow {
    capacity: usize,
    known: BTreeSet<u32>,
    reserved: HashMap<u64, Vec<u32>>,
    reserved_set: HashSet<u32>,
    next_id: u64,
}

/// The result of asking the window for work.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// A reservation to download.
    Reserved(Reservation),
    /// Nothing pending and nothing outstanding — the range is fully written through.
    Drained,
    /// Everything pending is currently reserved by other peers — wait for a completion or a reclaim.
    Busy,
}

impl ReservationWindow {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            known: BTreeSet::new(),
            reserved: HashMap::new(),
            reserved_set: HashSet::new(),
            next_id: 0,
        }
    }

    // Admit newly-discovered pending heights, up to the window cap — never a block, just the identifier.
    // Already-reserved or already-known heights are ignored (idempotent refill from get_unassociated).
    pub fn refill(&mut self, heights: impl IntoIterator<Item = u32>) {
        for h in heights {
            if self.live() >= self.capacity {
                break;
            }
            if !self.reserved_set.contains(&h) {
                self.known.insert(h);
            }
        }
    }

    // Split off the next contiguous run of up to `batch` lowest pending heights for one peer.
    pub fn reserve(&mut self, batch: u32) -> Claim {
        if self.known.is_empty() {
            return if self.reserved.is_empty() {
                Claim::Drained
            } else {
                Claim::Busy
            };
        }
        let start = *self.known.iter().next().expect("non-empty");
        let mut heights = Vec::new();
        let mut h = start;
        while heights.len() < batch as usize && self.known.remove(&h) {
            self.reserved_set.insert(h);
            heights.push(h);
            h += 1;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.reserved.insert(id, heights.clone());
        Claim::Reserved(Reservation { id, heights })
    }

    // A reservation's bodies are written through: retire it.
    pub fn complete(&mut self, id: u64) {
        if let Some(heights) = self.reserved.remove(&id) {
            for h in heights {
                self.reserved_set.remove(&h);
            }
        }
    }

    // A peer stalled: return its heights to the pool for another peer, no gap.
    pub fn reclaim(&mut self, id: u64) {
        if let Some(heights) = self.reserved.remove(&id) {
            for h in heights {
                self.reserved_set.remove(&h);
                self.known.insert(h);
            }
        }
    }

    // Total live identifiers held (pending + reserved) — the O(W·id) quantity, capped at `capacity`.
    #[must_use]
    pub fn live(&self) -> usize {
        self.known.len() + self.reserved_set.len()
    }

    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.reserved.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_reservation_returns_to_the_pool_with_no_gap() {
        let mut w = ReservationWindow::new(64);
        w.refill(100..110);
        assert_eq!(w.live(), 10);

        // Two peers each split off a contiguous run.
        let Claim::Reserved(a) = w.reserve(3) else {
            panic!("first reservation")
        };
        let Claim::Reserved(b) = w.reserve(3) else {
            panic!("second reservation")
        };
        assert_eq!(a.heights, vec![100, 101, 102]);
        assert_eq!(b.heights, vec![103, 104, 105]);

        // Peer A stalls: its run goes back to the pool, no height is dropped.
        w.reclaim(a.id);
        // Peer B completes normally.
        w.complete(b.id);

        // Every not-yet-written height (the reclaimed 100..103 plus the still-pending 106..110) is claimable;
        // the completed 103..106 are gone. Union of all future reservations == exactly the un-written set.
        // Collect until Drained (nothing left) or Busy (all remaining is now reserved by this loop).
        let mut got: Vec<u32> = Vec::new();
        while let Claim::Reserved(r) = w.reserve(4) {
            got.extend(r.heights);
        }
        got.sort_unstable();
        assert_eq!(got, vec![100, 101, 102, 106, 107, 108, 109]);
        assert_eq!(w.live(), 7);
    }

    #[test]
    fn window_is_capacity_bounded() {
        let mut w = ReservationWindow::new(8);
        w.refill(0..1000);
        assert_eq!(
            w.live(),
            8,
            "the window never holds more than its cap of identifiers"
        );
    }
}
