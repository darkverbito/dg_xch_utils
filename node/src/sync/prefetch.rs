//! Fetch-pipeline readahead: keep K follow windows of block bodies fetched-or-in-flight
//! AHEAD of the validator, striped across DISTINCT peers, so the follow loop never idles on the
//! network between windows. The measured problem this closes: every leg's sustained rate sat at
//! 40-60% of its phase-sum ceiling because the fetch of window N+1 effectively started only as
//! the validation of window N finished — one window deep, one peer at a time (worst case observed:
//! 18s wall per window, ~14s of it fetch wait for tx-dense bodies from public peers).
//!
//! The reference shape is libbitcoin-node's check chaser (`src/chasers/chaser_check.cpp`):
//! `set_unassociated()` keeps a download window of `position() + maximum_concurrency_` block
//! hashes populated ahead of the validate chaser and portions it across the live channels
//! (`ceilinged_divide(inventory, connections_)`); `get_hashes`/`put_hashes` hand batch maps to
//! peer channels concurrently instead of fetching on demand. Here the work unit is the follow
//! window (`FOLLOW_BATCH` heights) and the channel is a [`BlockRangeSource`]: up to `depth`
//! adjacent windows are dispatched to DIFFERENT peers, the validator [`WindowReadahead::take`]s
//! them back in height order, and the depth ADAPTS — grow while the validator measurably waits
//! on the network, shrink when the pipe runs ahead — capped by a resident-bytes budget and by
//! one in-flight window per peer slot (the W == P contract).
//!
//! The readahead holds fetched bodies in RAM ONLY — it never touches the store, and confirm
//! order is untouched (the caller stages and confirms exactly the window it asked for, in
//! height order). Crash/resume semantics are therefore byte-identical to the direct-fetch path:
//! a kill mid-readahead loses nothing but not-yet-validated downloads, and the restart
//! re-fetches from the confirmed peak. Every dispatch is bounded: at most
//! [`READAHEAD_MAX_DEPTH`] spawned tasks, each wrapped in the request timeout, all aborted on
//! drop.

use crate::sync::{BlockRangeSource, SyncError, SyncMetrics, TARGET_OUTBOUND};
use dg_xch_core::blockchain::full_block::FullBlock;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tracing::Instrument;

/// Initial readahead depth K: windows fetched-or-in-flight ahead of the validator.
pub const READAHEAD_START_DEPTH: usize = 4;

/// DEFAULT depth ceiling (the shipped default behavior) — one in-flight window per outbound peer slot
/// (`W == P`): at the default one-window-per-peer fan-out, striping past the peer count
/// would put two concurrent ranges on one connection, where two `RequestBlocks` from distinct source
/// instances could collide on a message id. The aggressive `--prefetch-memory-mb` /
/// `--prefetch-max-inflight` knobs ([`PrefetchConfig::aggressive`]) raise this ceiling — up to
/// [`READAHEAD_ABS_MAX_DEPTH`] — and lift the per-peer fan-out to [`READAHEAD_MAX_PER_PEER`], which
/// stays id-safe because a peer's extra windows share ONE source instance's `next_msg_id`.
pub const READAHEAD_MAX_DEPTH: usize = TARGET_OUTBOUND;

/// Adaptive floor: never below the classic one-window overlap the follow driver always had.
pub const READAHEAD_MIN_DEPTH: usize = 1;

/// Resident-bytes budget for fetched-but-not-yet-taken bodies (wire-size approximation, EWMA of
/// resolved windows). 256 MiB admits the full 8-window depth even at ~1 MiB/block dust-era wire
/// sizes (8 × 32 × 1 MiB = 256 MiB) while staying far inside a 4 GiB node envelope — the
/// validation arena (37-57 MiB/block transient), not the wire bytes, remains the memory ceiling.
pub const READAHEAD_BYTE_BUDGET: u64 = 256 * 1024 * 1024;

