// Per-connection OUTBOUND self-throttle — the send-side companion to the inbound `RateLimiter`
// (`rate_limits.rs`).
//
// Before writing a message, the SAME rate-limit table is run through an outgoing limiter (the
// peer's negotiated capabilities select v1/v2). If sending now would exceed the PEER's budget
// the message is NOT dropped outright — this send is skipped and the message is retried ~1s
// later. The ONE exception is `respond_peers`: when it is over budget it is dropped WITHOUT
// re-queue, because its own cap is so low that re-queuing would spin. `Unlimited` response
// types (RespondBlocks, RespondBlock, RejectBlocks, …) carry no frequency budget, so our own
// solicited fetch and serve traffic is NEVER deferred — only the frequency-capped gossip types
// (compact-VDF re-gossip, transaction announces, NewPeak, …) can be paced. That is the
// self-safety property that keeps a strict peer from banning US for a legitimate re-gossip
// burst while never throttling our sync into failure.
//
// There is no single per-connection send loop draining an outgoing queue; sends are driven
// directly by the caller task under the connection write lock. So the throttle runs in the
// CALLER's task BEFORE acquiring the write lock — never holding the write lock across a wait,
// or a deferred gossip type would stall an `Unlimited` RespondBlocks queued behind it on the
// same lock and break the self-safety guarantee. `admit` sleeps `retry_delay` between
// re-checks. It is BOUNDED: after `max_attempts` deferrals the message is shed with a
// `Drop(BackpressureCap)`. Because the limiter window is 60s, `max_attempts * retry_delay`
// spans a full window, so a message that is merely ahead of budget is always admitted once
// the window rolls; only sustained over-budget flooding is shed.

use crate::protocols::ProtocolMessageTypes;
use crate::protocols::rate_limits::RateLimiter;
use crate::protocols::shared::Capabilities;
use std::time::Duration;

/// Re-queue cadence: 1s between attempts.
pub const RETRY_DELAY: Duration = Duration::from_secs(1);

/// Bounded backpressure: the max number of `retry_delay` deferrals before a message is shed. Chosen so
/// `MAX_ATTEMPTS * RETRY_DELAY` (65s) exceeds one 60-second limiter window — a message merely ahead of
/// budget always drains once the window rolls; only sustained flooding hits the cap and is dropped.
pub const MAX_ATTEMPTS: u32 = 65;

/// True for message types dropped WITHOUT re-queue when they are over budget on the send path
/// (`respond_peers`). Every other frequency-capped type is instead deferred and retried.
#[must_use]
pub fn is_requeue_exempt(msg_type: ProtocolMessageTypes) -> bool {
    matches!(msg_type, ProtocolMessageTypes::RespondPeers)
}

/// The classification of one outbound message against the peer's budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboundDecision {
    /// Within budget (or an `Unlimited` type within its per-message size cap): send now. The budget
    /// slot has been committed by this call (the outgoing counter is committed only when allowed).
    Send,
    /// Over budget and NOT re-queue-exempt: wait `retry_delay` and re-check.
    Defer,
    /// Over budget and re-queue-exempt (`respond_peers`): drop now, no retry.
    DropExempt,
}

/// Why an outbound message was not sent (the non-`Admit` outcomes of [`OutboundLimiter::admit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// A re-queue-exempt type (`respond_peers`) over budget — dropped without retry.
    Exempt,
    /// `max_attempts` deferrals were exhausted without the budget opening — bounded backpressure shed.
    BackpressureCap,
}

/// The result of running one message through the throttle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThrottleOutcome {
    /// Cleared to send now — the limiter has already committed this message's budget slot.
    Admit,
    /// The message must not be sent.
    Drop(DropReason),
}

/// A per-connection outbound self-throttle. Wraps an `incoming = false` [`RateLimiter`] (the SAME
/// composed v1/v2 table the inbound limiter uses — a peer bans us by THEIR limits, which equal ours
/// under v2-compose since both advertise it) plus the re-queue policy.
pub struct OutboundLimiter {
    limiter: RateLimiter,
    max_attempts: u32,
    retry_delay: Duration,
}

