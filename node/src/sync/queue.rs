//! The bounded, byte-budgeted block reorder buffer (`BlockQueue`) — the structure that decouples the
//! fetch producer from the confirm consumer.
//!
//! It is the canonical bounded-buffer producer/consumer `[CITED: Dijkstra 1968, "Cooperating
//! Sequential Processes", the bounded buffer; Hoare 1974, "Monitors", the bounded-buffer example]`
//! specialized to a reorder buffer: producers `complete` blocks OUT of height order (each window is
//! fetched from a distinct peer), the consumer `drain_next`s them strictly IN height order — exactly
//! TCP receive reassembly over the sequence space `[CITED: RFC 9293 §3.8]`. The bound is measured in
//! BYTES, not a block count, because block wire size varies ~1 KiB … 455 MiB across eras, the same
//! reason TCP's receive window is byte-denominated `[CITED: RFC 9293 §3.8.6]`.
//!
//! The keyed store is a `BTreeMap<Height, Slot>` so the consumer reads `first_key_value` (min height)
//! and the scheduler answers gap/range queries a heap could not — both `O(log n)`, `n` = window depth,
//! never chain height.
//!
//! Phase 1 lands this as a first-class, fully-proven type. The producer (`FetchScheduler`, phase 3) and
//! consumer (`BlockProcessor`, phase 2) are wired onto it in the later phases; the frozen validation
//! core it feeds is never changed, only re-sourced.

use crate::sync::SyncMetrics;
use crate::sync::prefetch::approx_block_bytes;
use dg_xch_core::blockchain::full_block::FullBlock;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Notify;

/// Block height — the queue's sequence space (the reorder key).
pub type Height = u32;

/// A reorder-buffer cell. A height is in exactly one of {absent, `InFlight`, `Present`, consumed},
/// so a scheduler never double-assigns and a hedge loser's late arrival is a
/// no-op on an already-`Present`/consumed slot.
enum Slot {
    /// Reserved by a peer that is fetching it; carries the hard reclaim deadline. `peer_id`/`deadline`
    /// are read by the phase-3 `FetchScheduler` (stall sweep + reassignment); phase 1 only needs the
    /// variant to be distinguishable from `Present` for gap/drain accounting. `gen` is the generation
    /// stamped at `admit`, carried through the fetch and back into `complete` for the ABA/stale-branch
    /// guard.
    InFlight {
        #[allow(dead_code)]
        peer_id: u64,
        #[allow(dead_code)]
        deadline: Instant,
        // Stamped at `admit`; the phase-3 `FetchScheduler` carries it with the fetch and passes it back
        // to `complete` for the stale-branch guard. In phase 2 the producer reads `current_gen` directly
        // (it uses the pass-through `complete`, not `admit`), so the slot copy is not yet read.
        #[allow(dead_code)]
        generation: u64,
    },
    /// Fetched and resident; `nbytes` is its wire-size charge against the byte budget.
    Present { block: Box<FullBlock>, nbytes: u64 },
}

struct Inner {
    /// Reorder buffer keyed by height. Out-of-order `complete` is `O(log n)`; the consumer only ever
    /// touches `first_key_value`.
    slots: BTreeMap<Height, Slot>,
    /// Resident PRESENT bytes (`B` in the invariants). `InFlight` slots carry no bytes — their size is
    /// unknown until they complete, at which point `B` may overshoot the budget by at most one block
    /// (the deliberate over-fill bias).
    bytes: u64,
    /// The consumer's next needed height (`= confirmed_peak + 1`); monotone non-decreasing under
    /// `drain_*`; the sole operation that lowers it is `rebase` on a reorg, in lockstep with the
    /// engine peak.
    low_water: Height,
    /// Reorg generation. Bumped by every `rebase`; stamped into each `InFlight` at `admit` and required
    /// back at `complete`. A completion whose `gen` no longer matches was fetched on a superseded branch
    /// and is dropped — the deterministic ABA/stale-completion guard. tokio task-abort of a
    /// pre-rebase fetch is racy (a task past its last `.await` still runs to `complete`), so the queue,
    /// not the aborter, is the authority on staleness.
    generation: u64,
}