/// Absolute hard ceiling on readahead depth K — the most windows fetched-or-in-flight, hence the
/// most spawned fetch tasks, REGARDLESS of the operator's RAM budget. The byte budget does the real,
/// measured-size-adaptive clamp ([`depth_within_budget`]); this is the "bound everything" backstop so
/// a huge `--prefetch-memory-mb` can never spawn an unbounded task fan-out. 256 windows × 32 heights
/// = 8192 heights in flight — a Threadripper-scale ceiling, still O(1) in chain height.
pub const READAHEAD_ABS_MAX_DEPTH: usize = 256;

/// Ceiling on concurrent in-flight windows dispatched onto a SINGLE peer connection. The shipped
/// default is 1 (the `W == P` one-window-per-peer contract); the aggressive knob
/// raises AGGREGATE concurrency by allowing a bounded fan-out per peer — extra requests spread
/// ACROSS peers, never flooding one. Distinct windows on one connection are message-id-safe because
/// they share ONE [`crate::sync::source::OutboundPeerSource`] instance, whose `next_msg_id` hands out
/// distinct ids per request; this cap bounds only how hard a single peer is leaned on.
pub const READAHEAD_MAX_PER_PEER: usize = 16;

/// The RAM budget and concurrency bounds a [`WindowReadahead`] runs under. [`PrefetchConfig::default`]
/// reproduces the shipped default behavior EXACTLY (256 MiB budget, depth/aggregate-in-flight ≤ 8, one
/// window per peer); [`PrefetchConfig::aggressive`] derives the operator's `--prefetch-memory-mb` /
/// `--prefetch-max-inflight` knobs.
///
/// Two distinct bounds, hence (optionally) two knobs — they clamp in DIFFERENT units and a large-RAM
/// fetch-starved node needs both:
/// * `byte_budget` (bytes, from `--prefetch-memory-mb`) is the HARD OOM ceiling on resident bodies —
///   it drives DEPTH via the measured-size clamp, collapsing to one window when blocks are huge.
/// * `max_inflight` / `per_peer` (a COUNT, from `--prefetch-max-inflight`) cap the outstanding fetch
///   fan-out — the concurrency that keeps the buffer refilling as fast as the validator drains it.
///   A byte budget alone can't express this: at small (dust-era ~1 MiB) blocks a big budget admits a
///   very high window COUNT, so the count cap is what bounds spawned tasks and per-peer load.
#[derive(Clone, Copy, Debug)]
pub struct PrefetchConfig {
    /// HARD ceiling on resident (fetched-but-not-yet-taken) window bytes — the OOM bound.
    pub byte_budget: u64,
    /// Ceiling the adaptive depth K may grow to (resident-window count; the byte budget clamps it
    /// further by measured size at dispatch).
    pub max_depth: usize,
    /// Ceiling on AGGREGATE windows in flight at once, summed across all peers.
    pub max_inflight: usize,
    /// Ceiling on windows in flight on any SINGLE peer connection (anti-flood fan-out cap).
    pub per_peer: usize,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            byte_budget: READAHEAD_BYTE_BUDGET,
            max_depth: READAHEAD_MAX_DEPTH,
            max_inflight: READAHEAD_MAX_DEPTH,
            per_peer: 1,
        }
    }
}

