mod headers;
pub mod peer_manager;
pub mod prefetch;
pub mod queue;
pub mod source;
pub mod watchdog;
pub mod window;

use crate::cache::BlockRecordCache;
use crate::engine::{AddBlockOutcome, BlockDelta, Engine, ReorgReport};
use crate::error::NodeError;
use crate::primitives::ConsensusPrimitives;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_core::blockchain::header_block::HeaderBlock;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_core::blockchain::weight_proof::WeightProof;
use dg_xch_stores::{BlockStore, CoinStore};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::Instrument;

// The out-of-order write-through record: which header hash landed at which height, so the organizer can
// confirm bodies back in height order (the store is the reorder buffer).
type OrderedBodies = Arc<Mutex<BTreeMap<u32, Bytes32>>>;

pub use peer_manager::{
    Availability, FetchOutcome, LeasePriority, MAX_IN_TRANSIT_PER_PEER, PeerLease, PeerManager,
};
pub use prefetch::{
    PrefetchConfig, READAHEAD_ABS_MAX_DEPTH, READAHEAD_BYTE_BUDGET, READAHEAD_MAX_DEPTH,
    READAHEAD_MAX_PER_PEER, READAHEAD_MIN_DEPTH, READAHEAD_START_DEPTH, WindowReadahead,
    depth_within_budget,
};
pub use source::{BlockRangeSource, OutboundPeerSource, request_weight_proof};
pub use watchdog::StallWatchdog;
pub use window::{Claim, Reservation, ReservationWindow};

// Reservation-window width and outbound-slot target are one cross-crate contract: W == P ==
// `dg_xch_p2p::P2pSettings::target_outbound` (8). Over-provisioning past W wastes connections; under-
// provisioning starves the window.
pub const TARGET_OUTBOUND: usize = 8;

/// How far below the confirmed peak the short-sync backtrack searches for the fork point before giving
/// up and demanding a long sync — chia's `short_sync_backtrack` cap: it fetches single blocks backward
/// only `while curr_height > peak_height - 5` and returns `found_fork_point = False` past that
/// (chia full_node.py:738), which `new_peak` answers with the batch/long-sync fallback ladder
/// (chia full_node.py:845-873).
pub const BACKTRACK_MAX_DEPTH: u32 = 5;

/// Warm-up slack below the epoch-depth backfill span: covers the couple of records
/// `get_second_to_last_transaction_block_in_previous_epoch` reads BELOW the previous epoch
/// surpass (its two-transaction-block walk) and warms the light-path `required_iters` state so
/// the records the retarget actually reads near the surpass carry real values, never the zero
/// seed (see [`Chaser::backfill_epoch_depth`]).
pub const EPOCH_BACKFILL_SLACK: u32 = 128;

/// The lowest block-record height the next possible epoch retarget can read, for a node whose
/// forward validation resumes at height `at` (a confirmed peak, or a `--sync-from` anchor-span
/// base). Anything at or above this height must be present as a record before the follow loop
/// stages past the boundary, or the stage walk dies with "block record not found".
///
/// The retarget fires when the first new-slot block of the FIRST SUB-EPOCH after an epoch
/// boundary `B` is staged (chia `can_finish_sub_and_full_epoch`: `height + 1` must round down to
/// an epoch-aligned sub-epoch, so the trigger lies in `[B, B + sub_epoch_blocks)`), and its
/// `get_second_to_last_transaction_block_in_previous_epoch` walk reads records back past the
/// PREVIOUS epoch surpass `B - epoch_blocks` (chia difficulty_adjustment.py: the descent breaks
/// only once `curr.height < height_prev_epoch_surpass`).
///
/// The pending boundary is therefore the smallest epoch multiple `B` whose trigger window can
/// still be AHEAD of `at` — `B + sub_epoch_blocks > at` — not the next boundary above `at`.
/// Rounding `at` up to the next boundary (the pre-fix computation here and in the daemon's
/// resume repair) skipped one full epoch of needed records exactly when `at` sat inside the
/// first sub-epoch after a boundary whose retarget had not fired yet: the mainnet
/// 4,575,744-boundary wall — peak frozen at 4,575,757, trigger block 4,575,758, retarget walk
/// down to 4,571,135 against a record floor of 4,574,936.
#[must_use]
pub fn epoch_backfill_low(at: u32, epoch_blocks: u32, sub_epoch_blocks: u32) -> u32 {
    let pending_boundary =
        (at.saturating_sub(sub_epoch_blocks) / epoch_blocks + 1).saturating_mul(epoch_blocks);
    pending_boundary.saturating_sub(epoch_blocks + EPOCH_BACKFILL_SLACK)
}

/// How far past the stale local peak the long-sync reland re-follows before concluding the
/// weight-proof-attested branch will never outweigh it (a lying or stale serving peer). The
/// competing branch carries comparable per-block weight increments, so it outweighs the stale
/// peak within a handful of blocks of passing its height; three full windows is generous slack.
pub const LONG_SYNC_REORG_MARGIN: u32 = 96;

// How many locally-included sub-epoch summaries the fork-point walk collects at most: the
// agreement plus chia's two-sub-epoch conservative back-off need 3, and a few extra tolerate
// divergent summaries ABOVE the agreement (the reorg-across-the-gap case).
const WP_FORK_LOCAL_SES: usize = 8;

/// The fork point of a validated weight proof's summary chain against OUR stored chain — chia
/// `WeightProofHandler.get_fork_point` (chia/full_node/weight_proof.py:644-664) at the same
/// sub-epoch granularity. chia walks its full in-memory summary map bottom-up from genesis; a
/// checkpoint-anchored store does not have that map, so this walks TOP-DOWN from our peak —
/// equivalent because sub-epoch summaries hash-chain (`prev_subepoch_summary_hash`), so one
/// positional hash match proves the entire prefix below it agrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WpForkPoint {
    /// Every locally-visible summary agrees with the proof's at the same position: no fork is
    /// DETECTED — but chia still backs the sync start off two sub-epochs ("Two summaries can
    /// have different blocks and still be identical", weight_proof.py:659-661) unless the
    /// `check_fork_next_block` peer probe lifts it to the local peak
    /// (chia/full_node/check_fork_next_block.py).
    NoForkDetected {
        /// chia's conservative start: the height of the local summary two below the last
        /// agreement (0 when the agreement index is ≤ 2, weight_proof.py:659-663).
        conservative: u32,
    },
    /// A local summary disagrees with the proof's at the same position: the chains diverged in
    /// or below that sub-epoch during the offline gap.
    Diverged {
        /// chia's conservative start, as above — the long sync must REWIND to here through the
        /// engine's atomic reorg, never blindly extend the stale branch.
        fork_point: u32,
    },
    /// No positional agreement is visible within the walk window (no peak, no local summaries,
    /// or a divergence below the store's record floor): the fork point cannot be established
    /// and the caller must fail closed.
    Unknown,
}

/// Compute the [`WpForkPoint`] of validated weight-proof `summaries` against the chain in
/// `store` — the "where do the WP's chain and ours diverge" input to the long-sync band (chia
/// `_sync` → `validate_weight_proof` → `get_fork_point`, full_node.py:1104-1113). The walk
/// descends prev-hash-wise from the confirmed peak (store-fallback, so a cold restart works)
/// collecting locally-included sub-epoch summaries, and matches them positionally against the
/// proof's by hash. Mirrors chia exactly: the LAST received summary is never credited as an
/// agreement (chia's loop breaks before comparing it, weight_proof.py:650-652), and the
/// returned height backs off two sub-epochs below the agreement, clamped to 0 for the first
/// three (weight_proof.py:659-663).
///
/// # Errors
/// Returns [`SyncError::Io`] on a store read failure or an unhashable summary.
pub async fn wp_fork_point<S: BlockStore + Sync>(
    store: &S,
    summaries: &[SubEpochSummary],
    sub_epoch_blocks: u32,
) -> Result<WpForkPoint, SyncError> {
    if summaries.len() < 2 {
        return Ok(WpForkPoint::Unknown);
    }
    let Some((peak_hash, _)) = store.get_peak().await? else {
        return Ok(WpForkPoint::Unknown);
    };
    // hash → position over all but the LAST summary (chia never compares it).
    let mut idx_of: HashMap<Bytes32, usize> = HashMap::with_capacity(summaries.len() - 1);
    for (i, s) in summaries[..summaries.len() - 1].iter().enumerate() {
        idx_of.entry(s.hash().map_err(SyncError::Io)?).or_insert(i);
    }
    let last_hash = summaries[summaries.len() - 1]
        .hash()
        .map_err(SyncError::Io)?;
    // Collect (height, ses hash) descending from the peak. Bounded: at most WP_FORK_LOCAL_SES
    // summaries, and at most one over-length sub-epoch of records per summary (sub-epoch
    // boundaries drift past the nominal multiple by overflow blocks, never a full sub-epoch).
    let step_cap =
        (WP_FORK_LOCAL_SES as u32 + 1).saturating_mul(sub_epoch_blocks.saturating_mul(2));
    let mut cursor = peak_hash;
    let mut collected: Vec<u32> = Vec::new();
    let mut agree: Option<(usize, usize)> = None; // (summary index, position in `collected`)
    let mut mismatched = false;
    let mut steps = 0u32;
    loop {
        let Some(rec) = store.get_block_record(&cursor).await? else {
            break; // the store's record floor (checkpoint-anchored history ends here)
        };
        if let Some(ses) = &rec.sub_epoch_summary_included {
            let h = ses.hash().map_err(SyncError::Io)?;
            let pos = collected.len();
            collected.push(rec.height);
            if let Some(&i) = idx_of.get(&h) {
                if agree.is_none() {
                    agree = Some((i, pos));
                }
            } else if h != last_hash && agree.is_none() {
                // A summary the proof does not carry at any credited position: the chains
                // diverged at/below this sub-epoch. (A match on the excluded last summary is
                // neither an agreement nor a divergence — the next lower summary decides.)
                mismatched = true;
            }
            if let Some((_, p)) = agree
                && collected.len() > p + 2
            {
                break; // the two-below back-off height is in hand
            }
            if collected.len() >= WP_FORK_LOCAL_SES {
                break;
            }
        }
        if rec.height == 0 {
            break;
        }
        cursor = rec.prev_hash;
        steps += 1;
        if steps >= step_cap {
            break;
        }
    }
    let Some((agree_idx, pos)) = agree else {
        return Ok(WpForkPoint::Unknown);
    };
    // chia weight_proof.py:659-663: `if fork_point_index <= 2: return 0` else two summaries
    // below the agreement. A light store whose records end above the back-off height starts at
    // its record floor — the sync cannot begin below the history it holds.
    let conservative = if agree_idx <= 2 {
        0
    } else {
        match collected.get(pos + 2) {
            Some(&h) => h,
            None => store.min_record_height().await?.unwrap_or(0),
        }
    };
    if mismatched {
        Ok(WpForkPoint::Diverged {
            fork_point: conservative,
        })
    } else {
        Ok(WpForkPoint::NoForkDetected { conservative })
    }
}

