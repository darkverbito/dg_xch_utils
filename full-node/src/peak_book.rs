//! Per-peer peak-claim book — the chia `sync_store.py` analog.
//!
//! chia tracks every peer's announced peak per connection (`sync_store.peer_to_peak`, written by
//! `full_node.py::new_peak` via `peer_has_block`), selects the long-sync target as the HEAVIEST
//! collected claim (`sync_store.get_heaviest_peak` — weight, not height, is the fork-choice ordering
//! key), retracts a peer's claim when its connection dies (`full_node.py::on_disconnect` →
//! `sync_store.peer_disconnected`), and quarantines a peak hash whose weight proof failed so it is
//! never re-selected (`full_node.bad_peak_cache`, `full_node.py::in_bad_peak_cache` /
//! `add_to_bad_peak_cache`). This module is those four mechanisms in one bounded structure; the
//! daemon's `sync_target` layers chia's "not interested in less heavy peaks" gate
//! (`full_node.py::new_peak` weight drop + `request_validate_wp`'s already-caught-up refusal) on top.
//!
//! Bounds: claims are one entry per live connection, hard-capped at [`MAX_TRACKED_CLAIMS`] (chia caps
//! `peak_to_peer` at 256); the quarantine cache is capped at [`BAD_PEAK_CACHE_SIZE`] evicting the
//! lowest height (chia `bad_peak_cache_size: 100`, min-height eviction). Claims additionally expire
//! after [`STALE_CLAIM_TTL`] without a re-announcement — a liveness backstop chia gets from its
//! explicit disconnect callback: our outbound retraction rides the per-connection handler map's
//! `Drop` ([`ClaimGuard`]), whose timing follows the last `Arc` release rather than the socket close,
//! so a claim nobody refreshes must eventually stop steering the sync band on its own. An honest
//! peer re-announces on every network peak (~18.75 s mainnet cadence), so live claims never expire.

use dg_xch_core::blockchain::sized_bytes::Bytes32;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// chia `sync_store.peak_to_peer` cap ("256  # nice power of two").
pub const MAX_TRACKED_CLAIMS: usize = 256;
/// chia `bad_peak_cache_size` config default.
pub const BAD_PEAK_CACHE_SIZE: usize = 100;
/// Claim-liveness backstop: a claim not re-announced within this window stops being selectable.
pub const STALE_CLAIM_TTL: Duration = Duration::from_secs(300);

/// One peer's announced peak — chia `sync_store.Peak` (`header_hash`, `height`, `weight`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PeakClaim {
    pub header_hash: Bytes32,
    pub height: u32,
    pub weight: u128,
}

struct Entry {
    claim: PeakClaim,
    /// Inbound claims are keyed by the server-side peer id and reconciled against the live inbound
    /// map each driver tick; outbound claims are keyed by a minted per-connection id and retracted
    /// by the connection's [`ClaimGuard`] drop.
    inbound: bool,
    recorded_at: Instant,
}

struct Inner {
    claims: HashMap<Bytes32, Entry>,
    /// Quarantined peak hashes with the height they claimed — chia `bad_peak_cache`.
    bad: Vec<(Bytes32, u32)>,
    /// The last published heaviest claim, for change detection (the tip-follower wake signal).
    published: Option<PeakClaim>,
}

/// The per-connection peak-claim book. `published_height` mirrors the heaviest selectable claim's
/// height into the daemon's `claimed_peak` gauge (metrics + the declare plot-filter height), and —
/// unlike the fetch_max slot it replaces — rolls BACK when the top claim is retracted or quarantined.
pub struct PeakBook {
    published_height: Arc<AtomicU32>,
    next_outbound_key: AtomicU64,
    inner: Mutex<Inner>,
}

