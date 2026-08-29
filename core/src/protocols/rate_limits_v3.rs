// RATE_LIMITS_V3: window-based (in-flight) rate limits.
//
// When BOTH peers advertise `Capability::RateLimitsV3`, a `ConfigureWindowSizes` message is
// exchanged immediately after the handshake (each side sends its settings and validates the
// peer's), and for the message types in [`v3_setting`]'s table the time-based v1/v2 limiter is
// REPLACED by an in-flight window: a request type with `window_size = Some(n)` may have at most
// `n` requests being processed (receiver side) / in flight (sender side) at once; response and
// reject types are `None` (unlimited — implicitly bounded by their soliciting requests).
//
// The table, the 256-entry configure cap, and the validation rules are protocol constants, never
// defaulted; `core/tests/rate_limits_v3_table.rs` pins every entry.
//
// Capability posture: v3 is NOT in the default outgoing capability set (BASE / BLOCK_HEADERS /
// RATE_LIMITS_V2 only), so an INITIATOR advertises it only when explicitly configured. A RESPONDER
// auto-mirrors: when the inbound handshake advertises v3, it is appended to the reply capabilities
// and the configure exchange runs. Outbound dials keep the default set.

use crate::protocols::ProtocolMessageTypes;
use crate::protocols::shared::{Capabilities, Capability, ConfigureWindowSizes};
use std::collections::HashMap;
use std::io::{Error, ErrorKind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// One message type's v3 setting: the maximum number of in-flight messages of this type,
/// `None` = unlimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RlSettingsV3 {
    pub window_size: Option<u16>,
}

const fn window(n: u16) -> Option<RlSettingsV3> {
    Some(RlSettingsV3 {
        window_size: Some(n),
    })
}
const UNLIMITED: Option<RlSettingsV3> = Some(RlSettingsV3 { window_size: None });

/// The v3 table, all 36 entries: the 13 request types at `window_size = 2`, their
/// responses/rejects unlimited. A type not in this
/// table stays under the time-based v1/v2 limiter even when v3 is active.
#[must_use]
pub fn v3_setting(t: ProtocolMessageTypes) -> Option<RlSettingsV3> {
    use ProtocolMessageTypes as P;
    match t {
        P::RequestBlocks
        | P::RequestBlock
        | P::RequestBlockHeader
        | P::RequestBlockHeaders
        | P::RequestHeaderBlocks
        | P::RegisterInterestInPuzzleHash
        | P::RegisterInterestInCoin
        | P::RequestPuzzleState
        | P::RequestCoinState
        | P::RequestAdditions
        | P::RequestRemovals
        | P::RequestProofOfWeight
        | P::RequestPuzzleSolution => window(2),
        P::RespondBlocks
        | P::RejectBlocks
        | P::RespondBlock
        | P::RejectBlock
        | P::RespondBlockHeader
        | P::RejectHeaderRequest
        | P::RespondBlockHeaders
        | P::RejectBlockHeaders
        | P::RespondHeaderBlocks
        | P::RejectHeaderBlocks
        | P::RespondToPhUpdate
        | P::RespondToCoinUpdate
        | P::RespondPuzzleState
        | P::RejectPuzzleState
        | P::RespondCoinState
        | P::RejectCoinState
        | P::RespondAdditions
        | P::RejectAdditionsRequest
        | P::RespondRemovals
        | P::RejectRemovalsRequest
        | P::RespondProofOfWeight
        | P::RespondPuzzleSolution
        | P::RejectPuzzleSolution => UNLIMITED,
        _ => None,
    }
}

/// Maximum number of entries accepted in a `ConfigureWindowSizes` message — chia
/// `MAX_CONFIGURE_RATE_LIMITS_ENTRIES`.
pub const MAX_CONFIGURE_RATE_LIMITS_ENTRIES: usize = 256;

/// True when `caps` advertises `RATE_LIMITS_V3` with state "1" (chia
/// `known_active_capabilities`).
#[must_use]
pub fn peer_supports_v3(caps: &Capabilities) -> bool {
    let v3 = Capability::RateLimitsV3 as u16;
    caps.iter().any(|(v, state)| *v == v3 && state == "1")
}

