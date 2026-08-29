//! Health-scored peer selection. Owns the churning peer set and answers peer requests with a
//! *lease*, scored by liveness, EWMA request latency (RFC 6298 smoothing), in-flight load, and a
//! reliability ratio. Fetch selection is power-of-two-choices to avoid herding onto the single
//! fastest peer. Availability uses a 3-strike hysteresis (Fresh → Live → Suspect → Dead). A
//! RECOVERY lease strictly dominates FETCH: under producer saturation it preempts the lowest-score
//! FETCH lease so a reorg always obtains a peer in one bounded step.
//!
//! Keyed purely on `peer_id` (`u64`) so the logic is unit-testable without a live socket; the
//! driver maps a leased `peer_id` back to its `OutboundPeer`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

/// Per-peer in-flight request cap.
pub const MAX_IN_TRANSIT_PER_PEER: usize = 16;
/// RFC 6298 smoothing constants for the SRTT / RTTVAR EWMA.
const ALPHA: f64 = 1.0 / 8.0;
const BETA: f64 = 1.0 / 4.0;
/// Consecutive failures before a Live peer is demoted to Suspect (the 3-strike hysteresis).
const SUSPECT_STRIKES: u32 = 3;
/// A small floor added to SRTT so a zero-latency (unsampled) peer has a finite, comparable score.
const SRTT_FLOOR: f64 = 1e-3;

/// Where a peer sits in the churn lifecycle. `Dead` peers are dropped from selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Availability {
    /// Newly introduced; an optimistic prior lets it be tried without being starved by incumbents.
    Fresh,
    /// Serving normally.
    Live,
    /// `SUSPECT_STRIKES` consecutive failures/timeouts — still eligible (hysteresis), one strike from Dead.
    Suspect,
    /// Connection closed or a Suspect failed again — evicted from selection, its in-flight ranges reclaimed.
    Dead,
}

/// Lease priority classes contending for the peer resource. RECOVERY strictly dominates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeasePriority {
    Fetch,
    Recovery,
}

/// The result the caller reports so the health estimate, in-flight count, and availability update.
#[derive(Clone, Copy, Debug)]
pub enum FetchOutcome {
    /// The peer served `blocks` blocks in `latency`.
    Ok { blocks: usize, latency: Duration },
    /// The request timed out.
    Timeout,
    /// The peer answered `RejectBlocks` (cannot serve the range).
    Reject,
    /// The connection closed.
    Closed,
}

/// A granted claim on a peer. The caller uses `peer_id` to build a source, then reports the outcome via
/// [`PeerManager::release`] with this lease.
#[derive(Clone, Copy, Debug)]
pub struct PeerLease {
    pub peer_id: u64,
    pub lease_id: u64,
    pub priority: LeasePriority,
    /// Set when a RECOVERY lease preempted a FETCH lease to obtain a peer under saturation:
    /// the preempted `lease_id`, so the caller reclaims that peer's in-flight queue range. `None` when a
    /// free peer was available and nothing was preempted.
    pub preempted: Option<u64>,
}

struct PeerHealth {
    srtt: f64,
    rttvar: f64,
    successes: u32,
    failures: u32,
    consecutive_failures: u32,
    /// Active leases on this peer (`lease_id`, priority); its length is the in-flight load for the cap.
    inflight: Vec<(u64, LeasePriority)>,
    availability: Availability,
}

impl PeerHealth {
    fn fresh() -> Self {
        Self {
            srtt: 0.0,
            rttvar: 0.0,
            successes: 0,
            failures: 0,
            consecutive_failures: 0,
            inflight: Vec::new(),
            availability: Availability::Fresh,
        }
    }

    /// Higher = better. Reliability over smoothed latency; an unsampled peer gets an optimistic prior so it
    /// is tried (optimistic initialization).
    fn score(&self) -> f64 {
        let attempts = self.successes + self.failures;
        let reliability = if attempts == 0 {
            1.0
        } else {
            f64::from(self.successes) / f64::from(attempts)
        };
        reliability / (self.srtt + SRTT_FLOOR)
    }

