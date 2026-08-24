mod block;
mod coin;

use crate::error::StoreError;
use crate::telemetry::StoreTelemetry;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{ConnectOptions, Row, SqliteConnection, SqlitePool};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

/// A sqlx-SQLite backend for the coin + block stores. One single writer connection (sqlite is
/// single-writer) behind an async mutex; a separate WAL read pool so point reads stay lock-free while a
/// write batch is open; and a dedicated checkpointer connection that drains the WAL off the writer.
pub struct SqliteStore {
    read: SqlitePool,
    writer: Arc<Mutex<SqliteConnection>>,
    // Phase signal read by the checkpointer + the confirm loop: true near the tip (per-block commits +
    // active checkpointer), false during bulk catch-up (big batch commits + quiet checkpointer).
    near_tip: Arc<AtomicBool>,
    // Background WAL checkpointer. Aborted on drop; see `spawn_checkpointer`.
    checkpointer: tokio::task::JoinHandle<()>,
    // Read-only telemetry: commit latency by phase + checkpoint activity, recorded as a side
    // effect of work this store already does, rendered by the node's /metrics responder.
    pub(crate) telemetry: Arc<StoreTelemetry>,
    // `<db>-wal`, for the wal_bytes() file-size gauge (the SQLite WAL always lives at this suffix).
    wal_path: PathBuf,
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        self.checkpointer.abort();
    }
}

impl SqliteStore {
    /// Open (creating if missing) a WAL-mode store at `path` and apply the migrations for the enabled
    /// feature tier.
    ///
    /// # Errors
    /// Returns [`StoreError::Backend`] if the database cannot be opened or migrated.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .pragma("mmap_size", "268435456")
            // Page cache. The SQLite default (-2000 = 2 MiB) forces near-constant cache-spill to the
            // WAL when a confirm writes thousands of random `coin_name` (hash-keyed, WITHOUT ROWID)
            // rows into the multi-GB coin_record b-tree. 64 MiB holds a single block's dirty working
            // set so a per-block confirm spills nothing, and sharply cuts spill for a multi-block batch.
            .pragma("cache_size", "-65536")
            // Keep the writer-COMMIT autocheckpoint OUT of normal operation. The store runs on network
            // block storage (iSCSI, measured ~100 ms/fsync). An autocheckpoint firing inside a confirm
            // COMMIT copies the whole accumulated WAL into the DB file and fsyncs it on the hot path — a
            // multi-second-to-minute writer stall that freezes the confirmed peak and trips the
            // liveness probe. The dedicated checkpointer (below) does that copy+fsync off the writer.
            // The threshold is a disk-fill failsafe, not the mechanism: 262 144 pages ≈ 1 GiB is far
            // above the largest single confirm window's WAL (~240 MiB), so it never fires while the
            // checkpointer is alive; it only bounds the WAL if that background task ever stops.
            .pragma("wal_autocheckpoint", "262144");
        let mut writer = opts.clone().connect().await?;
        migrate(&mut writer).await?;
        let read = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts.clone().read_only(true))
            .await?;
        let near_tip = Arc::new(AtomicBool::new(false));
        let telemetry = StoreTelemetry::new();
        let checkpointer = spawn_checkpointer(
            opts.connect().await?,
            near_tip.clone(),
            telemetry.clone(),
            read.clone(),
        );
        // SQLite's WAL file always lives at `<db>-wal` (same directory, suffix appended).
        let mut wal_os = path.as_os_str().to_os_string();
        wal_os.push("-wal");
        Ok(Self {
            read,
            writer: Arc::new(Mutex::new(writer)),
            near_tip,
            checkpointer,
            telemetry,
            wal_path: PathBuf::from(wal_os),
        })
    }

    /// Current size in bytes of the `-wal` file (0 when it does not exist yet). A metadata stat —
    /// cheap enough for every scrape; the same number otherwise watched by hand by listing the
    /// `-wal` file size while diagnosing WAL growth.
    #[must_use]
    pub fn wal_file_bytes(&self) -> u64 {
        std::fs::metadata(&self.wal_path).map_or(0, |m| m.len())
    }
}