impl PeakBook {
    #[must_use]
    pub fn new(published_height: Arc<AtomicU32>) -> Self {
        Self {
            published_height,
            next_outbound_key: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                claims: HashMap::new(),
                bad: Vec::new(),
                published: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mint the claim key + RAII retraction for one OUTBOUND connection. The outbound dispatch path
    /// hands every connection our own cert hash as the peer id (the dial's `peer_id` is derived from
    /// the client cert), so per-connection identity must be minted here instead; the guard's `Drop`
    /// is the `sync_store.peer_disconnected` retraction, fired when the connection's handler map goes.
    #[must_use]
    pub fn outbound_guard(self: &Arc<Self>) -> ClaimGuard {
        let n = self.next_outbound_key.fetch_add(1, Ordering::Relaxed);
        // A minted key can never collide with a real inbound peer id (a cert hash): tagged prefix.
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(b"outbound");
        key[24..].copy_from_slice(&n.to_be_bytes());
        ClaimGuard {
            book: self.clone(),
            key: Bytes32::const_new(key),
        }
    }

    /// Record `key`'s announced peak, replacing its previous claim — chia
    /// `sync_store.peer_has_block(new_peak=True)`: one claim per peer, newest announcement wins
    /// (which is also how an honest peer WITHDRAWS an over-claim: its next announcement replaces it).
    /// Returns `true` when the published heaviest claim changed (the tip-follower wake condition).
    pub fn record(&self, key: Bytes32, inbound: bool, claim: PeakClaim) -> bool {
        let mut g = self.lock();
        let now = Instant::now();
        if g.claims.len() >= MAX_TRACKED_CLAIMS && !g.claims.contains_key(&key) {
            // At the cap, evict the stalest entry (chia pops the oldest `peak_to_peer` entry).
            if let Some(oldest) = g
                .claims
                .iter()
                .min_by_key(|(_, e)| e.recorded_at)
                .map(|(k, _)| *k)
            {
                g.claims.remove(&oldest);
            }
        }
        g.claims.insert(
            key,
            Entry {
                claim,
                inbound,
                recorded_at: now,
            },
        );
        self.republish(&mut g)
    }

    /// Retract `key`'s claim — chia `sync_store.peer_disconnected`.
    pub fn retract(&self, key: &Bytes32) {
        let mut g = self.lock();
        if g.claims.remove(key).is_some() {
            self.republish(&mut g);
        }
    }

    /// Retract every claim on `header_hash`, whoever made it. Driven when no peer will actually
    /// serve the claimed tip (the weight-proof fetch failed from every peer): an honest claimant
    /// re-announces within a block cadence, a phantom claim stays gone — the soft analog of chia
    /// closing the peer that failed to serve the proof (`request_validate_wp` → `peer.close`).
    pub fn retract_hash(&self, header_hash: &Bytes32) {
        let mut g = self.lock();
        let before = g.claims.len();
        g.claims.retain(|_, e| e.claim.header_hash != *header_hash);
        if g.claims.len() != before {
            self.republish(&mut g);
        }
    }

    /// Per-tick reconcile: drop inbound claims whose peer left the live inbound map (chia
    /// `on_disconnect` → `peer_disconnected`) and every claim past [`STALE_CLAIM_TTL`], then
    /// republish so a retraction rolls the claimed gauge back within one driver tick.
    pub fn reconcile(&self, live_inbound: &std::collections::HashSet<Bytes32>) {
        let mut g = self.lock();
        let now = Instant::now();
        g.claims.retain(|key, e| {
            (!e.inbound || live_inbound.contains(key))
                && now.duration_since(e.recorded_at) <= STALE_CLAIM_TTL
        });
        self.republish(&mut g);
    }

    /// Quarantine a peak hash whose weight proof failed to attest it — chia `add_to_bad_peak_cache`.
    /// A quarantined hash is never selectable again (until evicted by the cache bound), so a
    /// poisoned peak cannot be re-selected every tick.
    pub fn quarantine(&self, header_hash: Bytes32, height: u32) {
        let mut g = self.lock();
        if !g.bad.iter().any(|(h, _)| *h == header_hash) {
            g.bad.push((header_hash, height));
            if g.bad.len() > BAD_PEAK_CACHE_SIZE {
                // chia evicts the minimum-height entry when over the cap.
                if let Some(min_idx) = g
                    .bad
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, (_, h))| *h)
                    .map(|(i, _)| i)
                {
                    g.bad.swap_remove(min_idx);
                }
            }
        }
        self.republish(&mut g);
    }

    #[must_use]
    pub fn is_quarantined(&self, header_hash: &Bytes32) -> bool {
        self.lock().bad.iter().any(|(h, _)| h == header_hash)
    }