    fn under_cap(&self) -> bool {
        self.inflight.len() < MAX_IN_TRANSIT_PER_PEER
    }

    // Everything but Dead is eligible: a Suspect peer stays in the pool (hysteresis) so a success can
    // recover it (Suspect → Live on a success); only Dead is dropped from selection.
    fn selectable(&self) -> bool {
        !matches!(self.availability, Availability::Dead)
    }

    fn fetch_leases(&self) -> usize {
        self.inflight
            .iter()
            .filter(|(_, p)| *p == LeasePriority::Fetch)
            .count()
    }

    // RFC 6298 §2: first sample seeds SRTT=R, RTTVAR=R/2; thereafter the standard smoothing.
    fn observe_latency(&mut self, latency: Duration) {
        let r = latency.as_secs_f64();
        if self.srtt == 0.0 {
            self.srtt = r;
            self.rttvar = r / 2.0;
        } else {
            self.rttvar = (1.0 - BETA) * self.rttvar + BETA * (self.srtt - r).abs();
            self.srtt = (1.0 - ALPHA) * self.srtt + ALPHA * r;
        }
    }
}

/// The churning peer set: mutable per-peer health behind one lock.
pub struct PeerManager {
    inner: Mutex<Inner>,
}

struct Inner {
    peers: BTreeMap<u64, PeerHealth>,
    next_lease: u64,
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                peers: BTreeMap::new(),
                next_lease: 1,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reconcile the tracked set against the live outbound peers: introduce new ids as `Fresh` (optimistic
    /// prior), and mark any tracked peer absent from `live` as `Dead`. A `Dead` peer with no in-flight
    /// leases is dropped; one still holding leases is retained until its leases are released (so an
    /// in-flight range is still accounted) but is never selected again.
    pub fn observe_live(&self, live: &[u64]) {
        let mut inner = self.lock();
        for &id in live {
            inner.peers.entry(id).or_insert_with(PeerHealth::fresh);
        }
        let live_set: std::collections::HashSet<u64> = live.iter().copied().collect();
        inner.peers.retain(|id, h| {
            if !live_set.contains(id) {
                h.availability = Availability::Dead;
                // Keep a Dead peer only while it still has in-flight leases to account for.
                return !h.inflight.is_empty();
            }
            true
        });
    }

    // Two-choices sample over the eligible (selectable, under-cap) peers: return the higher-score of two
    // random draws (or the single candidate). `O(1)` given the candidate slice.
    fn pick_two_choices(inner: &mut Inner) -> Option<u64> {
        let candidates: Vec<u64> = inner
            .peers
            .iter()
            .filter(|(_, h)| h.selectable() && h.under_cap())
            .map(|(id, _)| *id)
            .collect();
        match candidates.len() {
            0 => None,
            1 => Some(candidates[0]),
            n => {
                // Two distinct samples; with exactly two candidates both are compared.
                let i = rand::random_range(0..n);
                let mut j = rand::random_range(0..n);
                if j == i {
                    j = (j + 1) % n;
                }
                let a = candidates[i];
                let b = candidates[j];
                let sa = inner.peers.get(&a).map_or(f64::MIN, PeerHealth::score);
                let sb = inner.peers.get(&b).map_or(f64::MIN, PeerHealth::score);
                Some(if sa >= sb { a } else { b })
            }
        }
    }

    fn grant(
        inner: &mut Inner,
        peer_id: u64,
        priority: LeasePriority,
        preempted: Option<u64>,
    ) -> PeerLease {
        let lease_id = inner.next_lease;
        inner.next_lease = inner.next_lease.wrapping_add(1);
        if let Some(h) = inner.peers.get_mut(&peer_id) {
            h.inflight.push((lease_id, priority));
        }
        PeerLease {
            peer_id,
            lease_id,
            priority,
            preempted,
        }
    }

    /// Lease the best eligible peer for `priority`, or `None` when none can be obtained.
    ///
    /// - `Fetch`: two-choices over selectable, under-cap peers; `None` if all are saturated/dead.
    /// - `Recovery`: prefer the highest-score free peer. If every peer is saturated, preempt the
    ///   FETCH lease on the lowest-score peer and grant recovery there; the returned lease carries
    ///   the preempted `lease_id` so the caller reclaims that peer's in-flight queue range.
    pub fn lease(&self, priority: LeasePriority) -> Option<PeerLease> {
        let mut inner = self.lock();
        match priority {
            LeasePriority::Fetch => {
                let peer_id = Self::pick_two_choices(&mut inner)?;
                Some(Self::grant(&mut inner, peer_id, priority, None))
            }
            LeasePriority::Recovery => {
                // 1) A free peer exists → take the highest-score one, no preemption.
                let free_best = inner
                    .peers
                    .iter()
                    .filter(|(_, h)| h.selectable() && h.under_cap())
                    .max_by(|a, b| a.1.score().total_cmp(&b.1.score()))
                    .map(|(id, _)| *id);
                if let Some(peer_id) = free_best {
                    return Some(Self::grant(&mut inner, peer_id, priority, None));
                }
                // 2) Producer-saturated → preempt the lowest-score peer that holds a FETCH lease.
                let victim = inner
                    .peers
                    .iter()
                    .filter(|(_, h)| h.selectable() && h.fetch_leases() > 0)
                    .min_by(|a, b| a.1.score().total_cmp(&b.1.score()))
                    .map(|(id, _)| *id)?;
                let preempted_lease = {
                    let h = inner.peers.get_mut(&victim)?;
                    // Remove one FETCH lease (the reclaimed in-flight range) to make room.
                    let pos = h
                        .inflight
                        .iter()
                        .position(|(_, p)| *p == LeasePriority::Fetch)?;
                    h.inflight.remove(pos).0
                };
                Some(Self::grant(
                    &mut inner,
                    victim,
                    priority,
                    Some(preempted_lease),
                ))
            }
        }
    }

    /// Report the outcome of a leased fetch, ending the lease and updating the health estimate,
    /// reliability ratio, and availability. A `Closed` or a further failure on a `Suspect` peer evicts it.
    pub fn release(&self, lease: PeerLease, outcome: FetchOutcome) {
        let mut inner = self.lock();
        let Some(h) = inner.peers.get_mut(&lease.peer_id) else {
            return;
        };
        if let Some(pos) = h.inflight.iter().position(|(id, _)| *id == lease.lease_id) {
            h.inflight.remove(pos);
        }
        match outcome {
            FetchOutcome::Ok { latency, .. } => {
                h.observe_latency(latency);
                h.successes = h.successes.saturating_add(1);
                h.consecutive_failures = 0;
                if matches!(h.availability, Availability::Fresh | Availability::Suspect) {
                    h.availability = Availability::Live;
                }
            }
            FetchOutcome::Timeout | FetchOutcome::Reject => {
                h.failures = h.failures.saturating_add(1);
                h.consecutive_failures = h.consecutive_failures.saturating_add(1);
                h.availability = match h.availability {
                    // A further failure on a Suspect peer evicts it.
                    Availability::Suspect => Availability::Dead,
                    // Three strikes demotes a Live/Fresh peer to Suspect (still eligible, hysteresis).
                    Availability::Live | Availability::Fresh
                        if h.consecutive_failures >= SUSPECT_STRIKES =>
                    {
                        Availability::Suspect
                    }
                    other => other,
                };
            }
            FetchOutcome::Closed => {
                h.availability = Availability::Dead;
            }
        }
        // A Dead peer with no more in-flight leases is dropped from the set entirely.
        if h.availability == Availability::Dead && h.inflight.is_empty() {
            inner.peers.remove(&lease.peer_id);
        }
    }

    /// Number of peers currently eligible for selection (everything but Dead) — the producer's fan-out
    /// width.
    #[must_use]
    pub fn selectable_count(&self) -> usize {
        self.lock()
            .peers
            .values()
            .filter(|h| h.selectable())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(latency_ms: u64) -> FetchOutcome {
        FetchOutcome::Ok {
            blocks: 32,
            latency: Duration::from_millis(latency_ms),
        }
    }

    fn availability(pm: &PeerManager, id: u64) -> Availability {
        pm.lock().peers.get(&id).unwrap().availability
    }

    #[test]
    fn observe_live_introduces_fresh_and_evicts_departed() {
        let pm = PeerManager::new();
        pm.observe_live(&[1, 2, 3]);
        assert_eq!(
            pm.selectable_count(),
            3,
            "new peers enter as selectable Fresh"
        );
        pm.observe_live(&[1, 2]); // peer 3 gone
        assert_eq!(
            pm.selectable_count(),
            2,
            "a departed peer with no leases is dropped"
        );
    }

    #[test]
    fn three_strikes_demotes_to_suspect_then_a_further_failure_evicts() {
        let pm = PeerManager::new();
        pm.observe_live(&[1]);
        for _ in 0..3 {
            let l = pm.lease(LeasePriority::Fetch).unwrap();
            pm.release(l, FetchOutcome::Timeout);
        }
        assert_eq!(
            availability(&pm, 1),
            Availability::Suspect,
            "3 strikes → Suspect"
        );
        // Suspect is still eligible (hysteresis) — one more lease is grantable.
        let l = pm
            .lease(LeasePriority::Fetch)
            .expect("suspect still leasable");
        pm.release(l, FetchOutcome::Timeout);
        // Further failure evicts and, with no in-flight, drops the peer entirely.
        assert!(
            pm.lease(LeasePriority::Fetch).is_none(),
            "dead peer not selectable"
        );
        assert_eq!(pm.selectable_count(), 0);
    }

    #[test]
    fn a_success_recovers_a_suspect_peer() {
        let pm = PeerManager::new();
        pm.observe_live(&[1]);
        for _ in 0..3 {
            let l = pm.lease(LeasePriority::Fetch).unwrap();
            pm.release(l, FetchOutcome::Reject);
        }
        assert_eq!(availability(&pm, 1), Availability::Suspect);
        let l = pm.lease(LeasePriority::Fetch).unwrap();
        pm.release(l, ok(50));
        assert_eq!(
            availability(&pm, 1),
            Availability::Live,
            "a success recovers the peer"
        );
    }

    #[test]
    fn closed_evicts_immediately() {
        let pm = PeerManager::new();
        pm.observe_live(&[1, 2]);
        let l = pm.lease(LeasePriority::Fetch).unwrap();
        let victim = l.peer_id;
        pm.release(l, FetchOutcome::Closed);
        assert_eq!(pm.selectable_count(), 1, "closed peer evicted");
        assert!(!pm.lock().peers.contains_key(&victim));
    }

    #[test]
    fn p2c_over_two_candidates_returns_the_higher_score_peer() {
        let pm = PeerManager::new();
        pm.observe_live(&[1, 2]);
        // Peer 1 is fast+reliable; peer 2 is slow. With exactly two candidates P2C samples both, so the
        // higher score (peer 1) is returned deterministically.
        for _ in 0..5 {
            let l = pm.lease(LeasePriority::Fetch).unwrap();
            pm.release(l, if l.peer_id == 1 { ok(20) } else { ok(400) });
        }
        // Prime both with their characteristic latencies.
        let l1 = PeerLease {
            peer_id: 1,
            lease_id: 0,
            priority: LeasePriority::Fetch,
            preempted: None,
        };
        pm.release(l1, ok(20));
        let l2 = PeerLease {
            peer_id: 2,
            lease_id: 0,
            priority: LeasePriority::Fetch,
            preempted: None,
        };
        pm.release(l2, ok(400));
        // Bind each score to a local first: `pm.lock()` twice in one expression would re-lock the
        // non-reentrant std mutex on the same thread and self-deadlock.
        let score_1 = pm.lock().peers.get(&1).unwrap().score();
        let score_2 = pm.lock().peers.get(&2).unwrap().score();
        assert!(score_1 > score_2, "the fast peer scores higher");
        let leased = pm.lease(LeasePriority::Fetch).unwrap();
        assert_eq!(
            leased.peer_id, 1,
            "P2C over two candidates picks the higher score"
        );
    }

    #[test]
    fn per_peer_cap_stops_leasing_a_saturated_peer() {
        let pm = PeerManager::new();
        pm.observe_live(&[1]);
        let mut leases = Vec::new();
        for _ in 0..MAX_IN_TRANSIT_PER_PEER {
            leases.push(pm.lease(LeasePriority::Fetch).expect("under cap"));
        }
        assert!(
            pm.lease(LeasePriority::Fetch).is_none(),
            "at the per-peer cap, no fetch lease"
        );
    }

    #[test]
    fn recovery_takes_a_free_fast_peer_without_preempting() {
        let pm = PeerManager::new();
        pm.observe_live(&[1, 2]);
        // Make peer 1 the fast one.
        let l1 = PeerLease {
            peer_id: 1,
            lease_id: 0,
            priority: LeasePriority::Fetch,
            preempted: None,
        };
        pm.release(l1, ok(10));
        let l2 = PeerLease {
            peer_id: 2,
            lease_id: 0,
            priority: LeasePriority::Fetch,
            preempted: None,
        };
        pm.release(l2, ok(500));
        let rec = pm.lease(LeasePriority::Recovery).unwrap();
        assert_eq!(rec.peer_id, 1, "recovery takes the highest-score free peer");
        assert!(
            rec.preempted.is_none(),
            "a free peer was available — no preemption"
        );
    }

    #[test]
    fn recovery_preempts_the_lowest_score_fetch_lease_when_producer_saturated() {
        let pm = PeerManager::new();
        pm.observe_live(&[1, 2]);
        // Score peer 1 fast, peer 2 slow.
        pm.release(
            PeerLease {
                peer_id: 1,
                lease_id: 0,
                priority: LeasePriority::Fetch,
                preempted: None,
            },
            ok(10),
        );
        pm.release(
            PeerLease {
                peer_id: 2,
                lease_id: 0,
                priority: LeasePriority::Fetch,
                preempted: None,
            },
            ok(500),
        );
        // Producer saturates BOTH peers to the cap with FETCH leases.
        let mut fetch_leases = Vec::new();
        for _ in 0..(MAX_IN_TRANSIT_PER_PEER * 2) {
            fetch_leases.push(pm.lease(LeasePriority::Fetch).expect("fill to saturation"));
        }
        assert!(
            pm.lease(LeasePriority::Fetch).is_none(),
            "producer-saturated"
        );
        // Recovery must still obtain a peer by preemption.
        let rec = pm
            .lease(LeasePriority::Recovery)
            .expect("recovery never starves");
        assert_eq!(
            rec.peer_id, 2,
            "preempts the LOWEST-score peer (the slow one)"
        );
        assert!(
            rec.preempted.is_some(),
            "a FETCH lease was preempted for the recovery range"
        );
    }

    #[test]
    fn ewma_smooths_toward_the_sampled_latency() {
        let pm = PeerManager::new();
        pm.observe_live(&[1]);
        for _ in 0..20 {
            let l = pm.lease(LeasePriority::Fetch).unwrap();
            pm.release(l, ok(100));
        }
        let srtt = pm.lock().peers.get(&1).unwrap().srtt;
        assert!(
            (srtt - 0.100).abs() < 0.02,
            "SRTT converges to the steady 100ms sample, got {srtt}"
        );
    }
}