/// Drive WAL checkpoints on a dedicated connection so their copy-into-DB + fsync latency never blocks
/// the confirm writer.
///
/// `wal_checkpoint(PASSIVE)` copies every checkpointable frame into the DB file and, when it reaches
/// them all with no reader still reading the WAL, resets the write pointer to the front so the file is
/// reused instead of growing without bound. PASSIVE is the only mode that takes no exclusive lock: it
/// never blocks a concurrent writer and never hands another connection `SQLITE_BUSY` ("database is
/// locked") — TRUNCATE/RESTART do, which deadlocks the single writer. While a confirm holds an open
/// transaction PASSIVE simply does what it can and resets after the COMMIT lands. The copy+fsync runs
/// here, off the writer, so a slow-fsync backend (iSCSI ~100 ms/sync) never stalls the confirmed peak.
///
/// PASSIVE does not shrink the WAL *file*: it stays at the high-water of the largest single
/// transaction (one batch-confirm window today; a few MB once confirms commit per block). That is a
/// bounded, reused file — harmless for reads now that the coin multi-get point-gets rather than scans.
fn spawn_checkpointer(
    mut conn: SqliteConnection,
    near_tip: Arc<AtomicBool>,
    telemetry: Arc<StoreTelemetry>,
    read: SqlitePool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The pragma's `checkpointed` column is a POSITION in the current WAL (total frames moved so
        // far), not a per-run delta — the counter must add only the advance since the last pass, and
        // treat a smaller value as a WAL reset (write pointer back at the front).
        let mut prev_checkpointed: i64 = 0;
        // Bulk-phase drain cadence: near the tip the checkpointer drains every tick (1 s); during bulk
        // catch-up it drains far less often so the confirm writer keeps most of the iSCSI write budget,
        // but it MUST still drain. A fully-quiet bulk checkpointer let the WAL grow to the in-writer
        // wal_autocheckpoint failsafe (262144 pages ≈ 1 GB) — whose blocking copy-to-DB stalls the
        // confirmed peak for minutes on slow storage (peak freeze → liveness restart → repeat), because
        // a from-genesis sync is millions of blocks from tip and never reaches the "drain at tip" path.
        // At ~1 MB/s WAL growth a 20 s cadence bounds the file to tens of MB, orders of magnitude under
        // the failsafe, and each off-writer PASSIVE pass is cheap.
        const BULK_CHECKPOINT_TICKS: u32 = 20;
        let mut bulk_tick: u32 = 0;
        loop {
            tick.tick().await;
            // Read-pool census every tick (cheap atomics): the WAL-pin witness. A high idle count while
            // `wal_frames` refuses to fall means a pooled reader holds an old WAL read mark, blocking the
            // checkpoint reset — names the reader that pins the WAL.
            telemetry
                .read_pool_idle
                .store(read.num_idle() as u64, Ordering::Relaxed);
            telemetry
                .read_pool_size
                .store(u64::from(read.size()), Ordering::Relaxed);
            // Best-effort: a busy/failed checkpoint is retried on the next tick.
            // Phase-aware cadence: near the tip drain every tick; during bulk drain on the slow cadence
            // above — enough to bound the WAL below the failsafe while leaving the write budget to catch-up.
            if !near_tip.load(Ordering::Relaxed) {
                bulk_tick = bulk_tick.wrapping_add(1);
                if !bulk_tick.is_multiple_of(BULK_CHECKPOINT_TICKS) {
                    continue;
                }
            } else {
                bulk_tick = 0;
            }
            // Telemetry hook: the SAME pragma as before, but its result row — (busy, log,
            // checkpointed) per SQLite's wal_checkpoint docs — is captured instead of discarded, and
            // the call is timed. This is the "is the checkpointer keeping up" instrument: duration
            // histogram, frames drained, WAL length in frames, busy passes, outright errors.
            let started = std::time::Instant::now();
            let result = sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .fetch_one(&mut conn)
                .await;
            match result {
                Ok(row) => {
                    telemetry.checkpoint.record(started.elapsed().as_secs_f64());
                    let busy: i64 = row.try_get(0).unwrap_or(0);
                    let log: i64 = row.try_get(1).unwrap_or(-1);
                    let checkpointed: i64 = row.try_get(2).unwrap_or(-1);
                    if busy != 0 {
                        telemetry
                            .checkpoint_busy_total
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if log >= 0 {
                        #[allow(clippy::cast_sign_loss)]
                        telemetry.wal_frames.store(log as u64, Ordering::Relaxed);
                    }
                    if checkpointed >= 0 {
                        let advance = if checkpointed >= prev_checkpointed {
                            checkpointed - prev_checkpointed
                        } else {
                            checkpointed // WAL reset: the new position IS this pass's work
                        };
                        prev_checkpointed = checkpointed;
                        #[allow(clippy::cast_sign_loss)]
                        telemetry
                            .wal_frames_checkpointed_total
                            .fetch_add(advance as u64, Ordering::Relaxed);
                    }
                }
                Err(_) => {
                    telemetry
                        .checkpoint_errors_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })
}

async fn migrate(conn: &mut SqliteConnection) -> Result<(), StoreError> {
    sqlx::raw_sql(include_str!("../../migrations/sqlite/0001_coin_record.sql"))
        .execute(&mut *conn)
        .await?;
    sqlx::raw_sql(include_str!("../../migrations/sqlite/0002_block.sql"))
        .execute(&mut *conn)
        .await?;
    // 0003 (service indexes) and 0006 (reorg indexes) are deferred to `build_indexes` at the
    // sync->tip transition — see the postgres migrate note and the coin-record index-cost
    // hardware report: secondary coin_record indexes are pure write-amplification during sync.
    #[cfg(feature = "hint")]
    sqlx::raw_sql(include_str!("../../migrations/sqlite/0004_hint.sql"))
        .execute(&mut *conn)
        .await?;
    sqlx::raw_sql(include_str!(
        "../../migrations/sqlite/0005_sub_epoch_segments.sql"
    ))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(crate) fn amount_be(amount: u64) -> Vec<u8> {
    amount.to_be_bytes().to_vec()
}

pub(crate) fn amount_from_be(bytes: &[u8]) -> Result<u64, StoreError> {
    let arr: [u8; 8] = bytes.try_into().map_err(|_| {
        StoreError::Corrupt(format!("amount blob is {} bytes, want 8", bytes.len()))
    })?;
    Ok(u64::from_be_bytes(arr))
}

fn row_to_coin_record(row: &sqlx::sqlite::SqliteRow) -> Result<CoinRecord, StoreError> {
    let confirmed: i64 = row.try_get("confirmed_index")?;
    let spent: i64 = row.try_get("spent_index")?;
    let coinbase: i64 = row.try_get("coinbase")?;
    let timestamp: i64 = row.try_get("timestamp")?;
    let puzzle_hash: Bytes32 = row.try_get("puzzle_hash")?;
    let coin_parent: Bytes32 = row.try_get("coin_parent")?;
    let amount: Vec<u8> = row.try_get("amount")?;
    let spent_index = spent as u32;
    Ok(CoinRecord {
        coin: Coin {
            parent_coin_info: coin_parent,
            puzzle_hash,
            amount: amount_from_be(&amount)?,
        },
        confirmed_block_index: confirmed as u32,
        spent_block_index: spent_index,
        coinbase: coinbase != 0,
        timestamp: timestamp as u64,
        spent: spent_index != 0,
    })
}