/// Chaser pipeline knobs. Every field is a hard bound — the pipeline bounds everything.
#[derive(Clone, Copy, Debug)]
pub struct SyncConfig {
    /// P: parallel download slots = outbound peers = reservation slots (the W==P contract).
    pub peers: usize,
    /// Bounded pending-identifier window (heights, ~60 B each — never blocks). Refilled from
    /// `get_unassociated`; caps total in-flight identifiers so peak RAM is `O(W·id)`, flat in chain height.
    pub window: usize,
    /// Heights per reservation = one `RequestBlocks` span handed to one peer.
    pub batch: u32,
    /// Per-range fetch deadline; a peer that misses it has its reservation reclaimed to the pool.
    pub request_timeout: Duration,
    /// Assume-valid milestone; 0 = validate everything (fresh-genesis default).
    pub assume_valid: u32,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            peers: TARGET_OUTBOUND,
            // ~W identifiers: a few reservations deep per peer, still tiny (heights), never blocks.
            window: TARGET_OUTBOUND * 32,
            batch: 32,
            request_timeout: Duration::from_secs(30),
            assume_valid: 0,
        }
    }
}

/// One confirmed block the reporting follow paths hand the daemon for the per-peak side effects
/// (wallet coin-state push + mempool revalidation). `reorg` is `Some` exactly on the FIRST delta
/// of a reorg's re-applied branch — the daemon pushes the rolled-back states (with the true fork
/// height) before the branch's own coin deltas, mirroring chia's `WalletUpdate.coin_records =
/// state_change_summary.rolled_back_records + new_states` (chia full_node.py:2101-2119).
#[derive(Debug)]
pub struct ConfirmedDelta {
    pub delta: BlockDelta,
    pub reorg: Option<ReorgWalletDelta>,
}

impl ConfirmedDelta {
    // A plain (non-reorg) confirmed delta.
    fn plain(delta: BlockDelta) -> Self {
        Self { delta, reorg: None }
    }
}

/// The wallet-facing half of a landed reorg: the true fork height (chia
/// `state_change_summary.fork_height`) and the abandoned span's post-rollback coin records (chia
/// `rolled_back_records` — see [`ReorgReport::rolled_back`] for the exact state shapes).
#[derive(Debug)]
pub struct ReorgWalletDelta {
    pub fork_height: u32,
    pub rolled_back: Vec<CoinRecord>,
}

// Expand one window's engine outcomes into the reported confirm feed: plain extends report their
// own delta; a `Reorg` outcome replaces the triggering delta with the ENTIRE re-applied branch
// (fork+1..=tip, from the engine's [`ReorgReport`]) so subscribers hear every winning-branch
// block — chia's `new_states` covers the whole branch above the fork, not just the tip — with
// the rollback delta attached to the branch's first block. Orphans and `AlreadyHave` report
// nothing (unchanged). Pure so the expansion is unit-testable without a live pipeline.
fn expand_confirmed(
    outcomes: Vec<AddBlockOutcome>,
    confirmed: Vec<BlockDelta>,
    mut reports: impl FnMut() -> Option<ReorgReport>,
    deltas: &mut Vec<ConfirmedDelta>,
) {
    for (outcome, delta) in outcomes.into_iter().zip(confirmed) {
        match outcome {
            AddBlockOutcome::AlreadyHave | AddBlockOutcome::Orphan { .. } => {}
            AddBlockOutcome::Reorg { .. } => match reports() {
                Some(report) if !report.reapplied.is_empty() => {
                    let mut wallet = Some(ReorgWalletDelta {
                        fork_height: report.fork_height,
                        rolled_back: report.rolled_back,
                    });
                    for d in report.reapplied {
                        deltas.push(ConfirmedDelta {
                            delta: d,
                            reorg: wallet.take(),
                        });
                    }
                }
                // No report (drained by a non-reporting path — cannot happen from this loop, the
                // reporting confirm clears before it batches) or an empty branch: fall back to
                // the triggering delta so the peak advance is still reported.
                _ => deltas.push(ConfirmedDelta::plain(delta)),
            },
            _ => deltas.push(ConfirmedDelta::plain(delta)),
        }
    }
}

/// Live pipeline instrumentation: the peak in-flight counts prove the `O(W·id + P·block)`
/// bound directly — window identifiers and simultaneously-resident downloaded blocks both stay bounded as
/// chain height grows. Feeds the `sync.batch` / `reservation` tracing spans; cheap atomics, zero-cost when
/// nobody reads them.
#[derive(Default)]
pub struct SyncMetrics {
    pub blocks_downloaded: AtomicU64,
    pub blocks_confirmed: AtomicU64,
    pub reclaimed: AtomicU64,
    pub peak_window: AtomicUsize,
    pub peak_inflight_blocks: AtomicUsize,
    // Last-window phase wall times in microseconds — the per-backend differentiators the
    // Grafana per-node pages chart (VDF drain, parallel body precompute, batched confirm).
    pub window_vdf_micros: AtomicU64,
    // Header-signature all-core drain wall time (the third precompute phase; the header-sig batch).
    pub window_sig_micros: AtomicU64,
    pub window_body_micros: AtomicU64,
    // Sequential staging-loop wall time (per-block prepare_delta store reads + record derivation +
    // the window staging commit) — the phase between the parallel body precompute and the VDF
    // drain that was previously UNMEASURED: live windows showed vdf+body+confirm ≈ 0.78 s against
    // a ~2.7 s window wall, and this gauge is what attributes that residue.
    pub window_stage_micros: AtomicU64,
    pub window_confirm_micros: AtomicU64,
    // Last-window composition: total blocks and how many carried a transactions generator.
    // window.body runs ONLY the generator blocks, so charting body time against these two
    // series separates transaction cost from the non-transaction baseline.
    pub window_blocks: AtomicU64,
    pub window_tx_blocks: AtomicU64,
    // Depth of the most recent reorg (peak height minus fork height; 0 = none yet) — the
    // chia-exporter-style last_reorg_depth signal.
    pub last_reorg_depth: AtomicU64,
    // Engine collection sizes sampled each follow window (retention bisect): with jemalloc
    // proving `allocated` itself climbs, whichever of these tracks the climb names the retainer.
    pub engine_cache_records: AtomicU64,
    pub engine_pending_orphans: AtomicU64,
    pub engine_staged_generators: AtomicU64,
    // Difficulty-window record serving: how many records the daemon's
    // consensus-walk maps took from the in-memory record window vs re-read from the store.
    // store_reads collapsing to the per-peak head delta is the trough fix's live witness.
    pub difficulty_window_cache_hits: AtomicU64,
    pub difficulty_window_store_reads: AtomicU64,
    // Follow-pipeline idle attribution: cumulative µs the follow driver waited on the
    // network for its NEXT window (readahead take + direct-fetch fallback) and cumulative µs of
    // whole follow steps (wait + stage + VDF drain + confirm). Validator idle fraction =
    // rate(fetch_wait) / rate(step) — the live judge of the fetch-pipeline overlap.
    pub follow_fetch_wait_micros: AtomicU64,
    pub follow_step_micros: AtomicU64,
    // Window readahead: current adaptive depth K, windows in flight, and the hit
    // (window served from the pipeline) / miss (fallback direct fetch or replan) counters.
    pub readahead_depth: AtomicU64,
    pub readahead_inflight: AtomicU64,
    pub readahead_hits: AtomicU64,
    pub readahead_misses: AtomicU64,
    // Bounded block queue (sync-decoupling phase 1): resident PRESENT bytes held in the reorder
    // buffer (the byte-budget gauge) and the count of slots (InFlight + Present) held ahead of the
    // consumer. The byte gauge is the producer/consumer backpressure witness the later phases chart.
    pub queue_resident_bytes: AtomicU64,
    pub queue_len: AtomicU64,
}

#[derive(Debug)]
pub enum SyncError {
    Node(NodeError),
    Io(std::io::Error),
    /// A peer rejected or timed out on a reservation; the range is reclaimed, the peer is dropped for it.
    PeerStalled(u64),
    /// A peer answered a block-range request with `RejectBlocks` — it cannot serve `start..=end` (behind our
    /// requested height, or missing the bodies). Non-fatal: the reservation is reclaimed for another peer.
    RangeRejected {
        start: u32,
        end: u32,
    },
    /// A peer answered `RespondBlocks` but the returned blocks do not cover the requested `start..=end`
    /// (wrong count, or non-contiguous / off-by heights). With correlation now id-routed this should
    /// not occur, but the check is defense-in-depth against a misbehaving peer echoing another range;
    /// like [`SyncError::RangeRejected`] it is non-fatal — the reservation is reclaimed for another peer.
    RangeMismatch {
        start: u32,
        end: u32,
        got: usize,
    },
    /// A returned block did not match the headers-first candidate chain: its body hashes to no committed
    /// weight-proof-attested header at the requested height (a re-stamped, forged, or wrong-anchor body —
    /// hardening). The candidate records the header pass stored ARE the connect target (the tip/fork
    /// for a normal window, the weight-proof-attested anchor header for the from-empty path), so a
    /// body that binds to none — or to a candidate at a different height — is rejected BEFORE the
    /// write-through, never accepted through the from-empty `prev = None` entry, and never allowed to hit
    /// the store's `block_body → block_record` foreign key as a fatal error. Non-fatal like
    /// [`SyncError::RangeMismatch`]: the batch is dropped, the reservation reclaimed for another peer, and
    /// this peer retired by the worker's failure budget (chia bans the peer for
    /// `CONSENSUS_ERROR_BAN_SECONDS`; the timed ban list is keyed on the peer HOST at the server accept
    /// seam and is not reachable through the [`crate::sync::source::BlockRangeSource`] abstraction, so the
    /// worker's drop-and-deprioritize is the posture here).
    BatchUnlinked {
        start: u32,
        end: u32,
        height: u32,
    },
    /// No peer could serve a still-pending reservation — the window cannot drain.
    Exhausted(u32),
    /// The short-sync backtrack walked [`BACKTRACK_MAX_DEPTH`] blocks below the peak without finding a
    /// parent we have — the fork is deeper than the short-sync regime covers. The caller must fall back
    /// to the long-sync/weight-proof path (chia: `short_sync_backtrack` returns `found_fork_point =
    /// False` past its cap, full_node.py:736-774, and `new_peak` falls through to batch sync and then
    /// `_sync()`, full_node.py:845-873) — never retry the same forward window.
    DeepFork {
        /// The base of the forward window that orphaned (our peak + 1).
        base: u32,
        /// The exclusive lower bound of the probe (chia's `peak_height - 5`): every height above it
        /// was fetched and none connected.
        floor: u32,
    },
}

impl SyncError {
    /// `true` when the failure is the engine's unknown-parent rejection ([`NodeError::Orphan`]) — the
    /// signal that the peer's chain forked at/below our stored tip and a forward re-fetch of the same
    /// window can never succeed. The driver answers it with [`Chaser::follow_backtrack_reporting`].
    #[must_use]
    pub fn is_orphan(&self) -> bool {
        matches!(self, SyncError::Node(NodeError::Orphan(_)))
    }