    /// The heaviest selectable claim — chia `sync_store.get_heaviest_peak`, minus quarantined
    /// hashes and stale entries. Ties break to the taller claim for determinism.
    #[must_use]
    pub fn heaviest(&self) -> Option<PeakClaim> {
        let g = self.lock();
        Self::heaviest_of(&g, Instant::now())
    }

    fn heaviest_of(g: &Inner, now: Instant) -> Option<PeakClaim> {
        g.claims
            .values()
            .filter(|e| now.duration_since(e.recorded_at) <= STALE_CLAIM_TTL)
            .map(|e| &e.claim)
            .filter(|c| !g.bad.iter().any(|(h, _)| *h == c.header_hash))
            .max_by_key(|c| (c.weight, c.height))
            .copied()
    }

    // Recompute + publish the heaviest claim; report whether it changed.
    fn republish(&self, g: &mut Inner) -> bool {
        let heaviest = Self::heaviest_of(g, Instant::now());
        self.published_height
            .store(heaviest.map_or(0, |c| c.height), Ordering::Relaxed);
        let changed = heaviest != g.published;
        g.published = heaviest;
        changed
    }
}

/// RAII retraction for one outbound connection's claim: dropped with the connection's handler map,
/// it retracts the claim exactly as chia's `peer_disconnected` does.
pub struct ClaimGuard {
    book: Arc<PeakBook>,
    key: Bytes32,
}