impl PrefetchConfig {
    /// Derive the aggressive config from the operator knobs. `memory_mb` sets the resident-bytes
    /// budget (the OOM ceiling that drives depth); `max_inflight` optionally caps the aggregate
    /// outstanding-fetch fan-out (defaulting to the anti-flood ceiling `peers × READAHEAD_MAX_PER_PEER`,
    /// itself bounded by [`READAHEAD_ABS_MAX_DEPTH`]); `peers` is the planning peer count (the
    /// `W == P` target) the aggregate is spread across as `per_peer = ceil(max_inflight / peers)`.
    ///
    /// The depth ceiling is raised to the aggregate so K may exceed the shipped 8; the actual K still
    /// ADAPTS up from [`READAHEAD_START_DEPTH`] under measured validator wait and is clamped every
    /// dispatch by the measured-size byte budget — so a huge `memory_mb` at dust-era block sizes still
    /// collapses depth and can never OOM (floored at [`READAHEAD_MIN_DEPTH`], the irreducible one
    /// window the direct-fetch path also holds).
    #[must_use]
    pub fn aggressive(memory_mb: u64, max_inflight: Option<usize>, peers: usize) -> Self {
        let byte_budget = memory_mb.saturating_mul(1024 * 1024);
        let peers = peers.max(1);
        let per_peer_ceiling = peers.saturating_mul(READAHEAD_MAX_PER_PEER);
        let max_inflight = max_inflight
            .unwrap_or(READAHEAD_ABS_MAX_DEPTH)
            .clamp(READAHEAD_MIN_DEPTH, READAHEAD_ABS_MAX_DEPTH.min(per_peer_ceiling));
        let per_peer = max_inflight.div_ceil(peers).clamp(1, READAHEAD_MAX_PER_PEER);
        // Resident-depth ceiling coincides with the aggregate in-flight cap (a resolved-but-not-taken
        // window still occupies a deque slot counted against `max_inflight`), and is never below the
        // shipped default so aggressive can only ever raise, never lower, the ceiling.
        let max_depth = max_inflight.max(READAHEAD_MAX_DEPTH);
        Self {
            byte_budget,
            max_depth,
            max_inflight,
            per_peer,
        }
    }
}

// A take() that had to wait longer than this counts as measurable validator idle → grow K.
const GROW_WAIT_THRESHOLD: Duration = Duration::from_millis(50);
// This many consecutive ~zero-wait takes shrink K by one — give unneeded budget back.
const SHRINK_STREAK: u32 = 32;
// Per-block wire-size overhead beyond the generator bytes (foliage, proofs, reward chain).
const BLOCK_OVERHEAD_BYTES: u64 = 4096;

// One dispatched window: `[from, to]` fetching on `peer_id`'s connection.
struct Inflight {
    from: u32,
    to: u32,
    peer_id: u64,
    handle: JoinHandle<Result<Vec<FullBlock>, SyncError>>,
}

/// The K-deep multi-peer window readahead the follow driver owns (see the module docs).
pub struct WindowReadahead {
    // Ascending, contiguous in-flight windows; head is the next window the validator will ask
    // for.
    inflight: VecDeque<Inflight>,
    depth: usize,
    zero_wait_streak: u32,
    // EWMA wire-size of a resolved window — the resident-budget denominator.
    window_bytes_ewma: u64,
    cfg: PrefetchConfig,
    request_timeout: Duration,
    // Round-robin cursor over the source list so the same peer is not always the head server.
    rotation: usize,
    metrics: Arc<SyncMetrics>,
}

/// The depth actually admissible under the resident-bytes budget: `depth`, reduced so that
/// `depth × window_bytes_ewma ≤ byte_budget`, never below [`READAHEAD_MIN_DEPTH`]. A zero EWMA
/// (nothing resolved yet) admits `depth` unchanged.
#[must_use]
pub fn depth_within_budget(depth: usize, window_bytes_ewma: u64, byte_budget: u64) -> usize {
    if window_bytes_ewma == 0 {
        return depth;
    }
    let by_budget = usize::try_from(byte_budget / window_bytes_ewma).unwrap_or(usize::MAX);
    depth.min(by_budget.max(READAHEAD_MIN_DEPTH))
}

// Wire-size approximation for the resident budget: the generator dominates a tx-dense block;
// the rest (foliage, proofs, reward chain) is a small near-constant. Shared with the byte-budgeted
// [`crate::sync::queue::BlockQueue`] so the readahead and the queue charge bytes identically.
pub(crate) fn approx_block_bytes(block: &FullBlock) -> u64 {
    BLOCK_OVERHEAD_BYTES
        + block
            .transactions_generator
            .as_ref()
            .map_or(0, |g| g.buffer().as_ref().len() as u64)
}

impl WindowReadahead {
    /// The shipped default readahead: 256 MiB budget, depth/aggregate-in-flight ≤ 8, one window per
    /// peer. Byte-identical to [`WindowReadahead::with_config`] under [`PrefetchConfig::default`].
    #[must_use]
    pub fn new(metrics: Arc<SyncMetrics>, request_timeout: Duration) -> Self {
        Self::with_config(metrics, request_timeout, PrefetchConfig::default())
    }

