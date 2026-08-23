//! The embedded memory-mapped backend — libbitcoin-database's store design (see DESIGN.md):
//! append-only data logs plus hash tables built as fixed bucket arrays over chained fixed-size
//! records, all accessed through read-write memory maps. The Pi profile: no SQL engine, no WAL,
//! sequential appends, chains instead of rehashing, and libbitcoin's crash ordering — record
//! bytes sync before the head link, so "data can be lost but the hashtable is never corrupted."
//!
//! Layout (all files under one directory):
//! - `records.dat` / `bodies.dat` / `coins.dat` — append-only logs, `[u32 LE len][bytes]`
//!   frames (bodies are zstd `FullBlock`s; records/coins are ChiaSerialize bytes).
//! - `blocks.tbl` — chained record table keyed by header hash; 22-byte payload
//!   `[record_off+1:8][body_off+1:8][height:4][status:1][main:1]`, updated in place.
//! - `coins.tbl` — chained record table keyed by coin id; 16-byte payload
//!   `[coin_off+1:8][spent_index:4][confirmed_index:4]`; spends update in place; reorg
//!   "deletes" zero the payload (the chain link stays — libbitcoin keeps structure immutable).
//! - `heights.dat` — dense main-chain index (32 bytes per height) through a read-write map.
//! - `meta.dat` — the confirmed peak, written LAST after log + table flushes.
//! - `reorg.journal` — crash-recovery intent for an in-flight reorg publish (T0-4). The batched
//!   reorg (fork sweep + branch re-applies + peak flip) has no transaction to abort, so its
//!   in-place table mutations are bracketed: the journal (fork height + the main-chain hash at
//!   the fork) is written durably BEFORE the first published mutation and removed AFTER the peak
//!   meta write. A journal found at open means a reorg publish was torn by a crash; the store
//!   CONVERGES by rolling coins, main-chain flags, heights, and the peak back to the fork —
//!   a consistent pre-reorg state from which the node re-syncs the heavier branch (bounded,
//!   already-archived work). Losing a completed-but-unacknowledged reorg to convergence is the
//!   deliberate conservative arm: data loss is resyncable, a peak over a reverted coin set is not.

mod block;
mod coin;
mod tables;

use crate::error::StoreError;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::traits::SizedBytes;
use memmap2::Mmap;
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tables::{ChainedTable, GrowableMap};
use tokio::sync::{Mutex, RwLock};

pub(crate) fn io_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(format!("mmap store io: {e}"))
}

pub(crate) const BLOCK_PAYLOAD: usize = 22;
pub(crate) const COIN_PAYLOAD: usize = 16;
// The reorg crash-recovery intent file (module note): [version:1][fork_height:4 LE]
// [has_hash:1][fork_hash:32].
const REORG_JOURNAL: &str = "reorg.journal";
// [segment_off+1:8] into segments.dat (0 = vacant, matching the other +1-offset payloads).
pub(crate) const SEGMENT_PAYLOAD: usize = 8;
// Bucket counts are fixed at creation (libbitcoin never rehashes; chains absorb load). Sized for
// mainnet scale: sparse header files cost only the pages actually touched.
const BLOCK_BUCKETS: u64 = 1 << 20;
const COIN_BUCKETS: u64 = 1 << 22;
// One entry per sub-epoch summary (384 blocks each) — ~26k at height 10M; 16k buckets keeps
// chains short at a few sparse pages.
const SEGMENT_BUCKETS: u64 = 1 << 14;

/// An append-only `[u32 len][bytes]` log: appends through a buffered `File`, reads through an
/// `Mmap` view refreshed whenever the file has grown past the mapping.
pub(crate) struct DataLog {
    path: PathBuf,
    file: Mutex<File>,
    map: RwLock<Option<Mmap>>,
    len: AtomicU64,
}