impl ClaimGuard {
    #[must_use]
    pub fn key(&self) -> Bytes32 {
        self.key
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        self.book.retract(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(hash: [u8; 32], height: u32, weight: u128) -> PeakClaim {
        PeakClaim {
            header_hash: Bytes32::const_new(hash),
            height,
            weight,
        }
    }

    fn book() -> (Arc<PeakBook>, Arc<AtomicU32>) {
        let published = Arc::new(AtomicU32::new(0));
        (Arc::new(PeakBook::new(published.clone())), published)
    }

    // chia sync_store.get_heaviest_peak: weight orders the target; height only breaks ties.
    #[test]
    fn heaviest_is_by_weight_not_height() {
        let (b, published) = book();
        b.record(
            Bytes32::const_new([1; 32]),
            true,
            claim([0xAA; 32], 100, 1_000),
        );
        b.record(
            Bytes32::const_new([2; 32]),
            true,
            claim([0xBB; 32], 120, 900),
        );
        assert_eq!(b.heaviest(), Some(claim([0xAA; 32], 100, 1_000)));
        assert_eq!(published.load(Ordering::Relaxed), 100);
    }

    // chia sync_store.peer_has_block(new_peak=True): a peer's newest announcement REPLACES its claim
    // — the withdrawal path for an over-claim.
    #[test]
    fn a_peers_new_announcement_replaces_its_claim() {
        let (b, published) = book();
        let peer = Bytes32::const_new([1; 32]);
        b.record(peer, true, claim([0xAA; 32], 500, 5_000));
        b.record(peer, true, claim([0xBB; 32], 100, 1_000));
        assert_eq!(b.heaviest(), Some(claim([0xBB; 32], 100, 1_000)));
        assert_eq!(published.load(Ordering::Relaxed), 100);
    }

    // chia sync_store.peer_disconnected: retraction rolls the published claim BACK (the fetch_max
    // slot this replaces could never regress).
    #[test]
    fn retract_rolls_the_published_claim_back() {
        let (b, published) = book();
        let bogus = Bytes32::const_new([9; 32]);
        b.record(bogus, true, claim([0xEE; 32], 9_999_999, u128::MAX));
        b.record(
            Bytes32::const_new([1; 32]),
            true,
            claim([0xAA; 32], 100, 1_000),
        );
        assert_eq!(published.load(Ordering::Relaxed), 9_999_999);
        b.retract(&bogus);
        assert_eq!(b.heaviest(), Some(claim([0xAA; 32], 100, 1_000)));
        assert_eq!(published.load(Ordering::Relaxed), 100);
    }

    // The outbound guard IS the disconnect callback: dropping it retracts the connection's claim.
    #[test]
    fn claim_guard_drop_retracts_the_outbound_claim() {
        let (b, published) = book();
        let guard = b.outbound_guard();
        b.record(guard.key(), false, claim([0xEE; 32], 9_999_999, u128::MAX));
        assert_eq!(published.load(Ordering::Relaxed), 9_999_999);
        drop(guard);
        assert_eq!(b.heaviest(), None);
        assert_eq!(published.load(Ordering::Relaxed), 0);
    }

    // chia on_disconnect reconcile for the shared inbound handler: claims of departed inbound peers
    // are dropped; outbound (guard-keyed) claims are untouched by the inbound reconcile.
    #[test]
    fn reconcile_drops_departed_inbound_claims_only() {
        let (b, published) = book();
        let inbound = Bytes32::const_new([1; 32]);
        let guard = b.outbound_guard();
        b.record(inbound, true, claim([0xEE; 32], 9_000_000, 9_000));
        b.record(guard.key(), false, claim([0xAA; 32], 100, 1_000));
        b.reconcile(&std::collections::HashSet::new()); // the inbound peer is gone
        assert_eq!(b.heaviest(), Some(claim([0xAA; 32], 100, 1_000)));
        assert_eq!(published.load(Ordering::Relaxed), 100);
    }

    // chia bad_peak_cache: a quarantined hash is never re-selected; the next-heaviest claim is.
    #[test]
    fn quarantined_peak_is_not_reselected() {
        let (b, published) = book();
        b.record(
            Bytes32::const_new([1; 32]),
            true,
            claim([0xEE; 32], 9_000_000, 9_000),
        );
        b.record(
            Bytes32::const_new([2; 32]),
            true,
            claim([0xAA; 32], 100, 1_000),
        );
        b.quarantine(Bytes32::const_new([0xEE; 32]), 9_000_000);
        assert!(b.is_quarantined(&Bytes32::const_new([0xEE; 32])));
        assert_eq!(b.heaviest(), Some(claim([0xAA; 32], 100, 1_000)));
        assert_eq!(published.load(Ordering::Relaxed), 100);
        // A RE-announcement of the quarantined hash stays unselectable.
        b.record(
            Bytes32::const_new([3; 32]),
            true,
            claim([0xEE; 32], 9_000_000, 9_000),
        );
        assert_eq!(b.heaviest(), Some(claim([0xAA; 32], 100, 1_000)));
    }

    // Bounds: the quarantine cache caps at BAD_PEAK_CACHE_SIZE evicting the lowest height (chia
    // add_to_bad_peak_cache), and the claim map caps at MAX_TRACKED_CLAIMS.
    #[test]
    fn quarantine_cache_and_claim_map_are_bounded() {
        let (b, _) = book();
        for i in 0..=BAD_PEAK_CACHE_SIZE {
            let mut h = [0u8; 32];
            h[..8].copy_from_slice(&(i as u64).to_be_bytes());
            b.quarantine(Bytes32::const_new(h), u32::try_from(i).unwrap());
        }
        assert_eq!(b.lock().bad.len(), BAD_PEAK_CACHE_SIZE);
        // The min-height entry (height 0) was evicted.
        assert!(!b.is_quarantined(&Bytes32::const_new([0u8; 32])));

        for i in 0..(MAX_TRACKED_CLAIMS + 8) {
            let mut k = [0u8; 32];
            k[..8].copy_from_slice(&(i as u64).to_be_bytes());
            b.record(Bytes32::const_new(k), true, claim([0x11; 32], 1, 1));
        }
        assert_eq!(b.lock().claims.len(), MAX_TRACKED_CLAIMS);
    }

    // retract_hash drops EVERY claimant of a never-served tip (the all-peers weight-proof-fetch
    // failure path); honest peers re-announce on the next peak and repopulate.
    #[test]
    fn retract_hash_drops_all_claimants_of_that_tip() {
        let (b, published) = book();
        b.record(
            Bytes32::const_new([1; 32]),
            true,
            claim([0xEE; 32], 9_000_000, 9_000),
        );
        b.record(
            Bytes32::const_new([2; 32]),
            true,
            claim([0xEE; 32], 9_000_000, 9_000),
        );
        b.record(
            Bytes32::const_new([3; 32]),
            true,
            claim([0xAA; 32], 100, 1_000),
        );
        b.retract_hash(&Bytes32::const_new([0xEE; 32]));
        assert_eq!(b.heaviest(), Some(claim([0xAA; 32], 100, 1_000)));
        assert_eq!(published.load(Ordering::Relaxed), 100);
    }
}
