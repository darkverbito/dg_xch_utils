//! Progress watchdog for the decoupled fetch/confirm pipeline (sync-decoupling liveness backstop).
//!
//! The bulk/fast-sync path has an explicit stall reclaim: a `download_worker` whose peer misses the
//! request timeout reclaims its reservation to the pool ([`crate::sync::ReservationWindow::reclaim`],
//! counted in `SyncMetrics::reclaimed`). The DECOUPLED genesis/follow pipeline
//! ([`crate::sync::WindowReadahead`] + [`crate::sync::queue::BlockQueue`]) has **no** analogous
//! reclaim — every individual fetch is timeout-bounded, but nothing detects the pipeline as a WHOLE
//! ceasing to make progress. So an unforeseen wedge anywhere in the consumer chain (a stalled
//! announcer back-pressuring the confirm loop, a lost wakeup, a peer set that all went quiet at once)
//! leaves `low_water` frozen with no bound and no recovery — the hard-stall this type closes.
//!
//! The guarantee: if the queue's `low_water` (the confirmed-peak frontier) stops advancing for
//! `timeout` **while there is work to do, peers are live, and no confirm is legitimately in flight**,
//! force a [`BlockQueue::rebase`] to the current `low_water`. That bumps the queue generation — the
//! `fetch_scheduler` reads the bump as its abort-and-replan signal and re-fetches from the frontier —
//! and wakes any producer parked on `wait_space` (rebase zeroes the byte charge and signals `space`).
//! The reclaim is counted in `SyncMetrics::reclaimed`, so a stuck decoupled pipeline is now VISIBLE in
//! the same gauge the bulk path uses, and — the point — BOUNDED: any stall is broken within `timeout`.
//!
//! Keyed purely on `Height`/`Instant`/booleans, so the decision is unit-tested deterministically with
//! injected clock values, and the reclaim action is tested against a live `BlockQueue`.

use crate::sync::SyncMetrics;
use crate::sync::queue::{BlockQueue, Height};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Tracks the confirmed-peak frontier over time and fires a bounded stall reclaim when it wedges.
pub struct StallWatchdog {
    /// Highest `low_water` observed so far; a strictly-greater value is real forward progress.
    last_low_water: Height,
    /// When `last_low_water` last advanced (or when a not-actionable state last reset the clock).
    last_advance: Instant,
    /// How long the frontier may sit still — while actionable — before a reclaim is forced.
    timeout: Duration,
}

impl StallWatchdog {
    /// Start tracking from `low_water` at `now`, firing after `timeout` of actionable no-progress.
    #[must_use]
    pub fn new(low_water: Height, now: Instant, timeout: Duration) -> Self {
        Self {
            last_low_water: low_water,
            last_advance: now,
            timeout,
        }
    }

    /// The pure decision (see the module docs). Returns `true` exactly when the frontier has not
    /// advanced for `>= timeout` AND the stall is actionable: work remains, peers are live, and no
    /// confirm is legitimately in flight. Every other case — a forward advance, or a not-actionable
    /// state (caught up, no peers, a confirm in flight) — resets the clock and returns `false`, so the
    /// timer only ever accrues during a *true* stall we can do something about. On a fire it resets the
    /// clock, so a persistent wedge is reclaimed again after another full `timeout` (bounded, repeatable
    /// — never a one-shot that gives up).
    pub fn poll(
        &mut self,
        low_water: Height,
        now: Instant,
        has_work: bool,
        peers_live: bool,
        confirm_in_flight: bool,
    ) -> bool {
        if low_water > self.last_low_water {
            self.last_low_water = low_water;
            self.last_advance = now;
            return false;
        }
        if !has_work || !peers_live || confirm_in_flight {
            // Not a stall we should act on — don't accumulate time toward a reclaim.
            self.last_advance = now;
            return false;
        }
        if now.duration_since(self.last_advance) >= self.timeout {
            self.last_advance = now;
            return true;
        }
        false
    }