impl DataLog {
    fn open(path: PathBuf) -> Result<Self, StoreError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(io_err)?;
        let len = file.metadata().map_err(io_err)?.len();
        Ok(Self {
            path,
            file: Mutex::new(file),
            map: RwLock::new(None),
            len: AtomicU64::new(len),
        })
    }

    /// Append one frame; returns the frame's offset. Durability is deferred to [`DataLog::sync`].
    pub(crate) async fn append(&self, bytes: &[u8]) -> Result<u64, StoreError> {
        let mut f = self.file.lock().await;
        let off = self.len.load(Ordering::Acquire);
        let len = u32::try_from(bytes.len())
            .map_err(|_| StoreError::Corrupt("mmap store: frame over 4GiB".into()))?;
        f.write_all(&len.to_le_bytes()).map_err(io_err)?;
        f.write_all(bytes).map_err(io_err)?;
        self.len
            .store(off + 4 + bytes.len() as u64, Ordering::Release);
        Ok(off)
    }

    pub(crate) async fn sync(&self) -> Result<(), StoreError> {
        let f = self.file.lock().await;
        f.sync_data().map_err(io_err)
    }

    /// Read the frame at `off`. Refreshes the mapping if the frame lies past the current view.
    pub(crate) async fn read(&self, off: u64) -> Result<Vec<u8>, StoreError> {
        let need_remap = {
            let m = self.map.read().await;
            match m.as_ref() {
                Some(map) => (off + 4) as usize > map.len(),
                None => true,
            }
        };
        if need_remap {
            // Flush the appender so the kernel view covers everything appended so far.
            {
                let f = self.file.lock().await;
                f.sync_data().map_err(io_err)?;
            }
            let file = File::open(&self.path).map_err(io_err)?;
            let map = unsafe { Mmap::map(&file) }.map_err(io_err)?;
            *self.map.write().await = Some(map);
        }
        let m = self.map.read().await;
        let map = m
            .as_ref()
            .ok_or_else(|| StoreError::Corrupt("mmap store: unmapped log".into()))?;
        let start = off as usize;
        let hdr = map
            .get(start..start + 4)
            .ok_or_else(|| StoreError::Corrupt("mmap store: frame offset out of range".into()))?;
        let flen = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let body = map
            .get(start + 4..start + 4 + flen)
            .ok_or_else(|| StoreError::Corrupt("mmap store: torn frame".into()))?;
        Ok(body.to_vec())
    }
}

/// Dense height → main-chain header hash (32 bytes per height; all-zero = vacant), through a
/// read-write map.
pub(crate) struct HeightIndex {
    map: GrowableMap,
}

impl HeightIndex {
    fn open(path: PathBuf) -> Result<Self, StoreError> {
        Ok(Self {
            map: GrowableMap::open(path, 32 * 1024)?,
        })
    }

    pub(crate) fn get(&self, height: u32) -> Result<Option<Bytes32>, StoreError> {
        let off = u64::from(height) * 32;
        if off + 32 > self.map.mapped_len() {
            return Ok(None);
        }
        let mut buf = [0u8; 32];
        self.map.read_at(off, &mut buf)?;
        if buf == [0u8; 32] {
            Ok(None)
        } else {
            Ok(Some(Bytes32::new(buf)))
        }
    }

    pub(crate) fn set(&self, height: u32, hash: Option<&Bytes32>) -> Result<(), StoreError> {
        let off = u64::from(height) * 32;
        match hash {
            Some(h) => {
                let hb: &[u8] = h;
                self.map.write_at(off, hb)
            }
            None => self.map.write_at(off, &[0u8; 32]),
        }
    }

    // Number of height slots currently mapped (the scan bound for the min-height floor).
    pub(crate) fn slots(&self) -> u32 {
        u32::try_from(self.map.mapped_len() / 32).unwrap_or(u32::MAX)
    }

    pub(crate) fn sync(&self) -> Result<(), StoreError> {
        self.map.sync()
    }
}

