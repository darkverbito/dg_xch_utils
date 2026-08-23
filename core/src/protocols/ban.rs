//! Timed, bounded, thread-safe peer ban list — chia's `ChiaServer.banned_peers`.
//!
//! chia (`chia/server/server.py`) keys bans on the **remote peer host (IP)**, not on the peer's
//! certificate identity: `banned_peers: dict[str, float]` maps a host to the wall-clock instant its
//! ban lifts. A misbehaving peer is entered with [`ban_peer`](https://github.com) semantics
//! (`server.py::ban_peer`) — `ban_until = time.time() + ban_time`, and a fresh ban NEVER shortens an
//! existing one (`max(ban_until, existing)`). Both the inbound accept path
//! (`incoming_connection`, server.py:307) and the outbound dial path (`start_client`, server.py:455)
//! refuse a host that is in the map and not yet expired. Expired entries are garbage-collected
//! (server.py:271-277).
//!
//! Durations come from `chia/protocols/protocol_timing.py` (verified against chia 2.7.1):
//! `RATE_LIMITER_BAN_SECONDS = 300`, `CONSENSUS_ERROR_BAN_SECONDS = 600`,
//! `INVALID_PROTOCOL_BAN_SECONDS = API_EXCEPTION_BAN_SECONDS = INTERNAL_PROTOCOL_ERROR_BAN_SECONDS = 10`.
//!
//! Divergence from chia (deliberate, documented): chia's `banned_peers` grows unbounded between GC
//! sweeps. Ours enforces a hard [`DEFAULT_MAX_BANNED_HOSTS`] cap and prunes expired entries on every
//! mutation, so a peer cycling source IPs cannot grow the map without bound and the cap + expiry
//! together guarantee a ban can never wedge the node permanently. The remote-host keying means our
//! per-connection cert-hash identity (used elsewhere for the peer id) is intentionally NOT the ban
//! key — a peer that presents a fresh cert but the same IP stays banned.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// chia `protocol_timing.py`: a rate-limit violation bans the peer for 300s.
pub const RATE_LIMITER_BAN_SECONDS: u64 = 300;
/// chia `protocol_timing.py`: a consensus error (bad tx cost/fee, bad block body) bans for 600s.
pub const CONSENSUS_ERROR_BAN_SECONDS: u64 = 600;
/// chia `protocol_timing.py`: a protocol/internal/api violation bans for 10s ("don't flap if our
/// client is at fault").
pub const PROTOCOL_ERROR_BAN_SECONDS: u64 = 10;

/// Upper bound on the number of distinct banned hosts held at once. chia keeps no cap (expiry-only
/// GC); we add one so a churn of source IPs cannot grow the map without bound. When the cap is hit,
/// the soonest-to-expire ban is evicted to make room for the new one.
pub const DEFAULT_MAX_BANNED_HOSTS: usize = 10_000;

/// Why a peer is being banned. Maps 1:1 to chia's `protocol_timing.py` durations, so the wire-close
/// site names the cause and the duration follows — the mapping is what the parity test pins.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum BanCause {
    /// Inbound rate-limit violation (chia `close(RATE_LIMITER_BAN_SECONDS)`), and unsolicited/late
    /// block replies (`full_node_api.py` respond_block/respond_blocks/reject_block/reject_blocks).
    RateLimit,
    /// Consensus error: a `NewTransaction` announcing a zero-cost or already-seen-but-mismatched tx
    /// (chia `full_node_api.py::new_transaction` → `close(CONSENSUS_ERROR_BAN_SECONDS)`), and the
    /// `RespondUnfinishedBlock` generator/body failures.
    ConsensusError,
    /// Malformed protocol frame / unknown message type (chia `INVALID_PROTOCOL_BAN_SECONDS`).
    InvalidProtocol,
    /// Our own API raised while handling the peer's message (chia `API_EXCEPTION_BAN_SECONDS`).
    ApiException,
    /// Internal framing/decode fault attributed to the peer (chia `INTERNAL_PROTOCOL_ERROR_BAN_SECONDS`).
    InternalProtocolError,
}