    /// One driver-tick evaluation against the live queue and metrics. When [`StallWatchdog::poll`]
    /// fires, force `queue.rebase(low_water)` (bump the generation → producer aborts + replans + is
    /// woken off `wait_space`) and count the reclaim in `metrics.reclaimed`. Returns whether a reclaim
    /// was performed. `sync_target` is the heaviest claimed height (the work frontier); `has_work` is
    /// `sync_target > low_water`.
    pub fn tick(
        &mut self,
        queue: &BlockQueue,
        metrics: &SyncMetrics,
        now: Instant,
        sync_target: Height,
        peers_live: bool,
        confirm_in_flight: bool,
    ) -> bool {
        let low_water = queue.low_water();
        let has_work = sync_target > low_water;
        if self.poll(low_water, now, has_work, peers_live, confirm_in_flight) {
            queue.rebase(low_water);
            metrics.reclaimed.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StallWatchdog;
    use crate::sync::SyncMetrics;
    use crate::sync::queue::BlockQueue;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    const TO: Duration = Duration::from_secs(60);

    #[test]
    fn a_frozen_frontier_with_work_and_peers_fires_after_the_timeout() {
        let t0 = Instant::now();
        let mut wd = StallWatchdog::new(100, t0, TO);
        // Just under the timeout with a frozen frontier: no fire yet.
        assert!(!wd.poll(100, t0 + TO - Duration::from_millis(1), true, true, false));
        // At/after the timeout, still frozen, work remains, peers live, no confirm in flight → FIRE.
        assert!(wd.poll(100, t0 + TO, true, true, false), "bounded stall must be reclaimed");
    }

    #[test]
    fn forward_progress_resets_the_clock_and_never_fires() {
        let t0 = Instant::now();
        let mut wd = StallWatchdog::new(100, t0, TO);
        // The frontier advances every tick well within the timeout — a healthy sync never reclaims.
        for k in 1..=10u64 {
            let now = t0 + Duration::from_secs(k * 10);
            assert!(
                !wd.poll(100 + k as u32, now, true, true, false),
                "advancing low_water is progress, not a stall"
            );
        }
    }

    #[test]
    fn a_confirm_in_flight_suppresses_the_reclaim() {
        let t0 = Instant::now();
        let mut wd = StallWatchdog::new(100, t0, TO);
        // Frozen far past the timeout, but a confirm is legitimately in flight → never reclaim (a
        // rebase would drop the live window the consumer is validating).
        assert!(!wd.poll(100, t0 + TO * 10, true, true, true));
    }

    #[test]
    fn caught_up_or_no_peers_never_fires() {
        let t0 = Instant::now();
        let mut wd = StallWatchdog::new(100, t0, TO);
        // No work (caught up): a still frontier is correct, not a stall.
        assert!(!wd.poll(100, t0 + TO * 5, false, true, false));
        // No live peers: nothing to reclaim toward; don't burn a rebase.
        assert!(!wd.poll(100, t0 + TO * 5, true, false, false));
    }

    #[test]
    fn a_persistent_wedge_is_reclaimed_repeatedly_not_once() {
        let t0 = Instant::now();
        let mut wd = StallWatchdog::new(100, t0, TO);
        assert!(wd.poll(100, t0 + TO, true, true, false), "first reclaim");
        // The clock reset on the fire; still wedged → it must fire AGAIN after another full timeout,
        // not give up after one shot.
        assert!(!wd.poll(100, t0 + TO + TO - Duration::from_millis(1), true, true, false));
        assert!(wd.poll(100, t0 + TO + TO, true, true, false), "second reclaim after another timeout");
    }

    // The live action against a real BlockQueue: a wedged frontier (frozen low_water, work far ahead)
    // is force-rebased — the generation bumps (the fetch_scheduler's abort-and-replan signal) and
    // reservations_reclaimed increments. (That the rebase also WAKES a producer parked on wait_space is
    // proven end-to-end in node/tests/block_queue.rs, where the mainnet-body fixture can fill the queue
    // to its budget so a genuine park occurs.)
    #[test]
    fn tick_rebases_a_wedged_queue_and_increments_reclaimed() {
        let metrics = Arc::new(SyncMetrics::default());
        let q = BlockQueue::new(100, 1 << 20, metrics.clone());
        let gen0 = q.current_gen();

        let mut wd = StallWatchdog::new(q.low_water(), Instant::now(), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(3));
        // Frontier frozen at 100, claimed target 10_000 >> 100, peers live, no confirm in flight → FIRE.
        let fired = wd.tick(&q, &metrics, Instant::now(), 10_000, true, false);

        assert!(fired, "a bounded stall with work + peers must reclaim");
        assert_eq!(metrics.reclaimed.load(Ordering::Relaxed), 1, "reservations_reclaimed increments");
        assert_ne!(q.current_gen(), gen0, "rebase bumped the generation (producer replan signal)");
        assert_eq!(q.low_water(), 100, "rebase held the frontier at the confirmed peak");

        // Caught up (target == low_water) must NOT keep reclaiming: no work → no fire.
        let mut wd2 = StallWatchdog::new(q.low_water(), Instant::now(), Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(3));
        assert!(!wd2.tick(&q, &metrics, Instant::now(), q.low_water(), true, false));
        assert_eq!(metrics.reclaimed.load(Ordering::Relaxed), 1, "no extra reclaim when caught up");
    }
}