/// The libbitcoin-style embedded store. One directory, opened exclusively by one process.
pub struct MmapStore {
    pub(crate) records: DataLog,
    pub(crate) bodies: DataLog,
    pub(crate) coins: DataLog,
    #[cfg(feature = "hint")]
    pub(crate) hints: DataLog,
    // Persisted weight-proof challenge segments (chia's sub_epoch_segments_v3,
    // block_store.py:85-88): opaque SubEpochSegments bytes in an append-only log, indexed by
    // ses block hash. Re-persist orphans the old frame (append-only; chia's INSERT OR REPLACE).
    pub(crate) segments: DataLog,
    pub(crate) blocks_tbl: ChainedTable<BLOCK_PAYLOAD>,
    pub(crate) coins_tbl: ChainedTable<COIN_PAYLOAD>,
    pub(crate) segments_tbl: ChainedTable<SEGMENT_PAYLOAD>,
    pub(crate) heights: HeightIndex,
    meta_path: PathBuf,
    // `reorg.journal` — the crash-recovery intent bracket around a reorg publish (module note).
    journal_path: PathBuf,
    // Whether the journal is on disk (armed by a published staged sweep, cleared after the peak
    // meta write) — saves an unlink syscall on every non-reorg commit.
    journal_armed: AtomicBool,
    pub(crate) peak: RwLock<Option<(Bytes32, u32)>>,
    // Heights holding a record but no body yet — the reservation-window feed. Rebuilt on open by
    // scanning the block table (bounded: bodies stream in close behind records in every sync path).
    pub(crate) unassociated: Mutex<BTreeSet<u32>>,
    // Serializes multi-file mutations (set_peak walks, coin batches) the way the SQLite backend's
    // single writer connection does.
    pub(crate) write_lock: Mutex<()>,
    // Cached sync floor for `min_record_height` (u64::MAX = unknown). The dense height index has
    // no cheap MIN, so the floor is scanned once, then revalidated with two point reads per call
    // (still occupied at the floor, still vacant just below it — a backfill below the old floor
    // fails the second check and forces a rescan).
    pub(crate) min_height_cache: std::sync::atomic::AtomicU64,
}

impl MmapStore {
    /// Open (or create) the store directory.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the directory or any store file cannot be opened.
    pub async fn open(dir: &Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(dir).map_err(io_err)?;
        let store = Self {
            records: DataLog::open(dir.join("records.dat"))?,
            bodies: DataLog::open(dir.join("bodies.dat"))?,
            coins: DataLog::open(dir.join("coins.dat"))?,
            #[cfg(feature = "hint")]
            hints: DataLog::open(dir.join("hints.dat"))?,
            segments: DataLog::open(dir.join("segments.dat"))?,
            blocks_tbl: ChainedTable::open(dir.join("blocks.tbl"), BLOCK_BUCKETS)?,
            coins_tbl: ChainedTable::open(dir.join("coins.tbl"), COIN_BUCKETS)?,
            segments_tbl: ChainedTable::open(dir.join("segments.tbl"), SEGMENT_BUCKETS)?,
            heights: HeightIndex::open(dir.join("heights.dat"))?,
            meta_path: dir.join("meta.dat"),
            journal_path: dir.join(REORG_JOURNAL),
            journal_armed: AtomicBool::new(false),
            peak: RwLock::new(None),
            unassociated: Mutex::new(BTreeSet::new()),
            write_lock: Mutex::new(()),
            min_height_cache: std::sync::atomic::AtomicU64::new(u64::MAX),
        };
        // Recover the peak pointer.
        if let Ok(bytes) = std::fs::read(&store.meta_path)
            && bytes.len() == 37
            && bytes[36] == 1
        {
            {
                let mut h = [0u8; 32];
                h.copy_from_slice(&bytes[..32]);
                let height = u32::from_le_bytes([bytes[32], bytes[33], bytes[34], bytes[35]]);
                *store.peak.write().await = Some((Bytes32::new(h), height));
            }
        }
        // A reorg journal on disk means a reorg publish was torn by a crash: converge back to the
        // fork BEFORE serving any read (module note; T0-4).
        if let Some((fork_height, fork_hash)) = store.read_reorg_journal()? {
            store.converge_torn_reorg(fork_height, fork_hash).await?;
        }
        // Rebuild the unassociated set: records without bodies.
        {
            let mut pending = BTreeSet::new();
            store.blocks_tbl.for_each(|_, payload| {
                let body_off = u64::from_le_bytes(payload[8..16].try_into().expect("payload"));
                if body_off == 0 {
                    let height = u32::from_le_bytes(payload[16..20].try_into().expect("payload"));
                    pending.insert(height);
                }
            })?;
            *store.unassociated.lock().await = pending;
        }
        Ok(store)
    }