impl BanCause {
    /// The chia ban duration for this cause, in seconds.
    #[must_use]
    pub const fn ban_seconds(self) -> u64 {
        match self {
            BanCause::RateLimit => RATE_LIMITER_BAN_SECONDS,
            BanCause::ConsensusError => CONSENSUS_ERROR_BAN_SECONDS,
            BanCause::InvalidProtocol
            | BanCause::ApiException
            | BanCause::InternalProtocolError => PROTOCOL_ERROR_BAN_SECONDS,
        }
    }
}

/// Timed, bounded, thread-safe host ban list (chia `ChiaServer.banned_peers`). Keyed on the REMOTE
/// peer IP. Cheap to clone via `Arc`; every method takes `&self` and holds the internal lock only
/// for the duration of a synchronous map operation (never across an `.await`), so it is safe to call
/// from the accept path and the close path concurrently.
pub struct BanRegistry {
    inner: Mutex<HashMap<IpAddr, Instant>>,
    cap: usize,
}

impl Default for BanRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_BANNED_HOSTS)
    }
}

impl BanRegistry {
    /// A registry holding at most `cap` distinct bans (min 1).
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cap: cap.max(1),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Instant>> {
        // A ban registry must never poison-panic the accept/close paths; recover the inner map.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ban `host` until `now + dur`. chia `ban_peer`: a fresh ban never SHORTENS an existing one — the
    /// later expiry wins. Prunes expired entries and enforces the cap on every call.
    pub fn ban_for(&self, host: IpAddr, dur: Duration) {
        let until = Instant::now() + dur;
        let mut map = self.guard();
        prune_expired(&mut map);
        match map.get(&host).copied() {
            // Keep the longer-standing ban (chia `max(ban_until, banned_peers[host])`).
            Some(existing) if existing >= until => {}
            _ => {
                map.insert(host, until);
            }
        }
        enforce_cap(&mut map, self.cap);
    }

    /// Ban `host` for the duration chia assigns to `cause`.
    pub fn ban(&self, host: IpAddr, cause: BanCause) {
        self.ban_for(host, Duration::from_secs(cause.ban_seconds()));
    }

    /// True iff `host` is currently banned (present and not yet expired). An expired entry is pruned
    /// as a side effect, so a probe both answers and cleans up — chia's `time.time() < ban_until`
    /// check plus the GC sweep, collapsed to the moment of the check.
    #[must_use]
    pub fn is_banned(&self, host: &IpAddr) -> bool {
        let mut map = self.guard();
        match map.get(host).copied() {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                map.remove(host);
                false
            }
            None => false,
        }
    }

    /// Remove any ban on `host` (operator/testing reset). Returns whether an entry was removed.
    pub fn unban(&self, host: &IpAddr) -> bool {
        self.guard().remove(host).is_some()
    }

    /// Drop every ban (operator/testing reset).
    pub fn clear(&self) {
        self.guard().clear();
    }

    /// The number of currently-live (non-expired) bans. Prunes expired entries first, so the count
    /// reflects only enforceable bans.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut map = self.guard();
        prune_expired(&mut map);
        map.len()
    }

    /// Whether there are no live bans.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drop every entry whose ban has already lifted.
fn prune_expired(map: &mut HashMap<IpAddr, Instant>) {
    let now = Instant::now();
    map.retain(|_, until| *until > now);
}

/// Evict soonest-to-expire bans until the map is within `cap`. Called after an insert, so it trims
/// at most one entry per ban in steady state — the bounded-growth guarantee chia lacks.
fn enforce_cap(map: &mut HashMap<IpAddr, Instant>, cap: usize) {
    while map.len() > cap {
        if let Some((victim, _)) = map.iter().min_by_key(|(_, until)| **until) {
            let victim = *victim;
            map.remove(&victim);
        } else {
            break;
        }
    }
}

// Unit coverage for this registry lives in `core/tests/ban_registry.rs` (an integration test against
// this fully-public API), so it runs without compiling the crate's in-tree `#[cfg(test)]` code.