/// Encode OUR default v3 table into the `ConfigureWindowSizes` message we send after the
/// handshake — chia `rl_v3_to_configure_message` (window 0 encodes "unlimited").
#[must_use]
pub fn configure_message() -> ConfigureWindowSizes {
    let mut settings: Vec<(u8, u16)> = Vec::new();
    for code in 0..=u8::MAX {
        let t = ProtocolMessageTypes::from(code);
        if t == ProtocolMessageTypes::Unknown {
            continue;
        }
        if let Some(s) = v3_setting(t) {
            settings.push((code, s.window_size.unwrap_or(0)));
        }
    }
    debug_assert!(!settings.is_empty() && settings.len() <= MAX_CONFIGURE_RATE_LIMITS_ENTRIES);
    ConfigureWindowSizes { settings }
}

/// Parse + validate a peer's `ConfigureWindowSizes` — chia
/// `rl_settings_v3_from_configure_message` + the `perform_handshake` envelope checks:
/// - an EMPTY settings list is invalid (chia raises INVALID_HANDSHAKE),
/// - more than [`MAX_CONFIGURE_RATE_LIMITS_ENTRIES`] entries is invalid,
/// - an unknown message-type code is silently skipped (the peer may know newer types),
/// - a non-zero window for a type OUR table holds unlimited is invalid — a peer must not
///   throttle our response types ("Don't allow peers to alter our unlimited ... windows"),
/// - `window_size == 0` decodes as unlimited.
pub fn settings_from_configure(
    msg: &ConfigureWindowSizes,
) -> Result<HashMap<ProtocolMessageTypes, RlSettingsV3>, Error> {
    if msg.settings.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "ConfigureWindowSizes: empty settings (INVALID_HANDSHAKE)",
        ));
    }
    if msg.settings.len() > MAX_CONFIGURE_RATE_LIMITS_ENTRIES {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "ConfigureWindowSizes: {} entries exceeds the {MAX_CONFIGURE_RATE_LIMITS_ENTRIES} cap (INVALID_HANDSHAKE)",
                msg.settings.len()
            ),
        ));
    }
    let mut out = HashMap::new();
    for (code, window_size) in &msg.settings {
        let t = ProtocolMessageTypes::from(*code);
        if t == ProtocolMessageTypes::Unknown {
            continue; // chia: unknown entries are silently skipped
        }
        if let Some(ours) = v3_setting(t)
            && ours.window_size.is_none()
            && *window_size != 0
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "ConfigureWindowSizes: peer tried to bound our unlimited type {t:?} at {window_size} (INVALID_HANDSHAKE)"
                ),
            ));
        }
        out.insert(
            t,
            RlSettingsV3 {
                window_size: if *window_size == 0 {
                    None
                } else {
                    Some(*window_size)
                },
            },
        );
    }
    Ok(out)
}

/// Per-connection v3 state, shared between the read loop (receive windows + reply routing), the
/// send paths (outbound in-flight windows), and the handshake arm (negotiation). One instance
/// per link, minted in `WebsocketConnection::new`.
#[derive(Default)]
pub struct V3Link {
    /// We offered/mirrored v3 on this link (responder appended the capability + sent our
    /// ConfigureWindowSizes); the peer's configure message is expected.
    offered: AtomicBool,
    /// Both sides advertised v3 AND the configure exchange completed — v3 semantics live.
    active: AtomicBool,
    inner: Mutex<V3Inner>,
}

#[derive(Default)]
struct V3Inner {
    /// The PEER's advertised settings — the windows bounding OUR outbound requests.
    peer_settings: HashMap<u8, RlSettingsV3>,
    /// In-flight inbound requests being processed, per type (chia `receive_window`).
    recv_in_flight: HashMap<u8, u16>,
    /// Our in-flight outbound requests, per type (chia `in_flight`).
    out_in_flight: HashMap<u8, u16>,
    /// Correlation ids of our outbound requests currently occupying a window → type
    /// (chia `v3_sent_request_ids`).
    out_ids: HashMap<u16, u8>,
}