    /// `true` when the failure is a `NotFound` from a consensus ancestry walk ("block record not
    /// found") — the anchor-window-gap signal: a retarget/SES lookback on the stage path needed a
    /// record below the store's record floor (or below the warmed cache edge). Retrying the
    /// identical window can never succeed; the driver answers it by re-arming the resume repair
    /// (floor re-measure + epoch-depth backfill + cache re-warm), the header-fetch analog of chia
    /// re-requesting missing ancestry.
    #[must_use]
    pub fn is_missing_record(&self) -> bool {
        matches!(
            self,
            SyncError::Node(NodeError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound
        )
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Node(e) => write!(f, "sync node error: {e}"),
            SyncError::Io(e) => write!(f, "sync io error: {e}"),
            SyncError::PeerStalled(p) => write!(f, "peer {p} stalled on its reservation"),
            SyncError::RangeRejected { start, end } => {
                write!(f, "peer rejected block range {start}..={end}")
            }
            SyncError::RangeMismatch { start, end, got } => write!(
                f,
                "peer answered block range {start}..={end} with {got} blocks that do not cover it"
            ),
            SyncError::BatchUnlinked { start, end, height } => write!(
                f,
                "peer's block range {start}..={end} contains a block at height {height} that does not \
                 match the headers-first candidate chain (re-stamped / forged / non-connecting body)"
            ),
            SyncError::Exhausted(h) => write!(f, "no peer can serve height {h}"),
            SyncError::DeepFork { base, floor } => write!(
                f,
                "fork behind height {base} is deeper than the backtrack floor {floor}; long sync required"
            ),
        }
    }
}

impl Error for SyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            SyncError::Node(e) => Some(e),
            SyncError::Io(e) => Some(e),
            SyncError::PeerStalled(_)
            | SyncError::RangeRejected { .. }
            | SyncError::RangeMismatch { .. }
            | SyncError::BatchUnlinked { .. }
            | SyncError::Exhausted(_)
            | SyncError::DeepFork { .. } => None,
        }
    }
}

impl From<NodeError> for SyncError {
    fn from(e: NodeError) -> Self {
        SyncError::Node(e)
    }
}
impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Io(e)
    }
}
impl From<dg_xch_stores::StoreError> for SyncError {
    fn from(e: dg_xch_stores::StoreError) -> Self {
        SyncError::Node(NodeError::Store(e))
    }
}

/// The chaser: headers-first candidate chain → splittable reservation window → out-of-order write-through
/// download → parallel validation → in-order confirm. Owns the block-validation engine; the
/// store is the reorder buffer, never RAM.
pub struct Chaser<S, P> {
    engine: Engine<S, P>,
    config: SyncConfig,
    metrics: Arc<SyncMetrics>,
    // Bounded height window of candidate header records the headers-first pass populates; the ancestry the
    // full validator then reads (bounded — flat in chain height).
    header_cache: BlockRecordCache,
}