    /// Publish a batch's staged coin mutations (see [`crate::types::MmapBatch`]): the staged
    /// reorg sweep (if any) bracketed by the reorg journal, then ONE coin-log fsync, then every
    /// staged payload image (in-place updates for existing keys, one batched
    /// [`ChainedTable::insert_batch`] for new ones), then ONE table msync so the links are
    /// durable BEFORE any caller publishes state that depends on them — `set_peak_in` drains
    /// ahead of its walk so the meta write (always last) can never land over unlinked coins.
    ///
    /// Ordering under a staged sweep (the reorg case): the journal lands durably FIRST, so any
    /// crash from the first in-place revert to the post-meta journal removal is detected at the
    /// next open and converged back to the fork (module note). Without a sweep the pre-T0-4
    /// argument is unchanged: a torn publish loses only idempotently re-appliable links under a
    /// peak that has not moved.
    pub(crate) async fn flush_coin_batch(
        &self,
        batch: &mut crate::types::BatchHandle,
    ) -> Result<(), StoreError> {
        let b = batch.mmap_batch()?;
        if b.pending_coins.is_empty() && b.sweep.is_none() {
            return Ok(());
        }
        let pending = std::mem::take(&mut b.pending_coins);
        b.pending_index.clear();
        let sweep = b.sweep.take();
        let _w = self.write_lock.lock().await;
        if let Some(s) = &sweep {
            // Durable intent BEFORE the first published mutation.
            self.write_reorg_journal(s.fork_height, s.fork_hash)?;
            for key in &s.deletions {
                if let Some(index) = self.coins_tbl.find(key)? {
                    let mut payload = self.coins_tbl.payload(index)?;
                    payload[..12].copy_from_slice(&[0u8; 12]); // off = vacant sentinel, spent = 0
                    self.coins_tbl.set_payload(index, &payload)?;
                }
            }
            for key in &s.unspends {
                if let Some(index) = self.coins_tbl.find(key)? {
                    let mut payload = self.coins_tbl.payload(index)?;
                    payload[8..12].copy_from_slice(&[0u8; 4]); // spent = 0
                    self.coins_tbl.set_payload(index, &payload)?;
                }
            }
        }
        // libbitcoin sync-before-link at the batch boundary: frames durable, then links, then
        // the links' own durability point. Staged images are absolute, so publishing them AFTER
        // the sweep's reverts yields the branch's final state for keys both touched.
        self.coins.sync().await?;
        let mut inserts: Vec<(Bytes32, [u8; COIN_PAYLOAD])> = Vec::new();
        for (key, image) in &pending {
            match self.coins_tbl.find(key)? {
                Some(index) => self.coins_tbl.set_payload(index, image)?,
                None => inserts.push((*key, *image)),
            }
        }
        if !inserts.is_empty() {
            self.coins_tbl.insert_batch(&inserts)?;
        }
        self.coins_tbl.sync()
    }