impl OutboundLimiter {
    /// Defaults: a 60-second outbound window at 100% of the published numbers, 1s re-queue
    /// cadence, bounded at [`MAX_ATTEMPTS`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            limiter: RateLimiter::new(false),
            max_attempts: MAX_ATTEMPTS,
            retry_delay: RETRY_DELAY,
        }
    }

    /// Test/tuning constructor: override the window, the percentage, the attempt cap, and the cadence.
    #[must_use]
    pub fn with_params(
        reset_seconds: u64,
        percentage_of_limit: u32,
        max_attempts: u32,
        retry_delay: Duration,
    ) -> Self {
        Self {
            limiter: RateLimiter::with_params(false, reset_seconds, percentage_of_limit),
            max_attempts,
            retry_delay,
        }
    }

    /// Classify one message against the peer's current budget. On [`OutboundDecision::Send`] the
    /// budget slot is committed (the outgoing counter advances only when the message is allowed);
    /// a `Defer`/`DropExempt` does not advance the counter, so re-checking is free of double-counting.
    #[must_use]
    pub fn decide(
        &self,
        msg_type: ProtocolMessageTypes,
        size: usize,
        peer_caps: &Capabilities,
    ) -> OutboundDecision {
        match self.limiter.process_and_check(msg_type, size, peer_caps) {
            None => OutboundDecision::Send,
            Some(_) if is_requeue_exempt(msg_type) => OutboundDecision::DropExempt,
            Some(_) => OutboundDecision::Defer,
        }
    }

    /// Block the CALLER (holding no connection lock) until the message fits the peer's budget, then
    /// return [`ThrottleOutcome::Admit`] — or shed it ([`ThrottleOutcome::Drop`]) when it is exempt or
    /// the attempt cap is reached. It defers (does not drop) a
    /// would-be-oversending message, retrying every `retry_delay`, bounded by `max_attempts`.
    pub async fn admit(
        &self,
        msg_type: ProtocolMessageTypes,
        size: usize,
        peer_caps: &Capabilities,
    ) -> ThrottleOutcome {
        let mut attempts: u32 = 0;
        loop {
            match self.decide(msg_type, size, peer_caps) {
                OutboundDecision::Send => return ThrottleOutcome::Admit,
                OutboundDecision::DropExempt => return ThrottleOutcome::Drop(DropReason::Exempt),
                OutboundDecision::Defer => {
                    if attempts >= self.max_attempts {
                        return ThrottleOutcome::Drop(DropReason::BackpressureCap);
                    }
                    attempts += 1;
                    tokio::time::sleep(self.retry_delay).await;
                }
            }
        }
    }
}