/// A byte-bounded, height-keyed reorder buffer decoupling fetch (producer) from confirm (consumer).
///
/// Backpressure is the bound: a producer is admitted only while `B < budget` (strict, pre-add — the
/// "slightly over rather than never under" bias), parking on [`BlockQueue::wait_space`]
/// otherwise; the consumer parks on [`BlockQueue::wait_ready`] only when its exact head height is
/// missing, never on a deeper slow window (the HOL elimination at the queue layer). The
/// two waits form no cycle — producers wait on space the consumer releases, the consumer waits on the
/// head a producer completes — so the buffer is deadlock-free `[CITED: Coffman-Elphick-Shoshani 1971]`.
pub struct BlockQueue {
    inner: Mutex<Inner>,
    /// Hard byte ceiling `W` (default [`crate::sync::READAHEAD_BYTE_BUDGET`] = 256 MiB).
    budget: u64,
    /// Lock-free mirror of `Inner::bytes` for the resident-bytes gauge and `wait_space` fast path.
    resident_bytes: AtomicU64,
    /// Producer wakeup: fired when a `drain_next` frees budget (all parked producers re-contend).
    space: Notify,
    /// Consumer wakeup: fired when the slot at `low_water` becomes `Present`.
    ready: Notify,
    metrics: Arc<SyncMetrics>,
}

