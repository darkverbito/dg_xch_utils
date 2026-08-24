//! Read-only store telemetry recorded by the backends and rendered by the node's `/metrics`
//! responder. Every instrument here exists to answer a diagnostic question that otherwise has to be
//! answered by hand during a live incident:
//!
//!   - "Did a store/confirm change regress commit latency in either band?" — the per-block-commit
//!     catch-up crawl was previously inferred from sqlx slow-statement log spam; the phase-labelled
//!     commit histogram answers it in one PromQL query.
//!   - "Is the WAL bounded and is the checkpointer keeping up?" — the WAL file can grow into the
//!     hundreds of MiB before a checkpoint catches up; the WAL gauges + checkpoint counters make that
//!     visible directly instead of by listing the `-wal` file size by hand.
//!
//! All plain atomics, recorded on paths that already did the work being measured (the COMMIT and the
//! `wal_checkpoint` pragma) — no store logic changes, zero cost beyond a clock read and a few
//! relaxed fetch_adds.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Histogram bucket upper bounds in seconds, shared by the commit and checkpoint histograms.
/// Spans a healthy local-fsync commit (~10 ms) through the iSCSI ~100 ms/fsync band up to the
/// multi-minute batch-commit / checkpoint stalls we have observed live; +Inf is implicit (`count`).
pub const DURATION_BUCKETS_SECS: [f64; 12] = [
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 15.0, 60.0, 120.0,
];

/// A Prometheus-shaped cumulative histogram over [`DURATION_BUCKETS_SECS`]: `buckets[i]` counts
/// observations `<= DURATION_BUCKETS_SECS[i]`, `count` is the +Inf bucket, `sum_micros` the total
/// observed time (micros so it stays an atomic integer; the renderer divides out to seconds).
#[derive(Default, Debug)]
pub struct DurationHistogram {
    pub buckets: [AtomicU64; DURATION_BUCKETS_SECS.len()],
    pub count: AtomicU64,
    pub sum_micros: AtomicU64,
}

impl DurationHistogram {
    /// Record one observation. Cumulative semantics: every bucket whose upper bound is `>= seconds`
    /// is incremented, plus the +Inf `count`.
    pub fn record(&self, seconds: f64) {
        for (i, ub) in DURATION_BUCKETS_SECS.iter().enumerate() {
            if seconds <= *ub {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        // Micros as u64: clamp a backwards clock to 0 rather than wrapping.
        let micros = (seconds * 1_000_000.0).max(0.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        self.sum_micros.fetch_add(micros as u64, Ordering::Relaxed);
    }

    /// Point-in-time copy for rendering (plain numbers, no atomics).
    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut buckets = [0u64; DURATION_BUCKETS_SECS.len()];
        for (b, a) in buckets.iter_mut().zip(self.buckets.iter()) {
            *b = a.load(Ordering::Relaxed);
        }
        HistogramSnapshot {
            buckets,
            count: self.count.load(Ordering::Relaxed),
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
        }
    }
}

/// Plain-number histogram state, `PartialEq`/`Default` so render tests stay pure.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HistogramSnapshot {
    pub buckets: [u64; DURATION_BUCKETS_SECS.len()],
    pub count: u64,
    pub sum_micros: u64,
}

/// Telemetry a store backend records as a side effect of work it already does. Held behind an `Arc`
/// shared between the backend and the `/metrics` sampler; see [`crate::BlockStore::telemetry`].
#[derive(Default, Debug)]
pub struct StoreTelemetry {
    /// Writer batch commits (body-append batches AND confirm transactions — every `COMMIT` on the
    /// single writer connection) while the store was in the CATCH-UP band (`near_tip = false`:
    /// one big transaction per sync window).
    pub commit_catch_up: DurationHistogram,
    /// Writer batch commits while in the NEAR-TIP band (`near_tip = true`: one transaction per
    /// confirmed block, active WAL checkpointer).
    pub commit_near_tip: DurationHistogram,
    /// Unix second of the last successful writer COMMIT (0 = none yet this process) — the
    /// "what was it last doing" witness the stall dump reads.
    pub last_commit_unix: AtomicU64,
    /// Completed `wal_checkpoint(PASSIVE)` pragmas on the dedicated checkpointer connection.
    pub checkpoint: DurationHistogram,
    /// WAL frames copied into the main DB by checkpoints (the pragma's `checkpointed` column).
    pub wal_frames_checkpointed_total: AtomicU64,
    /// WAL length in frames as of the last checkpoint (the pragma's `log` column) — the bounded-WAL
    /// witness in the checkpointer's own units.
    pub wal_frames: AtomicU64,
    /// Checkpoints that returned busy=1 (could not complete a full pass — a reader or the writer
    /// held the WAL). A persistently-busy checkpointer is one that is NOT keeping up.
    pub checkpoint_busy_total: AtomicU64,
    /// Checkpoint pragmas that failed outright (I/O or lock errors). Best-effort retries hide these
    /// from the logs; a climbing counter here means the WAL is not being drained at all.
    pub checkpoint_errors_total: AtomicU64,
    /// Block-record point reads executed on the read path (`get_block_record`,
    /// `get_block_record_by_height`, and each element of a multi-get) — the counted witness of
    /// the sync staging loop's per-block read serialization (the unmeasured catch-up residue).
    pub record_reads: AtomicU64,
    /// Coin-record point reads executed on the read path (`get_coin_record` and each element of
    /// `get_coin_records`) — the confirmed-set validation read volume, counted separately from
    /// the record reads so the residue attribution can split the two.
    pub coin_reads: AtomicU64,
}

impl StoreTelemetry {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[cfg(test)]
mod tests {
    use super::{DURATION_BUCKETS_SECS, DurationHistogram};
    use std::sync::atomic::Ordering;

    // Cumulative bucket semantics: an observation lands in ITS bucket and every wider one, and the
    // +Inf count includes observations past the last bound — the exact shape PromQL
    // histogram_quantile expects.
    #[test]
    fn record_is_cumulative() {
        let h = DurationHistogram::default();
        h.record(0.03); // > 0.025, <= 0.05
        h.record(0.03);
        h.record(500.0); // past the last bound: +Inf only
        let snap = h.snapshot();
        assert_eq!(snap.buckets[0], 0, "0.01 bucket must not see 0.03");
        assert_eq!(snap.buckets[1], 0, "0.025 bucket must not see 0.03");
        assert_eq!(snap.buckets[2], 2, "0.05 bucket sees both 0.03s");
        assert_eq!(
            snap.buckets[DURATION_BUCKETS_SECS.len() - 1],
            2,
            "last finite bucket must NOT include the 500s outlier"
        );
        assert_eq!(snap.count, 3, "+Inf sees all three");
        assert_eq!(snap.sum_micros, 30_000 + 30_000 + 500_000_000);
    }

    #[test]
    fn sum_never_underflows_on_negative_clock() {
        let h = DurationHistogram::default();
        h.record(-1.0); // a clock that stepped backwards must clamp, not wrap
        assert_eq!(h.sum_micros.load(Ordering::Relaxed), 0);
        assert_eq!(h.snapshot().count, 1);
    }
}