impl Default for OutboundLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::shared::{CAPABILITIES, Capability};
    use std::time::Instant;

    const MIB: usize = 1024 * 1024;

    fn v2_caps() -> Capabilities {
        CAPABILITIES
            .iter()
            .map(|(v, s)| (*v, (*s).to_string()))
            .collect()
    }

    // TEST (1): a burst of a frequency-capped gossip type over the peer's per-minute budget is PACED,
    // not sent immediately. NewCompactVdf is 100/min (no v2 override). With
    // a window that never rolls in-test, the first 100 are admitted (this is what a peer allows) and
    // every further one is Defer — delayed rather than send-and-get-banned.
    #[test]
    fn frequency_capped_burst_is_paced_not_sent_immediately() {
        let ol = OutboundLimiter::with_params(3600, 100, 65, Duration::from_millis(1));
        let caps = v2_caps();
        for _ in 0..100 {
            assert_eq!(
                ol.decide(ProtocolMessageTypes::NewCompactVdf, 64, &caps),
                OutboundDecision::Send,
                "the first 100 NewCompactVdf are within the peer's 100/min budget"
            );
        }
        for _ in 0..10 {
            assert_eq!(
                ol.decide(ProtocolMessageTypes::NewCompactVdf, 64, &caps),
                OutboundDecision::Defer,
                "each over-budget gossip message is DEFERRED (paced), never sent immediately"
            );
        }
    }

    // TEST (2) — self-safety: our own serve/sync traffic is `Unlimited` (RespondBlocks, size-only) and
    // must NEVER be deferred, no matter how fast we serve. Mirrors the inbound self-safety test
    // (t045 `solicited_respond_blocks_burst_...`). 2000 back-to-back RespondBlocks all admit.
    #[test]
    fn unlimited_serve_type_is_never_deferred() {
        let ol = OutboundLimiter::with_params(3600, 100, 65, Duration::from_millis(1));
        let caps = v2_caps();
        for _ in 0..2000 {
            assert_eq!(
                ol.decide(ProtocolMessageTypes::RespondBlocks, 10 * MIB, &caps),
                OutboundDecision::Send,
                "an Unlimited serve type is never throttled — our sync/serve is unaffected"
            );
        }
    }

    // TEST (3) — the re-queue exemption: respond_peers over budget is DROPPED, not deferred.
    // respond_peers is 10/min. The first 10 admit;
    // the 11th is DropExempt (not Defer).
    #[test]
    fn exempt_type_is_dropped_not_deferred_when_over_budget() {
        let ol = OutboundLimiter::with_params(3600, 100, 65, Duration::from_millis(1));
        let caps = v2_caps();
        for _ in 0..10 {
            assert_eq!(
                ol.decide(ProtocolMessageTypes::RespondPeers, 64, &caps),
                OutboundDecision::Send,
            );
        }
        assert_eq!(
            ol.decide(ProtocolMessageTypes::RespondPeers, 64, &caps),
            OutboundDecision::DropExempt,
            "respond_peers is exempt from re-queue — dropped when over budget, never deferred"
        );
    }

    // TEST (4) — bounded queue: when the budget never opens (window pinned) an over-budget non-exempt
    // message is shed after exactly `max_attempts` deferrals with Drop(BackpressureCap). Proves the
    // throttle can never grow an unbounded outbound backlog.
    #[tokio::test]
    async fn admit_sheds_after_max_attempts_when_budget_never_opens() {
        let ol = OutboundLimiter::with_params(3600, 100, 3, Duration::from_millis(1));
        let caps = v2_caps();
        for _ in 0..100 {
            let _ = ol.decide(ProtocolMessageTypes::NewCompactVdf, 64, &caps);
        }
        let outcome = ol
            .admit(ProtocolMessageTypes::NewCompactVdf, 64, &caps)
            .await;
        assert_eq!(
            outcome,
            ThrottleOutcome::Drop(DropReason::BackpressureCap),
            "a pinned-over-budget message is shed after max_attempts — bounded, no unbounded queue"
        );
    }

    // TEST (1) headline — deferred, NOT dropped, and EVENTUALLY sent once the window rolls. A 1s
    // window; fill NewCompactVdf to its 100/min cap, then admit one more: it defers across the window
    // boundary and is admitted (not dropped). Real-time ~1s (one slow-ish test).
    #[tokio::test]
    async fn admit_eventually_sends_over_budget_message_after_window_rolls() {
        let ol = OutboundLimiter::with_params(1, 100, 65, Duration::from_millis(50));
        let caps = v2_caps();
        for _ in 0..100 {
            assert_eq!(
                ol.decide(ProtocolMessageTypes::NewCompactVdf, 64, &caps),
                OutboundDecision::Send,
            );
        }
        let start = Instant::now();
        let outcome = ol
            .admit(ProtocolMessageTypes::NewCompactVdf, 64, &caps)
            .await;
        assert_eq!(
            outcome,
            ThrottleOutcome::Admit,
            "the over-budget message is DELAYED, not dropped, and sent once the window rolls"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "it drains within a window, not the full attempt cap"
        );
    }

    // A v1-only peer selects the v1 numbers for the outbound direction too: request_puzzle_solution is
    // 1000/min under v1 (5000 under v2), so against a v1 peer the 1001st is deferred. Confirms the
    // reused table is capability-driven on the send side exactly as on the receive side.
    #[test]
    fn v1_only_peer_selects_v1_numbers_on_the_send_side() {
        let ol = OutboundLimiter::with_params(3600, 100, 65, Duration::from_millis(1));
        let v1_caps: Capabilities = vec![
            (Capability::Base as u16, "1".to_string()),
            (Capability::BlockHeaders as u16, "1".to_string()),
        ];
        for _ in 0..1000 {
            assert_eq!(
                ol.decide(ProtocolMessageTypes::RequestPuzzleSolution, 10, &v1_caps),
                OutboundDecision::Send,
            );
        }
        assert_eq!(
            ol.decide(ProtocolMessageTypes::RequestPuzzleSolution, 10, &v1_caps),
            OutboundDecision::Defer,
            "a v1 peer caps request_puzzle_solution at 1000/min on the send side"
        );
    }
}