impl BlockQueue {
    /// A queue whose consumer starts at `low_water` (= `confirmed_peak + 1`), bounded by `budget`
    /// resident bytes.
    #[must_use]
    pub fn new(low_water: Height, budget: u64, metrics: Arc<SyncMetrics>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                slots: BTreeMap::new(),
                bytes: 0,
                low_water,
                generation: 0,
            }),
            budget,
            resident_bytes: AtomicU64::new(0),
            space: Notify::new(),
            ready: Notify::new(),
            metrics,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Resident PRESENT bytes `B` (the gauge value).
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        self.resident_bytes.load(Ordering::Relaxed)
    }

    /// The hard byte ceiling `W`.
    #[must_use]
    pub fn budget(&self) -> u64 {
        self.budget
    }

    /// Slots currently held (`InFlight` + `Present`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().slots.len()
    }

    /// `true` when the buffer holds no slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().slots.is_empty()
    }

    /// The consumer's next needed height.
    #[must_use]
    pub fn low_water(&self) -> Height {
        self.lock().low_water
    }

    /// The current reorg generation — a producer reads this when it dispatches a fetch and carries it
    /// back into [`BlockQueue::complete`], so a completion that lands after a [`BlockQueue::rebase`]
    /// (which bumped the generation) is recognised as stale and dropped.
    #[must_use]
    pub fn current_gen(&self) -> u64 {
        self.lock().generation
    }

    /// The over-fill admission gate: a producer may start a fetch while resident PRESENT
    /// bytes are strictly below the budget. Checked pre-add, so at most one in-flight block's worth of
    /// overshoot is admitted — never unbounded, never *under* budget waiting for an exact fit.
    #[must_use]
    pub fn can_admit(&self) -> bool {
        self.resident_bytes.load(Ordering::Relaxed) < self.budget
    }

    /// Reserve `height` for `peer_id` with a hard reclaim `deadline` (the scheduler's claim). No
    /// effect if the height is already tracked — a producer never clobbers a `Present` or a live claim.
    pub fn admit(&self, height: Height, peer_id: u64, deadline: Instant) {
        let mut inner = self.lock();
        if height < inner.low_water || inner.slots.contains_key(&height) {
            return;
        }
        let generation = inner.generation;
        inner.slots.insert(
            height,
            Slot::InFlight {
                peer_id,
                deadline,
                generation,
            },
        );
    }

    /// Mark `height` fetched and resident. Transitions `InFlight → Present` (or inserts `Present`
    /// directly on the producer-less pass-through path), charges its wire bytes against the budget, and
    /// — if it is the head the consumer is waiting on — signals `ready`. A block below `low_water`
    /// (already consumed), a duplicate `Present` (a hedge loser), or a completion whose `gen` no longer
    /// matches the queue (fetched on a branch a [`BlockQueue::rebase`] has since superseded) is dropped:
    /// idempotent on re-completion, deterministic across a reorg by the generation guard. The producer
    /// passes the `gen` it read via [`BlockQueue::current_gen`] when it dispatched the fetch.
    pub fn complete(&self, block: FullBlock, generation: u64) {
        let height = block.height();
        let nbytes = approx_block_bytes(&block);
        let mut inner = self.lock();
        if generation != inner.generation {
            return; // stale generation — fetched before a rebase, drop (ABA guard)
        }
        if height < inner.low_water {
            return; // already consumed — hedge/late duplicate, drop
        }
        if matches!(inner.slots.get(&height), Some(Slot::Present { .. })) {
            return; // duplicate present (hedge loser) — drop
        }
        inner.slots.insert(
            height,
            Slot::Present {
                block: Box::new(block),
                nbytes,
            },
        );
        inner.bytes = inner.bytes.saturating_add(nbytes);
        let is_head = height == inner.low_water;
        self.resident_bytes.store(inner.bytes, Ordering::Relaxed);
        self.publish_gauges(&inner);
        drop(inner);
        if is_head {
            self.ready.notify_waiters();
        }
    }

    /// Pop the head block iff it is `Present` at `low_water`; advance `low_water`, release its bytes,
    /// and wake parked producers. Returns `None` immediately when the head is absent or still
    /// `InFlight` — the consumer then `wait_ready`s, never blocking on a deeper slow window (the
    /// HOL elimination). This is the only mutation that advances `low_water` (monotone).
    pub fn drain_next(&self) -> Option<FullBlock> {
        let mut inner = self.lock();
        let head = inner.low_water;
        if !matches!(inner.slots.get(&head), Some(Slot::Present { .. })) {
            return None; // head absent or still InFlight — never surface a non-head slot
        }
        let Some(Slot::Present { block, nbytes }) = inner.slots.remove(&head) else {
            return None;
        };
        inner.bytes = inner.bytes.saturating_sub(nbytes);
        inner.low_water = inner.low_water.saturating_add(1);
        self.resident_bytes.store(inner.bytes, Ordering::Relaxed);
        self.publish_gauges(&inner);
        drop(inner);
        self.space.notify_waiters();
        Some(*block)
    }

    /// Drain the maximal contiguous run of `Present` blocks starting at `low_water`, up to `max`
    /// blocks, in one lock acquisition — the consumer's batch pull, so the frozen validation core still
    /// receives a whole height-ordered window for its all-core body/VDF/sig precompute drains (behavior
    /// unchanged; only the input's provenance moved from the driver's inline fetch to the queue). Returns
    /// an empty vec when the head is absent or still `InFlight`; the consumer then `wait_ready`s. Advances
    /// `low_water` past every drained block (monotone) and releases their bytes, waking producers.
    pub fn drain_ready_window(&self, max: u32) -> Vec<FullBlock> {
        let mut inner = self.lock();
        let mut out = Vec::new();
        while (out.len() as u32) < max {
            let head = inner.low_water;
            let Some(Slot::Present { .. }) = inner.slots.get(&head) else {
                break; // head absent or InFlight — never surface a non-head slot
            };
            let Some(Slot::Present { block, nbytes }) = inner.slots.remove(&head) else {
                break;
            };
            inner.bytes = inner.bytes.saturating_sub(nbytes);
            inner.low_water = inner.low_water.saturating_add(1);
            out.push(*block);
        }
        if !out.is_empty() {
            self.resident_bytes.store(inner.bytes, Ordering::Relaxed);
            self.publish_gauges(&inner);
            drop(inner);
            self.space.notify_waiters();
        }
        out
    }

    /// Reorg rewind: reset the consumer's head to `new_low_water` and invalidate every
    /// outstanding fetch. The driver calls this in lockstep with an engine peak change — a reorg to a
    /// fork (`new_low_water = fork_height + 1`, a backward move) OR a forward jump when a driver-side
    /// path (bulk/anchor/fast-sync/backtrack/infusion) advanced the peak outside the consumer — so the
    /// head invariant (`low_water == confirmed_peak + 1`) is restored. Every queued slot is dropped (each was fetched
    /// on an assumption the peak change invalidated), the byte charge is zeroed exactly (freed = the sum
    /// over the removed `Present` slots), the generation is bumped so any in-flight fetch that lands late
    /// is dropped by the [`BlockQueue::complete`] guard, and parked producers are woken to replan from
    /// [`BlockQueue::gaps`] on the new branch. Deliberately does NOT signal `ready`: the new head is
    /// absent and must be re-fetched. Never awaits, never nests a lock, and only ever *decreases* the
    /// byte charge, so it cannot introduce a backpressure wait-cycle.
    ///
    /// This is the full-reset generalization of the `split_off` form: in the reorg case every
    /// slot sits at or above `new_low_water` so the two are identical, and clearing all slots
    /// additionally reclaims the byte charge of any slot *below* the new head on a forward jump (which
    /// `split_off` would leak).
    pub fn rebase(&self, new_low_water: Height) {
        let mut inner = self.lock();
        inner.slots.clear();
        inner.bytes = 0;
        inner.low_water = new_low_water;
        inner.generation = inner.generation.wrapping_add(1);
        self.resident_bytes.store(0, Ordering::Relaxed);
        self.publish_gauges(&inner);
        drop(inner);
        self.space.notify_waiters();
    }

    /// Release an `InFlight` claim (the scheduler's stall reclaim): the height returns to
    /// absent so another peer can take it. No effect on a `Present` slot.
    pub fn reclaim(&self, height: Height) {
        let mut inner = self.lock();
        if matches!(inner.slots.get(&height), Some(Slot::InFlight { .. })) {
            inner.slots.remove(&height);
        }
    }

    /// The heights in `[low_water, low_water + window)` that are neither `Present` nor `InFlight` — the
    /// scheduler's fill targets (`FindNextBlocksToDownload` shape), lowest first.
    #[must_use]
    pub fn gaps(&self, window: u32) -> Vec<Height> {
        let inner = self.lock();
        let start = inner.low_water;
        let end = start.saturating_add(window);
        (start..end)
            .filter(|h| !inner.slots.contains_key(h))
            .collect()
    }

    /// The next height a producer should fetch: one past the highest slot currently held, or
    /// `low_water` when the buffer is empty. In phase 2 the driver-owned readahead fills contiguously
    /// from the bottom up, so this is the fetch frontier; a `rebase` resets it (via `low_water`) so the
    /// producer re-plans on the new branch without a separate, coordination-prone cursor.
    #[must_use]
    pub fn next_fetch_height(&self) -> Height {
        let inner = self.lock();
        inner
            .slots
            .keys()
            .next_back()
            .map_or(inner.low_water, |h| h.saturating_add(1))
    }

    /// `true` when the head height is `Present` (the consumer can drain without parking).
    #[must_use]
    pub fn head_ready(&self) -> bool {
        let inner = self.lock();
        matches!(
            inner.slots.get(&inner.low_water),
            Some(Slot::Present { .. })
        )
    }

    /// Park until the head height is `Present`, then return. The consumer is blocked ONLY on its own
    /// next block — never on a deeper window (liveness).
    ///
    /// The `notified()` future is created BEFORE the condition check on purpose: a tokio `Notified`
    /// snapshots the `notify_waiters` generation at CREATION, so any `complete`/`rebase`
    /// `notify_waiters` that lands after this line — even before the future is first polled — is
    /// captured and wakes the `.await`. Combined with the post-wake condition recheck, no wakeup is
    /// lost. (Proven in `node/tests/block_queue.rs::notified_created_before_the_check_captures_a_later_notify_waiters`.)
    pub async fn wait_ready(&self) {
        loop {
            // Register interest BEFORE the check so a `complete` between check and await cannot be lost.
            let notified = self.ready.notified();
            if self.head_ready() {
                return;
            }
            notified.await;
        }
    }

    /// Park until resident PRESENT bytes fall below the budget (space to admit another block). The
    /// producer's bounded-buffer `full` wait. Same create-before-check discipline as
    /// [`BlockQueue::wait_ready`]: the `Notified` snapshots the notify generation at creation, so a
    /// `drain_*`/`rebase` `notify_waiters` after this line is never lost.
    pub async fn wait_space(&self) {
        loop {
            let notified = self.space.notified();
            if self.can_admit() {
                return;
            }
            notified.await;
        }
    }

    fn publish_gauges(&self, inner: &Inner) {
        self.metrics
            .queue_resident_bytes
            .store(inner.bytes, Ordering::Relaxed);
        self.metrics
            .queue_len
            .store(inner.slots.len() as u64, Ordering::Relaxed);
        // The O(W) witness the readahead's peak_window gauge already charts: heights held ahead of the
        // consumer. Tracking it here lets phase 1 prove the queue mirrors the old in-flight depth.
        self.metrics
            .peak_window
            .fetch_max(inner.slots.len(), Ordering::Relaxed);
    }
}