    /// The readahead under an explicit [`PrefetchConfig`] — the aggressive `--prefetch-memory-mb` /
    /// `--prefetch-max-inflight` path. The adaptive depth K still STARTS at [`READAHEAD_START_DEPTH`]
    /// and ramps under measured wait; the config only raises the ceilings (budget, depth, aggregate
    /// in-flight, per-peer fan-out) K may climb to.
    #[must_use]
    pub fn with_config(
        metrics: Arc<SyncMetrics>,
        request_timeout: Duration,
        cfg: PrefetchConfig,
    ) -> Self {
        Self {
            inflight: VecDeque::new(),
            depth: READAHEAD_START_DEPTH,
            zero_wait_streak: 0,
            window_bytes_ewma: 0,
            cfg,
            request_timeout,
            rotation: 0,
            metrics,
        }
    }

    /// The RAM/concurrency bounds this readahead runs under (the operator knobs, or the shipped
    /// default).
    #[must_use]
    pub fn config(&self) -> PrefetchConfig {
        self.cfg
    }

    /// The sync-metrics handle this readahead reports through (the chaser's own atomics).
    #[must_use]
    pub fn metrics(&self) -> &Arc<SyncMetrics> {
        &self.metrics
    }

    /// Current adaptive depth K (dispatch also respects the resident-bytes budget).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Windows currently fetched-or-in-flight.
    #[must_use]
    pub fn inflight(&self) -> usize {
        self.inflight.len()
    }

    /// `true` when `peer_id` is at its per-peer window cap ([`PrefetchConfig::per_peer`]) — the
    /// striping exclusion. At the shipped default (`per_peer == 1`) this is exactly "already carries
    /// an in-flight window"; the aggressive knob raises the cap so a peer may carry a bounded fan-out.
    /// The caller uses it to pick a direct-fetch source that is not already saturated by the readahead.
    #[must_use]
    pub fn busy_peer(&self, peer_id: u64) -> bool {
        self.peer_window_count(peer_id) >= self.cfg.per_peer
    }

    // Windows currently in flight on `peer_id`'s connection.
    fn peer_window_count(&self, peer_id: u64) -> usize {
        self.inflight.iter().filter(|w| w.peer_id == peer_id).count()
    }

    /// Top the pipeline up: dispatch adjacent windows of `batch` heights (the last one capped at
    /// `claimed`) across the live peers — the fewest-in-flight peer first, at most
    /// [`PrefetchConfig::per_peer`] windows each (one per peer at the shipped default) — until the
    /// admissible depth is reached, starting at `next_from` or after the last in-flight window,
    /// whichever is higher. Windows entirely
    /// below `next_from` are stale (the base moved past them) and are aborted first. Call this
    /// right after [`WindowReadahead::take`], BEFORE validating — the whole point is that these
    /// fetches run during validation.
    pub fn fill(
        &mut self,
        sources: &[Arc<dyn BlockRangeSource>],
        next_from: u32,
        claimed: u32,
        batch: u32,
    ) {
        while self.inflight.front().is_some_and(|w| w.to < next_from) {
            if let Some(w) = self.inflight.pop_front() {
                w.handle.abort();
            }
        }
        // Admissible depth: the adaptive K, clamped by the config ceiling AND — the OOM bound — by
        // the measured-size resident-bytes budget. `max_inflight` additionally caps the AGGREGATE
        // windows in flight, the concurrency ceiling the aggressive knob raises past the shipped 8.
        let admissible = depth_within_budget(self.depth, self.window_bytes_ewma, self.cfg.byte_budget)
            .min(self.cfg.max_depth)
            .min(self.cfg.max_inflight);
        let mut from = self
            .inflight
            .back()
            .map_or(next_from, |w| w.to.saturating_add(1))
            .max(next_from);
        while self.inflight.len() < admissible && from <= claimed && !sources.is_empty() {
            let to = claimed.min(from.saturating_add(batch.saturating_sub(1)));
            let mut chosen = None;
            // Prefer the peer with the FEWEST in-flight windows (still under its per-peer cap), so the
            // aggregate fan-out spreads evenly across peers instead of piling onto the rotation head —
            // this is what raises aggregate concurrency WITHOUT flooding one connection.
            let mut best_count = self.cfg.per_peer;
            for i in 0..sources.len() {
                let idx = (self.rotation + i) % sources.len();
                let s = &sources[idx];
                if s.is_closed() {
                    continue;
                }
                let count = self.peer_window_count(s.peer_id());
                if count < best_count {
                    best_count = count;
                    chosen = Some((idx, s.clone()));
                    // A wholly-idle peer is the ideal pick; take it immediately.
                    if count == 0 {
                        break;
                    }
                }
            }
            // Every live peer is at its per-peer cap: dispatching more would flood a connection (and,
            // at `per_peer == 1`, risk a message-id collision between source instances) — stop here.
            let Some((idx, src)) = chosen else { break };
            self.rotation = (idx + 1) % sources.len();
            let peer_id = src.peer_id();
            let timeout = self.request_timeout;
            let span = tracing::info_span!("readahead.fetch", peer = peer_id, from, to);
            let handle = tokio::spawn(
                async move {
                    match tokio::time::timeout(timeout, src.fetch_range(from, to)).await {
                        Ok(r) => r,
                        Err(_) => Err(SyncError::PeerStalled(peer_id)),
                    }
                }
                .instrument(span),
            );
            self.inflight.push_back(Inflight {
                from,
                to,
                peer_id,
                handle,
            });
            from = to.saturating_add(1);
        }
        self.publish_gauges();
    }