impl<S, P> Chaser<S, P>
where
    S: CoinStore + BlockStore + Sync,
    P: ConsensusPrimitives + Sync,
{
    #[must_use]
    pub fn new(engine: Engine<S, P>, config: SyncConfig) -> Self {
        Self {
            engine: engine.with_assume_valid(config.assume_valid),
            config,
            metrics: Arc::new(SyncMetrics::default()),
            header_cache: BlockRecordCache::with_default_window(),
        }
    }

    #[must_use]
    pub fn engine(&self) -> &Engine<S, P> {
        &self.engine
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<SyncMetrics> {
        &self.metrics
    }

    #[must_use]
    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    /// Headers-first candidate chain. Validate each header's proof of space against the running tip
    /// cache and store its candidate record (no body); `get_unassociated` then reports these heights as
    /// bodies-pending. Returns the count of candidate records stored. Full PoW/VDF validation fires off the
    /// populated cache via [`Chaser::validate_stored_header`] — the sync-built ancestry is what triggers it,
    /// closing the single-block validator's hand-seeded-ancestor caveat. `ssi`/`difficulty` anchor the checkpoint epoch (from the
    /// weight proof in production).
    ///
    /// # Errors
    /// Returns [`SyncError::Node`] if any header's proof of space is invalid or the store rejects a record.
    pub async fn sync_headers(
        &mut self,
        headers: &[HeaderBlock],
        schedule: &EpochSchedule,
        summaries: &[SubEpochSummary],
    ) -> Result<usize, SyncError> {
        // Await-safe instrumentation: attach the span to the future (never an Entered guard across
        // the `.await`) — the sharded-registry clone-after-close panic under work-stealing.
        let span = tracing::info_span!("sync.headers", count = headers.len());
        headers::sync_header_chain(
            &self.engine,
            &mut self.header_cache,
            headers,
            schedule,
            summaries,
        )
        .instrument(span)
        .await
    }

    /// The weight-proof-attested per-height epoch schedule for this chain's constants.
    #[must_use]
    pub fn epoch_schedule(&self, summaries: &[SubEpochSummary]) -> EpochSchedule {
        let c = self.engine.constants();
        EpochSchedule::from_summaries(
            summaries,
            c.sub_epoch_blocks,
            c.sub_slot_iters_starting,
            c.difficulty_starting,
        )
    }

    /// Load the recent record ancestry from the store into the engine's walk cache — after a restart
    /// or a backfill. See [`Engine::warm_cache_from_store`].
    ///
    /// # Errors
    /// Returns [`SyncError`] on a store read failure.
    pub async fn warm_engine_cache(&mut self) -> Result<usize, SyncError> {
        Ok(self.engine.warm_cache_from_store().await?)
    }

    /// Epoch-depth header backfill below the checkpoint anchor. The live epoch retarget at the
    /// PENDING boundary (`get_second_to_last_transaction_block_in_previous_epoch`) walks records
    /// back past the PREVIOUS epoch surpass — up to a full epoch below the anchor, records a
    /// checkpoint sync never fetched. Fetch those blocks, build their candidate records
    /// headers-first with per-height epoch parameters (the summary schedule), and store the
    /// RECORDS only (no bodies) — the walks stay inside known records once
    /// [`Chaser::warm_engine_cache`] loads them. A slack of blocks before the span warms the
    /// light-path walk state so the records the retarget actually reads (`sp_total_iters` near
    /// the surpass) carry real `required_iters`, never a fabricated zero seed. The span floor comes
    /// from [`epoch_backfill_low`] — the PENDING boundary's depth, not the next boundary above
    /// the anchor (that rounding skipped a whole epoch of needed records whenever the anchor sat
    /// inside the first sub-epoch after a boundary whose retarget had not fired yet: the mainnet
    /// 4,575,744-boundary wall). Returns the number of backfilled records.
    ///
    /// # Errors
    /// Returns [`SyncError`] if no source can serve the span or a header fails validation.
    pub async fn backfill_epoch_depth(
        &mut self,
        sources: &[Arc<dyn BlockRangeSource>],
        summaries: &[SubEpochSummary],
        anchor: u32,
    ) -> Result<usize, SyncError>
    where
        S: Clone + Send + Sync + 'static,
    {
        let (epoch_blocks, sub_epoch_blocks) = {
            let c = self.engine.constants();
            (c.epoch_blocks, c.sub_epoch_blocks)
        };
        let low = epoch_backfill_low(anchor, epoch_blocks, sub_epoch_blocks);
        if low >= anchor {
            return Ok(0);
        }
        // Await-safe instrumentation: the fetch loop and header sync below both `.await`, so the
        // span is attached to the whole async section rather than held as an Entered guard.
        let span = tracing::info_span!("sync.backfill", low, high = anchor - 1);
        async move {
            let schedule = self.epoch_schedule(summaries);

            let mut headers: Vec<HeaderBlock> = Vec::with_capacity((anchor - low) as usize);
            let mut start = low;
            while start < anchor {
                let end = (start + self.config.batch - 1).min(anchor - 1);
                let mut fetched = None;
                for source in sources {
                    if source.is_closed() {
                        continue;
                    }
                    match source.fetch_range(start, end).await {
                        Ok(blocks) if !blocks.is_empty() => {
                            fetched = Some(blocks);
                            break;
                        }
                        Ok(_) | Err(_) => {}
                    }
                }
                let Some(mut blocks) = fetched else {
                    return Err(SyncError::Exhausted(start));
                };
                blocks.sort_by_key(FullBlock::height);
                for b in &blocks {
                    headers.push(crate::engine::header_block_from_full_block(b));
                }
                start = end + 1;
            }

            let mut cache = BlockRecordCache::with_default_window();
            let stored = headers::sync_header_chain(
                &self.engine,
                &mut cache,
                &headers,
                &schedule,
                summaries,
            )
            .await?;
            Ok(stored)
        }
        .instrument(span)
        .await
    }

    /// Full single-block PoW/VDF validation of a header against the ancestry the headers-first pass populated
    /// (headers-first). This is the closed-caveat proof: the ancestor map comes from [`Chaser::sync_headers`], not a
    /// hand-built fixture. `ssi`/`difficulty` are the block's epoch parameters.
    ///
    /// # Errors
    /// Returns [`SyncError::Node`] if the PoW/VDF is invalid or an ancestor is missing from the cache.
    pub fn validate_stored_header(
        &self,
        header: &HeaderBlock,
        ssi: u64,
        difficulty: u64,
    ) -> Result<u64, SyncError> {
        Ok(self.engine.validate_header_block(
            self.header_cache.records(),
            header,
            dg_xch_core::consensus::block_header_validation::ValidationState { ssi, difficulty },
            false,
        )?)
    }

    /// Splittable reservation window + parallel write-through download. Split the pending candidate
    /// heights (`get_unassociated`, identifiers only) across up to `peers` sources; each downloaded body is
    /// written through to the store out-of-order (`begin`/`append_many`/`commit`) as it arrives — never
    /// buffered in RAM. A source that misses `request_timeout` has its reservation reclaimed to the pool so
    /// another peer finishes it, no gap. Peak RAM is `O(W·id + P·block)`: flat in chain height (the window
    /// holds heights, the store holds bodies) and flat in peer count (bounded by `peers`).
    ///
    /// # Errors
    /// Returns [`SyncError::Exhausted`] if candidate heights remain unwritten after every source drained,
    /// or a store/download error from a worker.
    pub async fn sync_bodies(
        &mut self,
        sources: &[Arc<dyn BlockRangeSource>],
    ) -> Result<(), SyncError>
    where
        S: Clone + Send + Sync + 'static,
    {
        self.download_all(sources).await.map(drop)
    }

    // Fan the reservation window across the peers, write every body through, and return the height-ordered
    // set of downloaded (height, header_hash) — the confirm organizer's in-order feed.
    async fn download_all(
        &self,
        sources: &[Arc<dyn BlockRangeSource>],
    ) -> Result<BTreeMap<u32, Bytes32>, SyncError>
    where
        S: Clone + Send + Sync + 'static,
    {
        let store = self.engine.store().clone();
        let window = Arc::new(Mutex::new(ReservationWindow::new(self.config.window)));
        let bodies: OrderedBodies = Arc::new(Mutex::new(BTreeMap::new()));
        let mut set: JoinSet<Result<(), SyncError>> = JoinSet::new();
        for src in sources.iter().take(self.config.peers) {
            set.spawn(download_worker(
                store.clone(),
                window.clone(),
                bodies.clone(),
                src.clone(),
                self.config,
                self.metrics.clone(),
            ));
        }
        let mut first_err = None;
        while let Some(joined) = set.join_next().await {
            let err = match joined {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => e,
                Err(e) => SyncError::Io(std::io::Error::other(e)),
            };
            first_err.get_or_insert(err);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        // Every peer drained; if candidate heights still lack bodies, every source stalled — fail closed.
        let leftover = store.get_unassociated(1).await?;
        if let Some(&h) = leftover.first() {
            return Err(SyncError::Exhausted(h));
        }
        Ok(Arc::try_unwrap(bodies)
            .map(Mutex::into_inner)
            .unwrap_or_default())
    }

    /// Parallel write-through download + in-order confirm. Download the candidate range across the
    /// peers, then confirm the bodies back **in height order** through the engine: each is validated,
    /// its coins applied, and the confirmation pointer (`set_peak`) flipped forward. Confirm reads one body at
    /// a time from the store, so RAM stays O(1) in the range while the store is the reorder buffer. Returns the
    /// confirmed peak.
    ///
    /// # Errors
    /// Returns [`SyncError`] on a download failure, a body that fails validation, or a store error.
    pub async fn sync_range(
        &mut self,
        sources: &[Arc<dyn BlockRangeSource>],
    ) -> Result<Option<(Bytes32, u32)>, SyncError>
    where
        S: Clone + Send + Sync + 'static,
    {
        let bodies = self.download_all(sources).await?;
        for (height, hh) in bodies {
            // Await-safe instrumentation: the per-block `validate.parallel` span is attached to
            // the future for this block (the store read + `add_block` both `.await`); the loop's
            // `continue` becomes an early `return` from the instrumented block.
            let span = tracing::info_span!("validate.parallel", height);
            async {
                let Some(block) = self.engine.store().get_block(&hh).await? else {
                    return Ok::<(), SyncError>(());
                };
                match self.engine.add_block(&block).await? {
                    AddBlockOutcome::AlreadyHave => {}
                    AddBlockOutcome::Reorg { fork_height, .. } => {
                        self.metrics.last_reorg_depth.store(
                            u64::from(height.saturating_sub(fork_height)),
                            Ordering::Relaxed,
                        );
                        self.metrics
                            .blocks_confirmed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {
                        self.metrics
                            .blocks_confirmed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                Ok::<(), SyncError>(())
            }
            .instrument(span)
            .await?;
        }
        // This non-reporting path has no consumer for reorg reports; drop them so they can never
        // mis-attach to a later reporting window (the sync-end transition covers the wallet side
        // of a fast-sync landing).
        self.engine.clear_reorg_reports();
        Ok(self.engine.store().get_peak().await?)
    }

    /// Fast sync via the weight proof (the HAVE light-verify path). Validate the proof (all six phases:
    /// sampling, sub-epoch summaries, weight, sampled segments, recent-block PoSpace, total) and return the
    /// attested peak `(header_hash, height)`. This is the same peak a full sync reaches; recent bodies are then
    /// filled through [`Chaser::sync_range`] — the identical download/confirm pipeline — from the peak backward.
    ///
    /// # Errors
    /// Returns [`SyncError::Io`] if the weight proof fails any validation phase or carries no recent chain.
    pub fn fast_sync_peak(
        &self,
        wp: &dg_xch_core::blockchain::weight_proof::WeightProof,
    ) -> Result<(Bytes32, u32), SyncError> {
        let span = tracing::info_span!("sync.fast", recent = wp.recent_chain_data.len());
        let _e = span.enter();
        let (valid, _summaries) =
            dg_xch_weight_proof::validate_weight_proof(wp, self.engine.constants()).map_err(
                |e| SyncError::Io(std::io::Error::other(format!("weight proof: {e:?}"))),
            )?;
        if !valid {
            return Err(SyncError::Io(std::io::Error::other(
                "weight proof did not validate",
            )));
        }
        let tip = wp.recent_chain_data.last().ok_or_else(|| {
            SyncError::Io(std::io::Error::other("weight proof has no recent chain"))
        })?;
        Ok((tip.header_hash()?, tip.height()))
    }

    /// From-zero bulk sync via the weight proof — the driver composed from the sync
    /// primitives (weight-proof validate → headers-first → download/confirm), reusing them, not
    /// reinventing. Validate the proof, epoch-anchor the recent-chain header walk from the proof's summaries
    /// (a naive genesis-constant anchor poisons the validator with `required_iters == 0`), populate the
    /// candidate window, then download + confirm the recent bodies through the reservation pipeline across
    /// `sources`. Returns the confirmed peak. Recent-chain only: this lands the node at the attested near-tip
    /// so the cheap tip-follow driver takes over — it does not backfill deep history.
    ///
    /// # Errors
    /// Returns [`SyncError::Io`] if the weight proof fails validation, or a download/validation/store error
    /// from the header or body pass.
    pub async fn fast_sync(
        &mut self,
        wp: &WeightProof,
        sources: &[Arc<dyn BlockRangeSource>],
    ) -> Result<Option<(Bytes32, u32)>, SyncError>
    where
        S: Clone + Send + Sync + 'static,
    {
        let (valid, summaries) =
            dg_xch_weight_proof::validate_weight_proof(wp, self.engine.constants()).map_err(
                |e| SyncError::Io(std::io::Error::other(format!("weight proof: {e:?}"))),
            )?;
        if !valid {
            return Err(SyncError::Io(std::io::Error::other(
                "weight proof did not validate",
            )));
        }
        self.fast_sync_with_summaries(wp, &summaries, sources).await
    }

    /// The header-anchor + body-fill half of [`Chaser::fast_sync`], split out so the caller can run the
    /// multi-minute [`dg_xch_weight_proof::validate_weight_proof`] OFF the async runtime (a `spawn_blocking`
    /// pool) and OFF the chaser lock, then re-enter here with the already-verified `summaries`. This is the
    /// recovery path: a body-download failure retries ONLY this cheap leg — it must never re-run the
    /// weight-proof verify (that is what wedged the from-zero node into an 18-min-per-tick loop).
    ///
    /// # Errors
    /// Returns [`SyncError`] on a header-validation, download, or store error from the recent-chain fill.
    pub async fn fast_sync_with_summaries(
        &mut self,
        wp: &WeightProof,
        summaries: &[SubEpochSummary],
        sources: &[Arc<dyn BlockRangeSource>],
    ) -> Result<Option<(Bytes32, u32)>, SyncError>
    where
        S: Clone + Send + Sync + 'static,
    {
        // Await-safe instrumentation: attach the span to the header+body async section rather than
        // holding an Entered guard across the two `.await`s.
        let span = tracing::info_span!("sync.fast_bulk", recent = wp.recent_chain_data.len());
        async move {
            let schedule = self.epoch_schedule(summaries);
            self.sync_headers(&wp.recent_chain_data, &schedule, summaries)
                .await?;
            self.sync_range(sources).await
        }
        .instrument(span)
        .await
    }

    /// Short sync: follow a peer's newly-announced tip. On a `new_peak` at `to_height`, pull the small
    /// delta `from_height..=to_height` from a peer and confirm each block in order through the engine. This is
    /// the tip-tracking path (a handful of blocks), distinct from the bulk reservation-window long sync.
    /// Returns the confirmed peak.
    ///
    /// # Errors
    /// Returns [`SyncError`] if the peer cannot serve the delta or a block fails validation.
    pub async fn follow_to(
        &mut self,
        source: &Arc<dyn BlockRangeSource>,
        from_height: u32,
        to_height: u32,
    ) -> Result<Option<(Bytes32, u32)>, SyncError> {
        // Await-safe instrumentation: the original code dropped the `sync.short` guard BEFORE
        // `follow_blocks` (which emits its own `window.*` spans), so the span covers only the
        // fetch+sort. Preserve that exact scope by instrumenting just the fetch future.
        let span = tracing::info_span!("sync.short", from = from_height, to = to_height);
        let blocks = async {
            let mut blocks = source.fetch_range(from_height, to_height).await?;
            blocks.sort_by_key(dg_xch_core::blockchain::full_block::FullBlock::height);
            Ok::<_, SyncError>(blocks)
        }
        .instrument(span)
        .await?;
        self.follow_blocks(&blocks).await
    }

    /// The confirm half of [`Chaser::follow_to`] over pre-fetched blocks — lets the driver overlap
    /// the NEXT window's download with this window's validation (the fetch is otherwise a fully
    /// serial network stall in the follow loop).
    ///
    /// # Errors
    /// Returns [`SyncError`] if a block fails validation or the store errors.
    pub async fn follow_blocks(
        &mut self,
        blocks: &[dg_xch_core::blockchain::full_block::FullBlock],
    ) -> Result<Option<(Bytes32, u32)>, SyncError> {
        // The cross-block window pipeline lives in follow_blocks_reporting; this caller
        // just drops the reported deltas.
        Ok(self.follow_blocks_reporting(blocks).await?.0)
    }

    /// Short sync that also returns the per-block deltas of newly confirmed blocks — the daemon feeds
    /// these to the wallet coin-state subscription server and the mempool's new-peak revalidation. Deltas are
    /// returned in height order; `AlreadyHave` and orphan outcomes contribute none.
    ///
    /// # Errors
    /// Returns [`SyncError`] if the peer cannot serve the delta or a block fails validation.
    pub async fn follow_to_reporting(
        &mut self,
        source: &Arc<dyn BlockRangeSource>,
        from_height: u32,
        to_height: u32,
    ) -> Result<(Option<(Bytes32, u32)>, Vec<ConfirmedDelta>), SyncError> {
        // Await-safe instrumentation: as in `follow_to`, the `sync.short` span covers only the
        // fetch+sort (the guard was dropped before `follow_blocks_reporting`). Preserve that scope.
        let span = tracing::info_span!("sync.short", from = from_height, to = to_height);
        let blocks = async {
            let mut blocks = source.fetch_range(from_height, to_height).await?;
            blocks.sort_by_key(dg_xch_core::blockchain::full_block::FullBlock::height);
            Ok::<_, SyncError>(blocks)
        }
        .instrument(span)
        .await?;
        self.follow_blocks_reporting(&blocks).await
    }

    /// The mirrored `short_sync_backtrack` (chia full_node.py:715-774) — the recovery arm for a follow
    /// window that failed with the unknown-parent orphan ([`SyncError::is_orphan`]): the peer's chain
    /// forked at/below our stored tip, so no forward re-fetch of `from_height..` can ever connect.
    /// Fetch single blocks BACKWARD from below the failed window (chia fetches one at a time via
    /// `request_block`, full_node.py:743-745; our range seam issues the equivalent one-height
    /// `RequestBlocks`), collecting until a block whose parent we have (chia's `height_to_hash(h-1) ==
    /// prev_header_hash` fork-point test, full_node.py:753-759) or the [`BACKTRACK_MAX_DEPTH`] cap
    /// trips (chia's `while curr_height > peak_height - 5`, full_node.py:738). On a found fork point,
    /// submit the collected chain lowest-first (chia full_node.py:761-768) together with the re-fetched
    /// forward window through the ordinary follow pipeline — the engine's existing fork choice parks
    /// the shared-height blocks as orphan candidates and reorgs the moment the branch outweighs the
    /// peak, exactly as if the blocks had arrived in order. Returns the confirmed peak and the
    /// confirmed deltas (the daemon's wallet/mempool feed).
    ///
    /// # Errors
    /// [`SyncError::DeepFork`] when no fork point exists within the cap (the caller must long-sync,
    /// never retry); [`SyncError::Exhausted`] when the peer cannot serve a backtracked height; any
    /// validation/store error from the resubmitted chain.
    pub async fn follow_backtrack_reporting(
        &mut self,
        source: &Arc<dyn BlockRangeSource>,
        from_height: u32,
        to_height: u32,
    ) -> Result<(Option<(Bytes32, u32)>, Vec<ConfirmedDelta>), SyncError> {
        let peak_height = from_height.saturating_sub(1);
        let floor = peak_height.saturating_sub(BACKTRACK_MAX_DEPTH);
        // Await-safe instrumentation: the collection loop below `.await`s per backtracked height,
        // and the original dropped the `sync.backtrack` guard before `follow_blocks_reporting`
        // (which emits its own `window.*` spans). Instrument the collection future; keep the final
        // confirm outside the span exactly as before.
        let span = tracing::info_span!("sync.backtrack", peak = peak_height, floor, to = to_height);
        let collected = async {
            let mut collected: Vec<FullBlock> = Vec::new();
            let mut found_fork_point = false;
            // chia full_node.py:738 — `while curr_height > peak_height - 5`: the probed heights are
            // peak, peak-1, …, peak-4 (a signed bound in the reference, so genesis is reachable when
            // the peak itself is shallower than the cap).
            for depth in 0..BACKTRACK_MAX_DEPTH {
                let Some(curr_height) = peak_height.checked_sub(depth) else {
                    break;
                };
                let fetched = source.fetch_range(curr_height, curr_height).await?;
                let Some(block) = fetched.into_iter().find(|b| b.height() == curr_height) else {
                    return Err(SyncError::Exhausted(curr_height));
                };
                let prev_hash = block.prev_header_hash();
                collected.push(block);
                // chia full_node.py:753-759 — genesis reached, or the parent is a block we have.
                if curr_height == 0
                    || self
                        .engine
                        .store()
                        .get_block_record(&prev_hash)
                        .await?
                        .is_some()
                {
                    found_fork_point = true;
                    break;
                }
            }
            if !found_fork_point {
                return Err(SyncError::DeepFork {
                    base: from_height,
                    floor,
                });
            }
            tracing::info!(
                backtracked = collected.len(),
                fork_height = collected.last().map_or(0, |b| b.height().saturating_sub(1)),
                "short-sync backtrack found the fork point"
            );
            // chia full_node.py:761-768 adds the collected chain in reverse (lowest first); chia backtracked
            // from the claimed peak itself, whereas we backtracked from below the already-failed forward
            // window — so re-fetch that window and confirm the whole branch in one height-ordered pass.
            if from_height <= to_height {
                collected.extend(source.fetch_range(from_height, to_height).await?);
            }
            collected.sort_by_key(FullBlock::height);
            Ok::<_, SyncError>(collected)
        }
        .instrument(span)
        .await?;
        self.follow_blocks_reporting(&collected).await
    }

    /// One tip-follow step as chia's `new_peak` ladder (chia full_node.py new_peak): try the FORWARD
    /// extend first — the overwhelmingly common tip case is a block whose parent IS our peak, which
    /// `follow_to_reporting` confirms by fetching only `[from_height, to_height]`. Fall back to the
    /// backward `short_sync_backtrack` arm ONLY when the forward window fails with the unknown-parent
    /// orphan ([`SyncError::is_orphan`]) — a genuine reorg at/below our tip. Going straight to
    /// `follow_backtrack_reporting` (its depth-0 probe is `from_height - 1`, our own peak) re-fetches
    /// the peak block and re-confirms it `AlreadyHave` on EVERY step, adding a needless backward fetch
    /// + peer round-trip per block; that overhead is what keeps a follower from pinning tip at lag 0-1.
    pub async fn follow_tip_step_reporting(
        &mut self,
        source: &Arc<dyn BlockRangeSource>,
        from_height: u32,
        to_height: u32,
    ) -> Result<(Option<(Bytes32, u32)>, Vec<ConfirmedDelta>), SyncError> {
        match self
            .follow_to_reporting(source, from_height, to_height)
            .await
        {
            Err(e) if e.is_orphan() => {
                self.follow_backtrack_reporting(source, from_height, to_height)
                    .await
            }
            other => other,
        }
    }

    /// The reorg-across-the-gap arm of the WP-anchored long sync (chia `_sync` →
    /// `sync_from_fork_point` with a fork point BELOW the peak, full_node.py:1104-1113): re-follow
    /// forward windows from `fork_point + 1`. Blocks identical to ours confirm as `AlreadyHave`; a
    /// divergent branch stages as orphan candidates and reorgs ATOMICALLY through the engine's
    /// single-transaction fork choice (T0-4) the moment it outweighs the stale peak — never a
    /// blind extension of the dead branch. Returns as soon as the confirmed peak leaves the entry
    /// peak (the follow pipeline then extends from the re-landed chain), and fails closed
    /// [`LONG_SYNC_REORG_MARGIN`] heights past the stale tip without movement: the weight proof
    /// said the chains diverged but the served branch never outweighed (a lying or stale peer —
    /// the caller retries next tick with another).
    ///
    /// # Errors
    /// [`SyncError::Io`] when the margin is exhausted without the peak moving (or no peak exists —
    /// from-zero belongs to the fast-sync band); [`SyncError::Exhausted`] when the peer serves an
    /// empty window; any validation/store error from the resubmitted windows.
    pub async fn long_sync_reland_reporting(
        &mut self,
        source: &Arc<dyn BlockRangeSource>,
        fork_point: u32,
    ) -> Result<(Option<(Bytes32, u32)>, Vec<ConfirmedDelta>), SyncError> {
        let Some((entry_hash, entry_height)) = self.engine.store().get_peak().await? else {
            return Err(SyncError::Io(std::io::Error::other(
                "long-sync reland requires a confirmed peak (from-zero is the fast-sync band)",
            )));
        };
        let cap = entry_height.saturating_add(LONG_SYNC_REORG_MARGIN);
        let span = tracing::info_span!("sync.reland", fork_point, entry = entry_height, cap);
        async move {
            let mut peak = Some((entry_hash, entry_height));
            let mut deltas: Vec<ConfirmedDelta> = Vec::new();
            let mut lo = fork_point.saturating_add(1);
            while lo <= cap {
                let hi = lo
                    .saturating_add(self.config.batch.saturating_sub(1))
                    .min(cap);
                let mut blocks = source.fetch_range(lo, hi).await?;
                if blocks.is_empty() {
                    return Err(SyncError::Exhausted(lo));
                }
                blocks.sort_by_key(FullBlock::height);
                let (window_peak, mut window_deltas) =
                    self.follow_blocks_reporting(&blocks).await?;
                deltas.append(&mut window_deltas);
                if let Some(p) = window_peak {
                    peak = Some(p);
                }
                if let Some((hash, _)) = peak
                    && hash != entry_hash
                {
                    return Ok((peak, deltas));
                }
                lo = hi.saturating_add(1);
            }
            Err(SyncError::Io(std::io::Error::other(format!(
                "long-sync reland from fork {fork_point}: peak did not move within \
                 {LONG_SYNC_REORG_MARGIN} blocks past the stale tip {entry_height}"
            ))))
        }
        .instrument(span)
        .await
    }

    /// Generator back-ref heights in `blocks` that neither the span itself, the staged
    /// overlay, nor the confirmed store can resolve. A mid-chain anchor (`--sync-from`) hits
    /// these when a compression ref points below the anchor span; the daemon fetches each from
    /// a peer and seeds it via [`Self::seed_ref_generator`] before following the window.
    pub async fn missing_ref_heights(
        &self,
        blocks: &[dg_xch_core::blockchain::full_block::FullBlock],
    ) -> Vec<u32> {
        let in_span: std::collections::HashSet<u32> = blocks
            .iter()
            .filter(|b| b.transactions_generator.is_some())
            .map(FullBlock::height)
            .collect();
        let mut missing = std::collections::BTreeSet::new();
        for block in blocks {
            for r in &block.transactions_generator_ref_list {
                if in_span.contains(r)
                    || missing.contains(r)
                    || self.engine.staged_generator(*r).is_some()
                {
                    continue;
                }
                if matches!(
                    self.engine.store().get_generator_at_height(*r).await,
                    Ok(Some(_))
                ) {
                    continue;
                }
                missing.insert(*r);
            }
        }
        missing.into_iter().collect()
    }

    /// Seed an out-of-span generator ref into the engine's staged overlay — see
    /// [`Self::missing_ref_heights`].
    pub fn seed_ref_generator(
        &mut self,
        height: u32,
        generator: dg_xch_core::clvm::program::SerializedProgram,
    ) {
        self.engine.seed_generator(height, generator);
    }

    /// Wipe the out-of-span seed cache — the daemon calls this at the start of each
    /// `seed_missing_refs` pass so the cache carries only the current window's refs (bounded,
    /// eviction-free). See [`crate::engine::Engine::clear_seed_generators`].
    pub fn clear_seed_generators(&mut self) {
        self.engine.clear_seed_generators();
    }

    /// The confirm half of [`Chaser::follow_to_reporting`] over pre-fetched, height-sorted blocks —
    /// the driver overlaps the next window's download with this window's validation.
    ///
    /// # Errors
    /// Returns [`SyncError`] if a block fails validation or the store errors.
    pub async fn follow_blocks_reporting(
        &mut self,
        blocks: &[dg_xch_core::blockchain::full_block::FullBlock],
    ) -> Result<(Option<(Bytes32, u32)>, Vec<ConfirmedDelta>), SyncError> {
        {
            let (cache, pending, staged) = self.engine.collection_sizes();
            self.metrics
                .engine_cache_records
                .store(cache as u64, Ordering::Relaxed);
            self.metrics
                .engine_pending_orphans
                .store(pending as u64, Ordering::Relaxed);
            self.metrics
                .engine_staged_generators
                .store(staged as u64, Ordering::Relaxed);
        }
        // The daemon's follow path — same cross-block window pipeline as `follow_blocks`, plus
        // the confirmed deltas for the wallet/mempool side effects.
        //
        // Window body precompute: the expensive PURE half of body validation (CLVM generator run
        // + BLS aggregate verify) for every transaction block of the window runs across all cores
        // BEFORE the sequential stage loop — the same pattern as the window VDF batch. Inputs are
        // resolved without validation state: generator refs from the window's own blocks (else the
        // confirmed store) and the prev-transaction flag key from the window scan (else the
        // store). The engine accepts a precompute only when its own stage-time flag key matches;
        // otherwise it silently recomputes inline — behavior identical, cost saved on the match.
        let mut pre_bodies: std::collections::HashMap<u32, crate::engine::PrecomputedBody> = {
            let by_height: std::collections::HashMap<u32, &FullBlock> =
                blocks.iter().map(|b| (b.height(), b)).collect();
            let mut jobs: Vec<(
                &FullBlock,
                Vec<dg_xch_core::consensus::block_generator::GeneratorReference>,
                bool,
            )> = Vec::new();
            for block in blocks {
                if !block.is_transaction_block() || block.transactions_generator.is_none() {
                    continue;
                }
                // Refs: in-window first, confirmed store second; unresolvable → skip precompute
                // (the engine's inline path reports the real error).
                let mut refs = Vec::with_capacity(block.transactions_generator_ref_list.len());
                let mut ok = true;
                for (i, r) in block.transactions_generator_ref_list.iter().enumerate() {
                    let generator = match by_height
                        .get(r)
                        .and_then(|b| b.transactions_generator.clone())
                    {
                        Some(g) => Some(g),
                        None => match self.engine.staged_generator(*r) {
                            Some(g) => Some(g.clone()),
                            None => self
                                .engine
                                .store()
                                .get_generator_at_height(*r)
                                .await
                                .ok()
                                .flatten(),
                        },
                    };
                    match generator {
                        Some(g) => refs.push(
                            dg_xch_core::consensus::block_generator::GeneratorReference {
                                height: *r,
                                index: u32::try_from(i).unwrap_or(u32::MAX),
                                generator: g,
                            },
                        ),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    continue;
                }
                let verify_sig = block.height() >= self.engine.assume_valid();
                jobs.push((block, refs, verify_sig));
            }
            self.metrics
                .window_blocks
                .store(blocks.len() as u64, Ordering::Relaxed);
            self.metrics
                .window_tx_blocks
                .store(jobs.len() as u64, Ordering::Relaxed);
            if jobs.is_empty() {
                self.metrics.window_body_micros.store(0, Ordering::Relaxed);
                std::collections::HashMap::new()
            } else {
                // Purely-synchronous section (a scoped-thread CPU batch, no `.await`): use
                // `in_scope`, which exits the span when the closure returns — the invariant "no
                // Entered guard may cross an `.await`" is then structurally enforced here.
                let span = tracing::info_span!("window.body", tx_blocks = jobs.len());
                span.in_scope(|| {
                    let body_started = std::time::Instant::now();
                    let primitives = self.engine.primitives();
                    let constants = *self.engine.constants();
                    // Core-bounded workers over job chunks — the same shape as the `vdf.batch`
                    // drain (`header::verify_vdf_batch`), and for the same two reasons, the second
                    // fatal on the Pi envelope: a thread PER transaction block oversubscribes the
                    // CPUs with no throughput gain, and it multiplies peak memory by the WINDOW
                    // size instead of the core count. Each generator run holds its own CLVM heap:
                    // ~125 MB average at the first mainnet transaction blocks (~225.8k), but
                    // 350-455 MiB MEASURED per dust-era compressed block (a dust-dense mainnet window —
                    // the ROM bootstrap run retains every eval intermediate in the run arena, and
                    // ref-list decompression through the CLVM deserializer dominates). 32
                    // concurrent runs allocated ~4 GiB of anon heap in ~5 s and OOM-killed the
                    // 4 GiB node on that window every restart; on an 8-core cgroup the bound
                    // still admits ~3.6 GiB transient bursts on dust-era windows (the observed
                    // resident-memory spikes). Bounded workers cap the resident runs at the core
                    // count (4 on a Pi-4: ~1.8 GiB dust-era worst case), with per-run heaps freed
                    // between the jobs a worker executes sequentially. The durable shrink is the
                    // compact-arena CLVM representation (NodePtr-style, clvm_rs's footprint), not
                    // a lower worker count.
                    let workers = std::thread::available_parallelism()
                        .map(std::num::NonZeroUsize::get)
                        .unwrap_or(4)
                        .min(jobs.len());
                    let chunk = jobs.len().div_ceil(workers);
                    let out = std::thread::scope(|s| {
                        let handles: Vec<_> =
                            jobs.chunks(chunk)
                                .map(|part| {
                                    s.spawn(move || {
                                        part.iter()
                                            .filter_map(|(block, refs, verify_sig)| {
                                                crate::engine::run_body_expensive(
                                                    primitives,
                                                    &constants,
                                                    block,
                                                    refs,
                                                    *verify_sig,
                                                )
                                                .ok()
                                                .map(|(conds, verified)| {
                                                    (
                                                        block.height(),
                                                        crate::engine::PrecomputedBody {
                                                            conds,
                                                            agg_sig_verified: verified,
                                                        },
                                                    )
                                                })
                                            })
                                            .collect::<Vec<_>>()
                                    })
                                })
                                .collect();
                        handles
                            .into_iter()
                            .filter_map(|h| h.join().ok())
                            .flatten()
                            .collect()
                    });
                    self.metrics
                        .window_body_micros
                        .store(body_started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    out
                })
            }
        };

        let sink = crate::header::HeaderSink::default();
        // Each staged block carries its two window-queue high-water marks: the VDF-proof mark and
        // the header-signature mark. On a drain failure the per-block slice `[start..hi]` is exactly
        // that block's deferred work, so the batch attributes the failing height precisely.
        let mut staged: Vec<(BlockDelta, usize, usize)> = Vec::new();
        let mut stage_err: Option<SyncError> = None;
        // Phase-aware STAGING commit granularity, mirroring `confirm_staged_batch`'s model on the
        // confirm side: near the tip each staged block commits its own archive transaction
        // (per-block durability is correct AT tip); during bulk catch-up the whole window's archive
        // rows accumulate into ONE transaction, committed once below — on the SQLite backend that
        // collapses 32 sequential writer-fsync round-trips per window into one. The batch opens
        // lazily at the first block that actually stages.
        let per_block_staging = self.engine.store().near_tip();
        let mut window_batch: Option<dg_xch_stores::BatchHandle> = None;
        let stage_started = std::time::Instant::now();
        // Batch the loop's per-block store reads for the whole window (ONE candidate multi-get +
        // one peak read — chia block_store.get_block_records_by_hash): the staging loop then
        // awaits no per-block AlreadyHave/candidate point reads (the measured catch-up residue).
        self.engine.preload_stage_context(blocks).await?;
        for block in blocks {
            let pre = pre_bodies.remove(&block.height());
            let outcome = if per_block_staging {
                self.engine.stage_block_pre(block, &sink, pre).await
            } else {
                self.engine
                    .stage_block_pre_in(block, &sink, pre, &mut window_batch)
                    .await
            };
            match outcome {
                Ok(Some(delta)) => {
                    let vdf_mark = sink.vdf.lock().map(|q| q.len()).unwrap_or(0);
                    let sig_mark = sink.sig.lock().map(|q| q.len()).unwrap_or(0);
                    staged.push((delta, vdf_mark, sig_mark));
                }
                Ok(None) => {}
                Err(e) => {
                    stage_err = Some(e.into());
                    break;
                }
            }
        }
        // The window staging commit: one writer transaction for every staged archive row. It must
        // land HERE — before the drain and before confirm's own `begin()` (the open batch holds
        // the single sqlite writer connection), and even when a mid-window stage error broke the
        // loop, because the staged prefix still confirms below and its confirm (`set_peak`) walks
        // these archive rows. A commit failure fails the window; the unconfirmed blocks re-stage
        // next tick, so the overlay entries must not linger (same posture as the drain failure).
        if let Some(b) = window_batch.take()
            && let Err(e) = self.engine.store().commit(b).await
        {
            self.engine.clear_staged_overlay();
            return Err(crate::error::NodeError::from(e).into());
        }
        // The staging loop is the read context's ONLY consumer: drop it here so no later path
        // (drain/confirm error, the next per-block follow) can consult this window's snapshot.
        // The error return above clears it through clear_staged_overlay.
        self.engine.clear_stage_preload();
        self.metrics.window_stage_micros.store(
            stage_started.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );

        // Silent-fallback ban: a poisoned sink (a panicked staging thread) must FAIL the window, never
        // yield an empty queue -- the old `unwrap_or_default()` made `queue.is_empty()` true and every
        // staged block confirmed with its VDF verification silently SKIPPED (a validation bypass on the
        // panic path). Fail closed; the unconfirmed window re-stages next tick.
        let (queue, sig_queue) = match drain_header_sink(sink) {
            Ok(q) => q,
            Err(e) => {
                self.engine.clear_staged_overlay();
                return Err(e);
            }
        };
        let mut confirm_upto = staged.len();
        let mut vdf_err: Option<SyncError> = None;
        if !queue.is_empty() {
            // Purely-synchronous section (the scoped-thread VDF batch drain, no `.await`): use
            // `in_scope` so the span exits when the closure returns — no Entered guard can leak
            // across the `confirm_staged_batch().await` that follows.
            let span =
                tracing::info_span!("window.vdf", proofs = queue.len(), blocks = staged.len());
            span.in_scope(|| {
                let vdf_started = std::time::Instant::now();
                if !self.engine.verify_vdf_window(queue.clone()) {
                    let mut start = 0usize;
                    confirm_upto = 0;
                    for (i, (delta, hi, _)) in staged.iter().enumerate() {
                        if !self.engine.verify_vdf_window(queue[start..*hi].to_vec()) {
                            confirm_upto = i;
                            vdf_err = Some(
                                crate::error::NodeError::Invalid(format!(
                                    "INVALID_VDF at height {} (window drain)",
                                    delta.height
                                ))
                                .into(),
                            );
                            break;
                        }
                        start = *hi;
                    }
                    if vdf_err.is_none() {
                        vdf_err = Some(
                            crate::error::NodeError::Invalid(
                                "INVALID_VDF in window drain (unattributed)".to_string(),
                            )
                            .into(),
                        );
                    }
                }
                self.metrics
                    .window_vdf_micros
                    .store(vdf_started.elapsed().as_micros() as u64, Ordering::Relaxed);
            });
        }

        // Header-signature drain: the THIRD all-core precompute phase (the 26.7% on-CPU serial
        // residue on a threadripper node). Same two-tier shape as the VDF drain — fast whole-window
        // batch, then on a failure a per-block slice replay that attributes the exact failing height
        // AND the exact chia rejection string (via the signature's tag). `confirm_upto` takes the
        // MINIMUM of the VDF- and sig-determined boundaries so a block bad in EITHER gate is never
        // confirmed; the reported error is whichever fails at the lower height (matching the
        // sequential path's first-bad-block stop).
        //
        // BOUNDED SEMANTIC (not a bug — see node/tests/era_replay.rs::corrupt_header_sig_*): because
        // the sig gates are DEFERRED, a block that is invalid in MULTIPLE gates can report a
        // different first-fault STRING than the fully-inline path would. The accept/reject decision
        // and the failing HEIGHT are always identical; only which rejection string surfaces first can
        // move. Concretely: rc_sp/cc_sp signatures live inside the reward-chain block and pool_signature
        // lives inside foliage_block_data, so corrupting one of those also breaks an INLINE gate that
        // runs before this drain (the URSB hash, or the block-data sig message) — that inline gate
        // rejects the block first. This is the identical property the VDF deferral already carries
        // ("only the failure short-circuit order changes"); it costs extra work solely on
        // already-invalid blocks and never confirms a bad one.
        let mut sig_err: Option<SyncError> = None;
        if !sig_queue.is_empty() {
            let span =
                tracing::info_span!("window.sig", sigs = sig_queue.len(), blocks = staged.len());
            span.in_scope(|| {
                let sig_started = std::time::Instant::now();
                if !self.engine.verify_sig_window(&sig_queue) {
                    let mut start = 0usize;
                    let mut sig_confirm_upto = 0usize;
                    for (i, (delta, _, hi)) in staged.iter().enumerate() {
                        if let Some(tag) = crate::header::first_failing_sig(&sig_queue[start..*hi])
                        {
                            sig_confirm_upto = i;
                            sig_err = Some(
                                crate::error::NodeError::Invalid(format!(
                                    "{} at height {} (window drain)",
                                    tag.rejection(),
                                    delta.height
                                ))
                                .into(),
                            );
                            break;
                        }
                        start = *hi;
                    }
                    if sig_err.is_none() {
                        sig_err = Some(
                            crate::error::NodeError::Invalid(
                                "INVALID_HEADER_SIGNATURE in window drain (unattributed)"
                                    .to_string(),
                            )
                            .into(),
                        );
                    }
                    // A sig failure lowers the confirm boundary only if it is EARLIER than the
                    // VDF-determined one; the earlier fault is the one the sequential path reports.
                    if sig_confirm_upto < confirm_upto {
                        confirm_upto = sig_confirm_upto;
                        vdf_err = sig_err.take();
                    }
                }
                self.metrics
                    .window_sig_micros
                    .store(sig_started.elapsed().as_micros() as u64, Ordering::Relaxed);
            });
        }

        // One store batch confirms the whole window; the
        // engine falls back to per-block fork choice the moment a delta isn't a plain
        // extension, so semantics match the sequential path exactly.
        let to_confirm: Vec<BlockDelta> = staged.drain(..confirm_upto).map(|(d, _, _)| d).collect();
        let reported: Vec<BlockDelta> = to_confirm.clone();
        let mut deltas = Vec::new();
        // A stale reorg report from a non-reporting confirm path (fast-sync's per-block adds)
        // must never mis-attach to THIS window's outcomes: only reports pushed by the batch
        // below are consumed by the expansion after it.
        self.engine.clear_reorg_reports();
        // Await-safe instrumentation: `confirm_staged_batch` is async, so attach the span to the
        // future — the previous `{ let _e = span.enter(); ...await }` held an Entered guard across
        // the `.await`, the exact sharded-registry clone-after-close hazard.
        let span = tracing::info_span!("window.confirm", blocks = to_confirm.len());
        let confirm_started = std::time::Instant::now();
        let outcomes = self
            .engine
            .confirm_staged_batch(to_confirm)
            .instrument(span)
            .await?;
        self.metrics.window_confirm_micros.store(
            confirm_started.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        for (outcome, delta) in outcomes.iter().zip(&reported) {
            if let AddBlockOutcome::Reorg { fork_height, .. } = outcome {
                self.metrics.last_reorg_depth.store(
                    u64::from(delta.height.saturating_sub(*fork_height)),
                    Ordering::Relaxed,
                );
            }
            match outcome {
                AddBlockOutcome::AlreadyHave | AddBlockOutcome::Orphan { .. } => {}
                _ => {
                    self.metrics
                        .blocks_confirmed
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        expand_confirmed(
            outcomes,
            reported,
            || self.engine.pop_reorg_report(),
            &mut deltas,
        );
        if let Some(e) = vdf_err.or(stage_err) {
            // Unconfirmed staged blocks retry next tick and re-stage; their overlay entries
            // must not linger meanwhile.
            self.engine.clear_staged_overlay();
            return Err(e);
        }
        Ok((self.engine.store().get_peak().await?, deltas))
    }
}

/// Per-height epoch parameters derived from the weight proof's summary chain — the attested schedule
/// of every `(sub_slot_iters, difficulty)` retarget. The summary for sub-epoch `k-1` is included at
/// the first block of sub-epoch `k` (chia indexes `wp_summaries[height / SUB_EPOCH_BLOCKS - 1]`), and
/// values it declares activate at that inclusion — so for a block at height `h`, the applicable values
/// are the LAST ones declared among `summaries[..h / SUB_EPOCH_BLOCKS]`. A single-pair anchor is only
/// correct for a window that provably crosses no epoch boundary; any span that can cross one (the
/// epoch-depth backfill always does, and a recent chain may) must resolve per height.
pub struct EpochSchedule {
    // (activation sub-epoch index, ssi declared there) / (…, difficulty …), ascending.
    ssi_changes: Vec<(u32, u64)>,
    difficulty_changes: Vec<(u32, u64)>,
    sub_epoch_blocks: u32,
    ssi_start: u64,
    difficulty_start: u64,
}

impl EpochSchedule {
    #[must_use]
    pub fn from_summaries(
        summaries: &[SubEpochSummary],
        sub_epoch_blocks: u32,
        ssi_start: u64,
        difficulty_start: u64,
    ) -> Self {
        let mut ssi_changes = Vec::new();
        let mut difficulty_changes = Vec::new();
        for (i, s) in summaries.iter().enumerate() {
            // Summary i is included at the start of sub-epoch i+1; its declared values apply there.
            let activation = u32::try_from(i + 1).unwrap_or(u32::MAX);
            if let Some(v) = s.new_sub_slot_iters {
                ssi_changes.push((activation, v));
            }
            if let Some(v) = s.new_difficulty {
                difficulty_changes.push((activation, v));
            }
        }
        Self {
            ssi_changes,
            difficulty_changes,
            sub_epoch_blocks,
            ssi_start,
            difficulty_start,
        }
    }

    /// The `(sub_slot_iters, difficulty)` in force at `height`.
    #[must_use]
    pub fn at(&self, height: u32) -> (u64, u64) {
        let sub_epoch = height / self.sub_epoch_blocks;
        let ssi = self
            .ssi_changes
            .iter()
            .rev()
            .find_map(|&(a, v)| (a <= sub_epoch).then_some(v))
            .unwrap_or(self.ssi_start);
        let difficulty = self
            .difficulty_changes
            .iter()
            .rev()
            .find_map(|&(a, v)| (a <= sub_epoch).then_some(v))
            .unwrap_or(self.difficulty_start);
        (ssi, difficulty)
    }
}

// The tip epoch's `(sub_slot_iters, difficulty)`: the last summary that declares each (a sub-epoch boundary
// carries the new value), falling back to the genesis constants when the proof declares none. Superseded in
// production by [`EpochSchedule`] (per-height resolution); kept as the tests' independent oracle — the
// schedule's value AT THE TIP must equal this anchor (the headers-first invariant the unit test pins).
#[cfg(test)]
fn tip_epoch_from(
    summaries: &[SubEpochSummary],
    ssi_start: u64,
    difficulty_start: u64,
) -> (u64, u64) {
    let ssi = summaries
        .iter()
        .rev()
        .find_map(|s| s.new_sub_slot_iters)
        .unwrap_or(ssi_start);
    let difficulty = summaries
        .iter()
        .rev()
        .find_map(|s| s.new_difficulty)
        .unwrap_or(difficulty_start);
    (ssi, difficulty)
}

// One download slot bound to one source (= one outbound peer). Pulls contiguous reservations from the shared
// window, fetches the bodies, and writes them through to the store out of order. On a stall it reclaims its
// reservation to the pool and drops out — a live peer picks the run up; eviction never revives a dead channel.
// A worker tolerates this many consecutive fetch misses (timeout or reject) on its peer before it gives up
// that peer — one transient reject/timeout must not permanently remove a peer for the whole sync (that
// fragility turned any single body-download hiccup into a full `Exhausted` restart of the from-zero sync).
const MAX_PEER_FETCH_FAILURES: u32 = 5;
// Short pause after a miss so a hard-rejecting peer cannot hot-loop reclaim→re-reserve→reject.
const FETCH_FAILURE_BACKOFF: Duration = Duration::from_millis(250);

/// Validate a downloaded block batch against the headers-first candidate chain BEFORE it is written
/// through — hardening. A `RespondBlocks` is untrusted: a lying peer can re-stamp a body or answer
/// with a different height range. chia validates the returned blocks are the requested heights,
/// contiguous, and connect (`prev_header_hash` chains to the fork/anchor) and bans the peer on any
/// mismatch (`full_node.py` short_sync_batch first-block connect :645-651 — `height_to_hash(start-1)` for
/// start>0, `GENESIS_CHALLENGE` for the from-genesis batch :663-667 — and add_prevalidated_blocks →
/// `peer.close(CONSENSUS_ERROR_BAN_SECONDS)` on any batch error :1436). We mirror the checks against our
/// architecture: the headers-first pass ([`Chaser::sync_headers`] → [`headers::sync_header_chain`]) has
/// already stored the WP-attested candidate record for every reserved height, and that candidate
/// chain was validated to be contiguous and anchor-connected. So the batch is sound iff
/// 1. it covers EXACTLY the reserved `start..=end`, ascending and contiguous, and
/// 2. every body's `header_hash` binds to a committed candidate record AT its own height — the
///    WP-attested header, never blind trust.
///
/// A re-stamped / out-of-range / wrong-anchor body hashes to no candidate (or to one at a different
/// height) and is rejected here, before it can reach `append_many` (where an orphan body would trip the
/// store's `block_body → block_record` foreign key as a FATAL error that aborts the whole sync — a
/// one-peer DoS), the confirm pipeline, or the from-empty `prev = None` entry. The connect check for the
/// from-empty/anchor path reuses the anchor/WP linkage: the anchor's candidate record IS the
/// WP-attested header, so matching a body against it inherits the proof's attestation without a prior
/// in-store parent.
///
/// Returns `Ok(Ok(()))` when the batch is sound, `Ok(Err(reason))` when it is rejected (non-fatal — the
/// caller reclaims the reservation like any miss), and `Err(_)` only on a genuine store fault (propagated).
async fn validate_downloaded_batch<S>(
    store: &S,
    blocks: &[FullBlock],
    start: u32,
    end: u32,
) -> Result<Result<(), SyncError>, SyncError>
where
    S: BlockStore + Sync,
{
    // (1) Exact coverage: count + ascending-contiguous heights over the reserved range. The
    // OutboundPeerSource repeats this on the wire, but a batch reaches append_many only through the
    // worker, so the gate must live here to cover EVERY source (in-memory tests, replay fixtures).
    let expected = usize::try_from(u64::from(end) - u64::from(start) + 1).unwrap_or(usize::MAX);
    let covers = blocks.len() == expected
        && blocks
            .iter()
            .zip(start..=end)
            .all(|(b, want)| b.height() == want);
    if !covers {
        return Ok(Err(SyncError::RangeMismatch {
            start,
            end,
            got: blocks.len(),
        }));
    }
    // (2) Each body must bind to a committed candidate header AT its height — the connect check. A body
    // that will not serialize its own foliage, hashes to no candidate, or matches a candidate at a
    // different height is a re-stamp / forgery: reject the whole batch (non-fatal, treated as a miss).
    for b in blocks {
        let Ok(hh) = b.header_hash() else {
            return Ok(Err(SyncError::BatchUnlinked {
                start,
                end,
                height: b.height(),
            }));
        };
        match store.get_block_record(&hh).await? {
            Some(rec) if rec.height == b.height() => {}
            _ => {
                return Ok(Err(SyncError::BatchUnlinked {
                    start,
                    end,
                    height: b.height(),
                }));
            }
        }
    }
    Ok(Ok(()))
}

async fn download_worker<S>(
    store: S,
    window: Arc<Mutex<ReservationWindow>>,
    bodies: OrderedBodies,
    src: Arc<dyn BlockRangeSource>,
    cfg: SyncConfig,
    metrics: Arc<SyncMetrics>,
) -> Result<(), SyncError>
where
    S: BlockStore + Send + Sync + 'static,
{
    let mut failures: u32 = 0;
    loop {
        // Discover pending heights before touching the lock (no await under the window mutex). Written bodies
        // drop out of get_unassociated, so this is also the drain signal.
        let pending = store.get_unassociated(cfg.window).await?;
        let (claim, live) = {
            let mut w = window.lock().await;
            w.refill(pending);
            (w.reserve(cfg.batch), w.live())
        };
        metrics.peak_window.fetch_max(live, Ordering::Relaxed);
        let reservation = match claim {
            Claim::Reserved(r) => r,
            Claim::Drained => return Ok(()),
            Claim::Busy => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                continue;
            }
        };

        let (start, end) = (reservation.start(), reservation.end());
        let span = tracing::info_span!(
            "reservation",
            peer = src.peer_id(),
            window = live,
            start,
            end
        );
        // Await-safe instrumentation: the fetch + write-through below all `.await`, so the
        // `reservation` span is attached to that per-reservation future rather than held as an
        // Entered guard across the awaits. The loop's control flow (`continue` on a good fetch,
        // `return Ok(())` when the peer is given up) is carried out through a `ControlFlow` value
        // so `?` still propagates store errors out of the block.
        let flow = async {
            let outcome =
                tokio::time::timeout(cfg.request_timeout, src.fetch_range(start, end)).await;
            // Reason string so the live log names WHY a peer could not serve a recent range (timeout vs. reject
            // vs. decode) — the measurement the "no peer can serve height" stall needs to be diagnosed, not
            // guessed. `Ok(Ok(_))` is success; the two failure shapes are a hard timeout or a returned error.
            let reason: std::borrow::Cow<'static, str> = match outcome {
                Ok(Ok(blocks)) => {
                    metrics
                        .peak_inflight_blocks
                        .fetch_max(blocks.len(), Ordering::Relaxed);
                    // HARDENING: the RespondBlocks body is untrusted until it matches the
                    // headers-first candidate chain. Validate the batch BEFORE the write-through; a
                    // re-stamped / out-of-range / wrong-anchor batch is reclaimed like any miss and never
                    // reaches append_many (nor the store's block_body→block_record FK as a fatal error),
                    // the confirm pipeline, or the from-empty prev=None entry.
                    match validate_downloaded_batch(&store, &blocks, start, end).await? {
                        Ok(()) => {
                            let mut batch = store.begin().await?;
                            store.append_many(&mut batch, &blocks).await?;
                            store.commit(batch).await?;
                            metrics
                                .blocks_downloaded
                                .fetch_add(blocks.len() as u64, Ordering::Relaxed);
                            {
                                let mut b = bodies.lock().await;
                                for block in &blocks {
                                    b.insert(block.height(), block.header_hash()?);
                                }
                            }
                            window.lock().await.complete(reservation.id);
                            failures = 0; // a good fetch clears the peer's miss budget
                            return Ok::<_, SyncError>(std::ops::ControlFlow::Continue(()));
                        }
                        // Rejected: fall through to the miss path (reclaim + count against the budget +
                        // retire the peer). The named reason surfaces in the live warn log.
                        Err(reject) => std::borrow::Cow::Owned(reject.to_string()),
                    }
                }
                Ok(Err(e)) => std::borrow::Cow::Owned(e.to_string()),
                Err(_) => std::borrow::Cow::Borrowed("request timed out"),
            };
            // A miss: reclaim the run for another peer and count it against this peer's budget. Only after
            // MAX_PEER_FETCH_FAILURES consecutive misses (or a closed channel) do we give the peer up — a single
            // hiccup no longer nukes it for the whole sync.
            window.lock().await.reclaim(reservation.id);
            metrics.reclaimed.fetch_add(1, Ordering::Relaxed);
            failures += 1;
            tracing::warn!(
                peer = src.peer_id(),
                start,
                end,
                failures,
                reason = %reason,
                "block-range fetch failed; reservation reclaimed"
            );
            if src.is_closed() || failures >= MAX_PEER_FETCH_FAILURES {
                return Ok(std::ops::ControlFlow::Break(()));
            }
            tokio::time::sleep(FETCH_FAILURE_BACKOFF).await;
            Ok(std::ops::ControlFlow::Continue(()))
        }
        .instrument(span)
        .await?;
        match flow {
            std::ops::ControlFlow::Continue(()) => continue,
            std::ops::ControlFlow::Break(()) => return Ok(()),
        }
    }
}

/// Take the staged window's queued VDF proofs AND header signatures out of the sink, refusing a
/// poisoned sink. Poison means a staging thread panicked mid-window; treating that as "nothing
/// queued" would confirm the whole window unverified. No consensus input may silently degrade to a
/// default (the difficulty-0 lesson). Both queues are drained atomically: a poison on EITHER fails
/// the window closed.
///
/// # Errors
/// [`SyncError`] (invalid) when either sink mutex is poisoned.
pub fn drain_header_sink(
    sink: crate::header::HeaderSink,
) -> Result<(Vec<crate::header::QueuedVdf>, Vec<crate::header::QueuedSig>), SyncError> {
    let poisoned = || -> SyncError {
        crate::error::NodeError::Invalid(
            "window sink poisoned by a panicked staging thread; window fails closed instead of \
             confirming unverified"
                .to_string(),
        )
        .into()
    };
    let vdf = sink.vdf.into_inner().map_err(|_| poisoned())?;
    let sig = sink.sig.into_inner().map_err(|_| poisoned())?;
    Ok((vdf, sig))
}

#[cfg(test)]
mod tests {
    use super::tip_epoch_from;
    use dg_xch_core::blockchain::sized_bytes::Bytes32;
    use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;

    // Red-first: the epoch anchor must take the LAST declared
    // (ssi, difficulty), not the first, and must fall back to the genesis constants when the proof
    // declares none. A wrong anchor is the headers-first required_iters==0 poison.
    fn ses(new_difficulty: Option<u64>, new_sub_slot_iters: Option<u64>) -> SubEpochSummary {
        SubEpochSummary {
            prev_subepoch_summary_hash: Bytes32::default(),
            reward_chain_hash: Bytes32::default(),
            num_blocks_overflow: 0,
            new_difficulty,
            new_sub_slot_iters,
        }
    }

    #[test]
    fn empty_summaries_fall_back_to_genesis_constants() {
        assert_eq!(tip_epoch_from(&[], 128, 7), (128, 7));
    }

    #[test]
    fn tip_epoch_takes_the_last_declared_values() {
        let s = [
            ses(Some(7), Some(128)),
            ses(Some(9), None),
            ses(None, Some(1024)),
        ];
        // last new_difficulty is 9 (third has None), last new_sub_slot_iters is 1024
        assert_eq!(tip_epoch_from(&s, 64, 3), (1024, 9));
    }

    #[test]
    fn undeclared_fields_hold_the_starting_value() {
        let s = [ses(None, None), ses(None, None)];
        assert_eq!(tip_epoch_from(&s, 64, 3), (64, 3));
    }

    // The pending-boundary depth math — pinned to the live mainnet wall (epoch_blocks = 4608,
    // sub_epoch_blocks = 384, boundary 4,575,744 = 993 * 4608, previous surpass 4,571,136):
    // every position that can still trigger the 4,575,744 retarget must demand records down to
    // 4,571,136 - EPOCH_BACKFILL_SLACK = 4,571,008.
    #[test]
    fn epoch_backfill_low_covers_the_pending_boundary_retarget() {
        use super::epoch_backfill_low;
        let (e, s) = (4608u32, 384u32);
        // Mid-epoch anchor base (a sync leg's --sync-from=4575000 span base H-64): the next
        // boundary IS the pending boundary; old and new formulas agree.
        assert_eq!(epoch_backfill_low(4_574_936, e, s), 4_571_008);
        // Peak just past the boundary, retarget trigger still ahead (the frozen pod: peak
        // 4,575,757, trigger 4,575,758). The old `(at / e + 1) * e` rounding targeted 4,580,352
        // and demanded only 4,575,616 — one full epoch short; the resume repair concluded
        // "nothing to backfill" on every restart.
        assert_eq!(epoch_backfill_low(4_575_757, e, s), 4_571_008);
        // The boundary block itself and the last height inside the trigger window.
        assert_eq!(epoch_backfill_low(4_575_744, e, s), 4_571_008);
        assert_eq!(epoch_backfill_low(4_576_127, e, s), 4_571_008);
        // Past the trigger window: the 4,575,744 retarget must have fired; only the NEXT
        // boundary (4,580,352) remains pending, whose surpass depth is 4,575,616.
        assert_eq!(epoch_backfill_low(4_576_128, e, s), 4_575_616);
        // Genesis-side saturation: never underflows.
        assert_eq!(epoch_backfill_low(0, e, s), 0);
        assert_eq!(epoch_backfill_low(383, e, s), 0);
    }

    // The recovery signal: a consensus-walk NotFound ("block record not found") inside the stage
    // path classifies as a missing-record failure; validation rejections and peer failures do not.
    #[test]
    fn is_missing_record_matches_only_the_notfound_walk_error() {
        use super::SyncError;
        use crate::error::NodeError;
        let missing = SyncError::Node(NodeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "block record not found: 0xdead",
        )));
        assert!(missing.is_missing_record());
        assert!(!missing.is_orphan());
        let invalid = SyncError::Node(NodeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "INVALID_VDF",
        )));
        assert!(!invalid.is_missing_record());
        let orphan = SyncError::Node(NodeError::Orphan("h".into()));
        assert!(!orphan.is_missing_record());
        let io = SyncError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "socket"));
        assert!(!io.is_missing_record());
    }

    // The per-height schedule: summary i activates at sub-epoch i+1, values hold until the next
    // declaration, and the schedule at the TIP equals the tip_epoch_from anchor (the headers-first invariant).
    #[test]
    fn epoch_schedule_resolves_per_height_and_matches_the_tip_anchor() {
        use super::EpochSchedule;
        let sub_epoch_blocks = 64u32;
        // sub-epoch 0 summary declares (d=9, ssi=1024) -> active from sub-epoch 1;
        // sub-epoch 2 summary declares (d=11, ssi=2048) -> active from sub-epoch 3.
        let s = [
            ses(Some(9), Some(1024)),
            ses(None, None),
            ses(Some(11), Some(2048)),
            ses(None, None),
        ];
        let sched = EpochSchedule::from_summaries(&s, sub_epoch_blocks, 128, 7);
        assert_eq!(
            sched.at(0),
            (128, 7),
            "before any activation: starting values"
        );
        assert_eq!(sched.at(63), (128, 7), "last block of sub-epoch 0");
        assert_eq!(
            sched.at(64),
            (1024, 9),
            "first block of sub-epoch 1: summary 0 active"
        );
        assert_eq!(sched.at(191), (1024, 9), "held through sub-epoch 2");
        assert_eq!(sched.at(192), (2048, 11), "sub-epoch 3: summary 2 active");
        assert_eq!(
            sched.at(64 * 10),
            tip_epoch_from(&s, 128, 7),
            "at the tip the schedule equals the tip anchor"
        );
    }
}
