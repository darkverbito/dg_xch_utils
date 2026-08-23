//! The libbitcoin table primitives (see DESIGN.md, grounded in libbitcoin-database version3):
//! read-write memory maps with reserve-ahead growth, and hash tables built as a fixed bucket
//! array of chain heads over fixed-size chained records `[key][next:8][payload]`. Insert is
//! append + sync + link-at-head — libbitcoin's ordering: "data can be lost but the hashtable is
//! never corrupted." Chains absorb load; nothing is ever rehashed.

use super::io_err;
use crate::error::StoreError;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::traits::SizedBytes;
use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::RwLock;

// Reserve-ahead factor on growth (libbitcoin's expansion): grow to 150% of the needed size so
// remaps are amortized, truncated back to logical size only on close (we skip the truncate —
// sparse tail pages cost nothing).
const EXPANSION_NUM: u64 = 150;
const EXPANSION_DEN: u64 = 100;

/// A read-write memory-mapped file with reserve-ahead growth. All reads and writes go through
/// the map; `sync` is `msync`. Remap (growth) takes the write lock; accessors take read locks.
pub(crate) struct GrowableMap {
    file: std::fs::File,
    map: RwLock<MmapMut>,
    logical: RwLock<u64>,
}

impl GrowableMap {
    pub(crate) fn open(path: PathBuf, initial: u64) -> Result<Self, StoreError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(io_err)?;
        let disk = file.metadata().map_err(io_err)?.len();
        let logical = disk;
        if disk < initial {
            file.set_len(initial).map_err(io_err)?;
        }
        let map = unsafe { MmapMut::map_mut(&file) }.map_err(io_err)?;
        Ok(Self {
            file,
            map: RwLock::new(map),
            logical: RwLock::new(logical.max(initial.min(disk))),
        })
    }

    pub(crate) fn logical_len(&self) -> u64 {
        *self.logical.read().expect("logical lock")
    }

    pub(crate) fn set_logical_len(&self, len: u64) {
        *self.logical.write().expect("logical lock") = len;
    }

    /// Ensure the map covers `need` bytes, growing the file to `need * 150%` and remapping.
    pub(crate) fn reserve(&self, need: u64) -> Result<(), StoreError> {
        {
            let map = self.map.read().expect("map lock");
            if need <= map.len() as u64 {
                return Ok(());
            }
        }
        let mut map = self.map.write().expect("map lock");
        if need <= map.len() as u64 {
            return Ok(());
        }
        let target = need * EXPANSION_NUM / EXPANSION_DEN;
        self.file.set_len(target).map_err(io_err)?;
        *map = unsafe { MmapMut::map_mut(&self.file) }.map_err(io_err)?;
        Ok(())
    }

    pub(crate) fn read_at(&self, off: u64, out: &mut [u8]) -> Result<(), StoreError> {
        let map = self.map.read().expect("map lock");
        let start = off as usize;
        let src = map
            .get(start..start + out.len())
            .ok_or_else(|| StoreError::Corrupt("mmap: read past mapping".into()))?;
        out.copy_from_slice(src);
        Ok(())
    }

    pub(crate) fn write_at(&self, off: u64, data: &[u8]) -> Result<(), StoreError> {
        self.reserve(off + data.len() as u64)?;
        let mut map = self.map.write().expect("map lock");
        let start = off as usize;
        let dst = map
            .get_mut(start..start + data.len())
            .ok_or_else(|| StoreError::Corrupt("mmap: write past mapping".into()))?;
        dst.copy_from_slice(data);
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), StoreError> {
        self.map.read().expect("map lock").flush().map_err(io_err)
    }
}

/// A libbitcoin record hash table: one file, laid out as
/// `[8B record_count][8B bucket_count][bucket_count × 8B chain heads][records…]`,
/// each record `[32B key][8B next][PAYLOAD bytes]`. Head/next values store `index + 1`
/// (0 = end of chain). The bucket count is fixed at creation and never rehashed.
pub(crate) struct ChainedTable<const PAYLOAD: usize> {
    map: GrowableMap,
    buckets: u64,
}