    /// Write the reorg journal durably (tmp + fsync + rename, the meta discipline plus the fsync
    /// the intent record requires — its CONTENT must be readable after a crash, not merely
    /// present-or-absent).
    fn write_reorg_journal(
        &self,
        fork_height: u32,
        fork_hash: Option<Bytes32>,
    ) -> Result<(), StoreError> {
        let mut buf = [0u8; 38];
        buf[0] = 1; // format version
        buf[1..5].copy_from_slice(&fork_height.to_le_bytes());
        if let Some(h) = fork_hash {
            buf[5] = 1;
            let hb: &[u8] = &h;
            buf[6..38].copy_from_slice(hb);
        }
        let tmp = self.journal_path.with_extension("tmp");
        {
            let mut f = File::create(&tmp).map_err(io_err)?;
            f.write_all(&buf).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        std::fs::rename(&tmp, &self.journal_path).map_err(io_err)?;
        self.journal_armed.store(true, Ordering::Release);
        Ok(())
    }

    /// Parse the reorg journal if present: `(fork_height, main-chain hash at the fork)`.
    fn read_reorg_journal(&self) -> Result<Option<(u32, Option<Bytes32>)>, StoreError> {
        let bytes = match std::fs::read(&self.journal_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(e)),
        };
        if bytes.len() != 38 || bytes[0] != 1 {
            return Err(StoreError::Corrupt(
                "mmap store: malformed reorg journal".to_string(),
            ));
        }
        let fork_height = u32::from_le_bytes(bytes[1..5].try_into().expect("journal layout"));
        let fork_hash = (bytes[5] == 1).then(|| {
            let mut h = [0u8; 32];
            h.copy_from_slice(&bytes[6..38]);
            Bytes32::new(h)
        });
        Ok(Some((fork_height, fork_hash)))
    }

    /// Remove the reorg journal — the reorg publish is complete past its peak meta write (or the
    /// open-time convergence finished). Idempotent.
    pub(crate) fn clear_reorg_journal(&self) -> Result<(), StoreError> {
        if !self.journal_armed.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        match std::fs::remove_file(&self.journal_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    /// Open-time convergence for a torn reorg publish (module note): roll coins, main-chain
    /// flags, heights, and the peak back to the fork the journal recorded, then drop the
    /// journal. Every step is idempotent — a crash DURING convergence leaves the journal in
    /// place and the next open redoes it.
    async fn converge_torn_reorg(
        &self,
        fork_height: u32,
        fork_hash: Option<Bytes32>,
    ) -> Result<(), StoreError> {
        use crate::traits::CoinStore;
        // 1. Coin sweep: delete additions above the fork (old-branch AND any partially-published
        //    new-branch coins alike — both carry confirmed_index > fork), un-spend removals.
        self.rollback_to(fork_height).await?;
        // 2. Retire every main-chain link above the fork, whichever branch it belongs to. The
        //    dense height index bounds the walk (vacant slots above the old top read as None).
        let _w = self.write_lock.lock().await;
        for h in fork_height.saturating_add(1)..self.heights.slots() {
            if let Some(hh) = self.heights.get(h)? {
                if let Some((index, mut entry)) = self.block_entry(&hh)? {
                    entry.main = false;
                    self.blocks_tbl.set_payload(index, &entry.pack())?;
                }
                self.heights.set(h, None)?;
            }
        }
        // 3. Peak = the fork (the pre-reorg common ancestor), durably, THEN drop the journal —
        //    the same peak-last ordering every mutation path follows.
        *self.peak.write().await = fork_hash.map(|h| (h, fork_height));
        self.blocks_tbl.sync()?;
        self.heights.sync()?;
        self.write_meta().await?;
        self.journal_armed.store(true, Ordering::Release);
        self.clear_reorg_journal()
    }

    /// Persist the peak pointer — always LAST in any mutation (the crash-consistency ordering).
    pub(crate) async fn write_meta(&self) -> Result<(), StoreError> {
        let peak = *self.peak.read().await;
        let mut buf = [0u8; 37];
        if let Some((h, height)) = peak {
            let hb: &[u8] = &h;
            buf[..32].copy_from_slice(hb);
            buf[32..36].copy_from_slice(&height.to_le_bytes());
            buf[36] = 1;
        }
        let tmp = self.meta_path.with_extension("tmp");
        std::fs::write(&tmp, buf).map_err(io_err)?;
        std::fs::rename(&tmp, &self.meta_path).map_err(io_err)?;
        Ok(())
    }
}