impl V3Link {
    /// Mark that we mirrored v3 on this link (responder side) — the peer's configure is now
    /// legitimate.
    pub fn offer(&self) {
        self.offered.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_offered(&self) -> bool {
        self.offered.load(Ordering::Acquire)
    }
    /// Complete activation with the peer's validated settings.
    pub fn activate(&self, peer_settings: HashMap<ProtocolMessageTypes, RlSettingsV3>) {
        {
            let mut inner = self.guard();
            inner.peer_settings = peer_settings
                .into_iter()
                .map(|(t, s)| (t as u8, s))
                .collect();
        }
        self.active.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, V3Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Receiver side: admit one more in-flight request of `t` under OUR table's window. Returns
    /// `Ok(true)` (admitted, caller must [`V3Link::recv_release`] when processing completes),
    /// `Ok(false)` (type has no bounded window — nothing tracked), or `Err(())` when the window
    /// is full — chia closes with `RATE_LIMITER_BAN_SECONDS`.
    #[allow(clippy::result_unit_err)]
    pub fn recv_acquire(&self, t: ProtocolMessageTypes) -> Result<bool, ()> {
        let Some(RlSettingsV3 {
            window_size: Some(w),
        }) = v3_setting(t)
        else {
            return Ok(false);
        };
        let mut inner = self.guard();
        let count = inner.recv_in_flight.entry(t as u8).or_insert(0);
        if *count >= w {
            return Err(());
        }
        *count += 1;
        Ok(true)
    }
    /// Receiver side: one in-flight request of `t` finished processing.
    pub fn recv_release(&self, t: ProtocolMessageTypes) {
        let mut inner = self.guard();
        if let Some(count) = inner.recv_in_flight.get_mut(&(t as u8)) {
            *count = count.saturating_sub(1);
        }
    }

    /// Sender side: try to occupy one slot of the PEER's window for a request of type `t` with
    /// correlation id `id`. `Ok(true)`: slot taken (released when the reply/timeout releases
    /// `id`); `Ok(false)`: the type is unbounded/untracked by the peer — send freely;
    /// `Err(())`: the peer's window is full — the caller must defer the send (chia
    /// `_wait_and_retry`).
    #[allow(clippy::result_unit_err)]
    pub fn out_acquire(&self, t: ProtocolMessageTypes, id: u16) -> Result<bool, ()> {
        let mut inner = self.guard();
        let Some(RlSettingsV3 {
            window_size: Some(w),
        }) = inner.peer_settings.get(&(t as u8)).copied()
        else {
            return Ok(false);
        };
        let count = inner.out_in_flight.entry(t as u8).or_insert(0);
        if *count >= w {
            return Err(());
        }
        *count += 1;
        inner.out_ids.insert(id, t as u8);
        Ok(true)
    }
    /// Sender side: the request with correlation `id` completed (reply arrived, timed out, or
    /// was cancelled) — free its window slot. Unknown ids are a no-op (not every request is
    /// window-tracked).
    pub fn out_release(&self, id: u16) {
        let mut inner = self.guard();
        if let Some(code) = inner.out_ids.remove(&id)
            && let Some(count) = inner.out_in_flight.get_mut(&code)
        {
            *count = count.saturating_sub(1);
        }
    }
}

/// RAII release of one inbound receive-window slot: the read loop acquires before spawning the
/// handler task(s) and threads an `Arc<RecvGuard>` into them — the slot frees when the last
/// clone drops (processing finished).
pub struct RecvGuard {
    link: std::sync::Arc<V3Link>,
    t: ProtocolMessageTypes,
}

impl RecvGuard {
    #[must_use]
    pub fn new(link: std::sync::Arc<V3Link>, t: ProtocolMessageTypes) -> Self {
        Self { link, t }
    }
}

impl Drop for RecvGuard {
    fn drop(&mut self) {
        self.link.recv_release(self.t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The V3Link window accounting: acquire to the bound, refuse past it, release frees.
    #[test]
    fn recv_window_admits_to_the_bound_and_refuses_past_it() {
        let link = V3Link::default();
        let t = ProtocolMessageTypes::RequestBlocks; // window 2
        assert_eq!(link.recv_acquire(t), Ok(true));
        assert_eq!(link.recv_acquire(t), Ok(true));
        assert_eq!(link.recv_acquire(t), Err(()), "third in-flight exceeds w=2");
        link.recv_release(t);
        assert_eq!(link.recv_acquire(t), Ok(true), "released slot re-admits");
        // A type outside the v3 table is untracked.
        assert_eq!(link.recv_acquire(ProtocolMessageTypes::NewPeak), Ok(false));
    }

    #[test]
    fn out_window_tracks_ids_against_peer_settings() {
        let link = V3Link::default();
        let peer = settings_from_configure(&configure_message()).expect("our own table is valid");
        link.activate(peer);
        let t = ProtocolMessageTypes::RequestBlocks;
        assert_eq!(link.out_acquire(t, 1), Ok(true));
        assert_eq!(link.out_acquire(t, 2), Ok(true));
        assert_eq!(link.out_acquire(t, 3), Err(()), "peer window 2 is full");
        link.out_release(1);
        assert_eq!(link.out_acquire(t, 3), Ok(true));
        // Responses are unlimited in the peer's table — untracked.
        assert_eq!(
            link.out_acquire(ProtocolMessageTypes::RespondBlocks, 9),
            Ok(false)
        );
        // Unknown id release is a no-op.
        link.out_release(4242);
    }
}