const HEAD_BYTES: u64 = 16;

impl<const PAYLOAD: usize> ChainedTable<PAYLOAD> {
    const RECORD: u64 = 32 + 8 + PAYLOAD as u64;

    pub(crate) fn open(path: PathBuf, buckets: u64) -> Result<Self, StoreError> {
        debug_assert!(buckets.is_power_of_two());
        let header = HEAD_BYTES + buckets * 8;
        let map = GrowableMap::open(path, header)?;
        // A fresh file: stamp the bucket count; an existing one: trust its stamp.
        let mut stamp = [0u8; 8];
        map.read_at(8, &mut stamp)?;
        let existing = u64::from_le_bytes(stamp);
        let buckets = if existing == 0 {
            map.write_at(8, &buckets.to_le_bytes())?;
            buckets
        } else {
            existing
        };
        if map.logical_len() < header {
            map.set_logical_len(header);
        }
        Ok(Self { map, buckets })
    }

    fn record_count(&self) -> Result<u64, StoreError> {
        let mut b = [0u8; 8];
        self.map.read_at(0, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn record_off(&self, index: u64) -> u64 {
        HEAD_BYTES + self.buckets * 8 + index * Self::RECORD
    }

    fn bucket_of(&self, key: &Bytes32) -> u64 {
        let b: &[u8] = key;
        u64::from_le_bytes([b[24], b[25], b[26], b[27], b[28], b[29], b[30], b[31]])
            & (self.buckets - 1)
    }

    fn head(&self, bucket: u64) -> Result<u64, StoreError> {
        let mut b = [0u8; 8];
        self.map.read_at(HEAD_BYTES + bucket * 8, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Walk the bucket chain for `key`; returns the record index when present.
    pub(crate) fn find(&self, key: &Bytes32) -> Result<Option<u64>, StoreError> {
        let kb: &[u8] = key;
        let mut link = self.head(self.bucket_of(key))?;
        let mut rec = [0u8; 40];
        while link != 0 {
            let index = link - 1;
            self.map.read_at(self.record_off(index), &mut rec)?;
            if &rec[..32] == kb {
                return Ok(Some(index));
            }
            link = u64::from_le_bytes(rec[32..40].try_into().expect("record layout"));
        }
        Ok(None)
    }

    pub(crate) fn payload(&self, index: u64) -> Result<[u8; PAYLOAD], StoreError> {
        let mut out = [0u8; PAYLOAD];
        self.map.read_at(self.record_off(index) + 40, &mut out)?;
        Ok(out)
    }

    pub(crate) fn set_payload(
        &self,
        index: u64,
        payload: &[u8; PAYLOAD],
    ) -> Result<(), StoreError> {
        self.map.write_at(self.record_off(index) + 40, payload)
    }

    /// Insert a new record for `key` (callers ensure the key is absent or want a shadowing
    /// entry at the chain head). libbitcoin's ordering: the record bytes are written and synced
    /// before the head link lands, so a torn shutdown loses the record, never the table.
    pub(crate) fn insert(
        &self,
        key: &Bytes32,
        payload: &[u8; PAYLOAD],
        durable: bool,
    ) -> Result<u64, StoreError> {
        let index = self.record_count()?;
        let bucket = self.bucket_of(key);
        let prev_head = self.head(bucket)?;
        let off = self.record_off(index);
        let mut rec = Vec::with_capacity(Self::RECORD as usize);
        let kb: &[u8] = key;
        rec.extend_from_slice(kb);
        rec.extend_from_slice(&prev_head.to_le_bytes());
        rec.extend_from_slice(payload);
        self.map.write_at(off, &rec)?;
        if durable {
            self.map.sync()?;
        }
        // Link at head + bump the count (the count is the allocation pointer).
        self.map
            .write_at(HEAD_BYTES + bucket * 8, &(index + 1).to_le_bytes())?;
        self.map.write_at(0, &(index + 1).to_le_bytes())?;
        self.map.set_logical_len(off + Self::RECORD);
        Ok(index)
    }

    /// Insert a batch of new records with ONE durability ordering point for the whole batch —
    /// the libbitcoin sync-before-link rule applied at the batch boundary (DESIGN.md §4: "data
    /// can be lost but the hashtable is never corrupted"). Two phases:
    ///
    /// A. Every record's bytes (key, chain link, payload) are written, with intra-batch chain
    ///    threading resolved through a staged-heads view (two batch keys sharing a bucket chain
    ///    correctly), then `msync`ed ONCE.
    /// B. The record count (the allocation pointer) is published FIRST, then every staged bucket
    ///    head. A link is only ever written after the bytes it points to are durable (phase A),
    ///    and never before the count that protects those bytes from re-allocation — so a torn
    ///    shutdown loses records (invisible, past-count, or unlinked) but never corrupts a chain.
    ///
    /// This replaces N× (`msync` + link) with 1× `msync` + N links: the per-insert `durable`
    /// sync was one full-map `msync` per NEW key — the dominant confirm cost on high-latency
    /// backing stores (iSCSI ~100 ms/sync, one per new coin).
    ///
    /// Callers hold the store write lock; readers may walk chains concurrently and observe
    /// either the old or the fully-written new head (same visibility as single `insert`).
    pub(crate) fn insert_batch(
        &self,
        entries: &[(Bytes32, [u8; PAYLOAD])],
    ) -> Result<(), StoreError> {
        if entries.is_empty() {
            return Ok(());
        }
        let base = self.record_count()?;
        // Phase A: write all record bytes, threading intra-batch bucket chains.
        let mut staged_heads: std::collections::HashMap<u64, u64> =
            std::collections::HashMap::new();
        for (i, (key, payload)) in entries.iter().enumerate() {
            let index = base + i as u64;
            let bucket = self.bucket_of(key);
            let prev_head = match staged_heads.get(&bucket) {
                Some(&h) => h,
                None => self.head(bucket)?,
            };
            let off = self.record_off(index);
            let mut rec = Vec::with_capacity(Self::RECORD as usize);
            let kb: &[u8] = key;
            rec.extend_from_slice(kb);
            rec.extend_from_slice(&prev_head.to_le_bytes());
            rec.extend_from_slice(payload);
            self.map.write_at(off, &rec)?;
            staged_heads.insert(bucket, index + 1);
        }
        // The one ordering point: record bytes durable before any link or count lands.
        self.map.sync()?;
        // Phase B: allocation pointer first, then the bucket heads.
        let count = base + entries.len() as u64;
        self.map.write_at(0, &count.to_le_bytes())?;
        for (bucket, head) in staged_heads {
            self.map
                .write_at(HEAD_BYTES + bucket * 8, &head.to_le_bytes())?;
        }
        self.map.set_logical_len(self.record_off(count));
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), StoreError> {
        self.map.sync()
    }

    /// Sequential visit of every record: `(key, payload)`.
    pub(crate) fn for_each(
        &self,
        mut visit: impl FnMut(&Bytes32, &[u8; PAYLOAD]),
    ) -> Result<(), StoreError> {
        let count = self.record_count()?;
        let mut rec = vec![0u8; Self::RECORD as usize];
        for index in 0..count {
            self.map.read_at(self.record_off(index), &mut rec)?;
            let mut key = [0u8; 32];
            key.copy_from_slice(&rec[..32]);
            let mut payload = [0u8; PAYLOAD];
            payload.copy_from_slice(&rec[40..40 + PAYLOAD]);
            visit(&Bytes32::new(key), &payload);
        }
        Ok(())
    }
}

impl GrowableMap {
    pub(crate) fn mapped_len(&self) -> u64 {
        self.map.read().expect("map lock").len() as u64
    }
}