    /// Hand the validator the window `[from, to]` if it is the readahead's head window: await
    /// just that head (bounded by the dispatch-time request timeout), leaving every deeper
    /// window in flight — a slow peer serving window N+3 can never stall the take (and hence
    /// the confirm) of window N. Returns `None` when the head fetch failed (the caller
    /// direct-fetches this window; the tail stays valid) or when the plan no longer matches the
    /// request (a claimed-cap drift or a base jump — everything is aborted and the next
    /// [`WindowReadahead::fill`] replans). The wait, hit, and miss are recorded into
    /// [`SyncMetrics`]; the measured wait drives the adaptive depth.
    pub async fn take(&mut self, from: u32, to: u32) -> Option<Vec<FullBlock>> {
        while self.inflight.front().is_some_and(|w| w.to < from) {
            if let Some(w) = self.inflight.pop_front() {
                w.handle.abort();
            }
        }
        let head_matches = self
            .inflight
            .front()
            .is_some_and(|w| w.from == from && w.to == to);
        if !head_matches {
            if !self.inflight.is_empty() {
                self.abort_all();
            }
            self.metrics
                .readahead_misses
                .fetch_add(1, Ordering::Relaxed);
            self.publish_gauges();
            return None;
        }
        let head = self.inflight.pop_front()?;
        let waited = Instant::now();
        let result = head.handle.await;
        let wait = waited.elapsed();
        self.metrics
            .follow_fetch_wait_micros
            .fetch_add(wait.as_micros() as u64, Ordering::Relaxed);
        self.adapt_depth(wait);
        self.publish_gauges();
        match result {
            Ok(Ok(mut blocks)) if !blocks.is_empty() => {
                blocks.sort_by_key(FullBlock::height);
                let bytes: u64 = blocks.iter().map(approx_block_bytes).sum();
                self.window_bytes_ewma = if self.window_bytes_ewma == 0 {
                    bytes
                } else {
                    (self.window_bytes_ewma * 7 + bytes) / 8
                };
                self.metrics.readahead_hits.fetch_add(1, Ordering::Relaxed);
                Some(blocks)
            }
            // Fetch failed (peer error/timeout/abort) or came back empty: this window falls
            // back to a direct fetch by the caller; the deeper windows remain valid and in
            // flight.
            _ => {
                self.metrics
                    .readahead_misses
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Abort every in-flight fetch and clear the plan (near-tip handoff, shutdown, replan).
    pub fn abort_all(&mut self) {
        for w in self.inflight.drain(..) {
            w.handle.abort();
        }
        self.publish_gauges();
    }

    // The adaptive-K policy: a take that measurably waited grows the depth (validator idle =
    // pipeline too shallow); a long streak of instant takes shrinks it back (network running
    // ahead = budget to give back). Bounds: [READAHEAD_MIN_DEPTH, READAHEAD_MAX_DEPTH]; the
    // resident-bytes budget is applied at dispatch time (`depth_within_budget`).
    fn adapt_depth(&mut self, wait: Duration) {
        if wait > GROW_WAIT_THRESHOLD {
            self.zero_wait_streak = 0;
            if self.depth < self.cfg.max_depth {
                self.depth += 1;
            }
        } else if wait < Duration::from_millis(1) {
            self.zero_wait_streak += 1;
            if self.zero_wait_streak >= SHRINK_STREAK {
                self.zero_wait_streak = 0;
                if self.depth > READAHEAD_MIN_DEPTH {
                    self.depth -= 1;
                }
            }
        } else {
            self.zero_wait_streak = 0;
        }
    }

    fn publish_gauges(&self) {
        self.metrics
            .readahead_depth
            .store(self.depth as u64, Ordering::Relaxed);
        self.metrics
            .readahead_inflight
            .store(self.inflight.len() as u64, Ordering::Relaxed);
        // The O(W·id) witness the reservation gauge already charts: height identifiers
        // fetched-or-in-flight ahead of the validator.
        let ids: usize = self
            .inflight
            .iter()
            .map(|w| (w.to.saturating_sub(w.from) as usize).saturating_add(1))
            .sum();
        self.metrics.peak_window.fetch_max(ids, Ordering::Relaxed);
    }
}

impl Drop for WindowReadahead {
    fn drop(&mut self) {
        for w in self.inflight.drain(..) {
            w.handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PrefetchConfig, READAHEAD_ABS_MAX_DEPTH, READAHEAD_BYTE_BUDGET, READAHEAD_MAX_DEPTH,
        READAHEAD_MAX_PER_PEER, READAHEAD_MIN_DEPTH, depth_within_budget,
    };

    // The resident-budget clamp: admissible depth is depth × ewma ≤ budget, floored at the
    // minimum overlap, and a zero EWMA (nothing resolved yet) admits the requested depth.
    #[test]
    fn budget_clamps_depth_but_never_below_the_floor() {
        // 4 windows at 10 MiB each fit a 64 MiB budget.
        assert_eq!(depth_within_budget(4, 10 << 20, 64 << 20), 4);
        // 8 windows at 10 MiB do not: 64/10 = 6.
        assert_eq!(depth_within_budget(8, 10 << 20, 64 << 20), 6);
        // Giant windows clamp to the floor, never zero.
        assert_eq!(depth_within_budget(8, 512 << 20, 64 << 20), 1);
        // No EWMA yet: requested depth stands.
        assert_eq!(depth_within_budget(5, 0, 64 << 20), 5);
    }

    // Unset == the shipped default behavior, field for field (the byte-identical contract).
    #[test]
    fn default_config_reproduces_the_shipped_bounds() {
        let d = PrefetchConfig::default();
        assert_eq!(d.byte_budget, READAHEAD_BYTE_BUDGET);
        assert_eq!(d.byte_budget, 256 * 1024 * 1024);
        assert_eq!(d.max_depth, READAHEAD_MAX_DEPTH);
        assert_eq!(d.max_inflight, READAHEAD_MAX_DEPTH);
        assert_eq!(d.per_peer, 1);
    }

    // `--prefetch-memory-mb` alone: the budget grows, depth/aggregate-in-flight climb far past the
    // shipped 8 (bounded by the anti-flood ceiling), and the fan-out spreads across peers.
    #[test]
    fn aggressive_scales_budget_depth_and_concurrency() {
        // 8 GiB, no explicit in-flight cap, planning for the 8-peer W==P target.
        let a = PrefetchConfig::aggressive(8192, None, 8);
        assert_eq!(a.byte_budget, 8192 * 1024 * 1024);
        // Aggregate in-flight defaults to the anti-flood ceiling: peers × per-peer cap = 8 × 16.
        assert_eq!(a.max_inflight, 8 * READAHEAD_MAX_PER_PEER);
        assert!(a.max_inflight > READAHEAD_MAX_DEPTH, "concurrency exceeds the shipped 8");
        assert_eq!(a.max_depth, a.max_inflight, "resident depth ceiling tracks the aggregate");
        // Spread ACROSS peers, not flooded onto one: ceil(128 / 8) = 16 per peer.
        assert_eq!(a.per_peer, a.max_inflight.div_ceil(8));
        assert!(a.per_peer <= READAHEAD_MAX_PER_PEER);
    }

    // The explicit `--prefetch-max-inflight` count knob caps the fan-out independently of the byte
    // budget, and the per-peer spread follows from it.
    #[test]
    fn explicit_max_inflight_sets_the_fanout() {
        let a = PrefetchConfig::aggressive(16384, Some(24), 8);
        assert_eq!(a.max_inflight, 24);
        // 24 requests across 8 peers = 3 per peer — raised aggregate, never per-peer flooding.
        assert_eq!(a.per_peer, 3);
        // Even a request cap below the shipped 8 keeps the resident-depth ceiling at the default
        // floor (aggressive never downgrades the shipped overlap).
        let small = PrefetchConfig::aggressive(16384, Some(2), 8);
        assert_eq!(small.max_inflight, 2);
        assert_eq!(small.max_depth, READAHEAD_MAX_DEPTH);
    }

    // Every bound is hard: an absurd budget and in-flight request can never spawn an unbounded
    // task fan-out or flood a peer.
    #[test]
    fn bounds_are_hard_regardless_of_the_knob() {
        let a = PrefetchConfig::aggressive(u64::MAX, Some(1_000_000), 8);
        assert!(a.max_inflight <= READAHEAD_ABS_MAX_DEPTH);
        assert!(a.max_inflight <= 8 * READAHEAD_MAX_PER_PEER);
        assert!(a.per_peer <= READAHEAD_MAX_PER_PEER);
        assert!(a.max_depth <= READAHEAD_ABS_MAX_DEPTH);
    }

    // OOM-safety: the budget bound is in BYTES (measured window size), so however huge the budget,
    // the admissible depth collapses far below the window-count ceiling at dust-era block sizes and
    // the resident bodies never exceed the budget — bar the one irreducible floor window the
    // direct-fetch path also holds.
    #[test]
    fn huge_budget_collapses_depth_at_dust_era_block_sizes() {
        // A dust-era window measured at ~455 MiB/block × 32 heights ≈ 14.2 GiB.
        let dust_window_bytes: u64 = 455 * 1024 * 1024 * 32;
        // An ENORMOUS 64 GiB budget admits only a handful of such windows — depth collapses far
        // below the 128-window ceiling, and the resident bodies stay within the budget.
        let big = PrefetchConfig::aggressive(64 * 1024, None, 8);
        let admissible = depth_within_budget(big.max_depth, dust_window_bytes, big.byte_budget)
            .min(big.max_depth)
            .min(big.max_inflight);
        assert!(
            admissible < big.max_depth,
            "huge budget still collapses depth: {admissible} vs ceiling {}",
            big.max_depth
        );
        // Every window but the irreducible floor one fits inside the byte budget — the OOM ceiling.
        assert!(
            (admissible as u64)
                .saturating_sub(1)
                .saturating_mul(dust_window_bytes)
                <= big.byte_budget,
            "resident bodies (bar one floor window) stay within the byte budget"
        );
        // A budget smaller than a SINGLE dust-era window collapses all the way to the one-window
        // floor — the node can never hold more than the block it is validating.
        let tight = PrefetchConfig::aggressive(4 * 1024, None, 8); // 4 GiB < one ~14 GiB window
        let floored = depth_within_budget(tight.max_depth, dust_window_bytes, tight.byte_budget)
            .min(tight.max_depth)
            .min(tight.max_inflight);
        assert_eq!(floored, READAHEAD_MIN_DEPTH, "sub-window budget floors at one window");
    }
}
