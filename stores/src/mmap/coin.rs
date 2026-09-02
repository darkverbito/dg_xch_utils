use super::{COIN_PAYLOAD, MmapStore};
use crate::error::StoreError;
use crate::traits::CoinStore;
#[cfg(feature = "coin-index")]
use crate::traits::{coin_state_from_record, merge_coin_states_bounded, page_coin_states};
use async_trait::async_trait;
use dg_xch_core::blockchain::coin::Coin;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
#[cfg(feature = "coin-index")]
use dg_xch_core::protocols::wallet::{CoinState, CoinStateFilters};
use dg_xch_serialize::{ChiaProtocolVersion, ChiaSerialize};
use std::io::Cursor;

const VERSION: ChiaProtocolVersion = ChiaProtocolVersion::Chia0_0_37;

// Coin log frame: [Coin (ChiaSerialize)][1B coinbase][4B confirmed_index][8B timestamp].
// The table payload is authoritative for spend state (updated in place on spend/unspend);
// the frame is immutable insert-time data.
fn encode_coin(cr: &CoinRecord, height: u32, timestamp: u64) -> Result<Vec<u8>, StoreError> {
    let mut out = cr.coin.to_bytes(VERSION)?;
    out.push(u8::from(cr.coinbase));
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    Ok(out)
}

fn decode_coin(frame: &[u8], spent_index: u32) -> Result<CoinRecord, StoreError> {
    let mut cursor = Cursor::new(frame);
    let coin = Coin::from_bytes(&mut cursor, VERSION)?;
    let rest = &frame[cursor.position() as usize..];
    if rest.len() < 13 {
        return Err(StoreError::Corrupt("mmap store: short coin frame".into()));
    }
    let coinbase = rest[0] != 0;
    let confirmed = u32::from_le_bytes(rest[1..5].try_into().expect("frame layout"));
    let timestamp = u64::from_le_bytes(rest[5..13].try_into().expect("frame layout"));
    Ok(CoinRecord {
        coin,
        confirmed_block_index: confirmed,
        spent_block_index: spent_index,
        coinbase,
        timestamp,
        spent: spent_index > 0,
    })
}

// Payload codec: [coin_off+1:8][spent_index:4][confirmed_index:4]. An all-zero offset field is
// the reorg-deleted sentinel: the chain record stays (libbitcoin keeps table structure
// immutable), the entry just reads as absent.
struct CoinEntry {
    off: Option<u64>,
    spent_index: u32,
    confirmed_index: u32,
}

impl CoinEntry {
    fn parse(p: &[u8; COIN_PAYLOAD]) -> Self {
        Self {
            off: u64::from_le_bytes(p[..8].try_into().expect("payload")).checked_sub(1),
            spent_index: u32::from_le_bytes(p[8..12].try_into().expect("payload")),
            confirmed_index: u32::from_le_bytes(p[12..16].try_into().expect("payload")),
        }
    }

    fn pack(&self) -> [u8; COIN_PAYLOAD] {
        let mut out = [0u8; COIN_PAYLOAD];
        out[..8].copy_from_slice(&self.off.map_or(0, |o| o + 1).to_le_bytes());
        out[8..12].copy_from_slice(&self.spent_index.to_le_bytes());
        out[12..16].copy_from_slice(&self.confirmed_index.to_le_bytes());
        out
    }
}

impl MmapStore {
    async fn coin_by_name(&self, coin_name: &Bytes32) -> Result<Option<CoinRecord>, StoreError> {
        let Some(index) = self.coins_tbl.find(coin_name)? else {
            return Ok(None);
        };
        let entry = CoinEntry::parse(&self.coins_tbl.payload(index)?);
        let Some(off) = entry.off else {
            return Ok(None); // reorg-deleted sentinel
        };
        decode_coin(&self.coins.read(off).await?, entry.spent_index).map(Some)
    }

    // The service-tier scans (coin-index / hint features): a sequential sweep of the coin table
    // decoding each live record. Correct but linear — the Pi profile's pure validating node runs
    // without these features; enabling them on this backend trades query speed for zero extra
    // index maintenance on the write path.
    #[cfg(feature = "coin-index")]
    async fn scan_coins(
        &self,
        mut keep: impl FnMut(&CoinRecord) -> bool,
    ) -> Result<Vec<CoinRecord>, StoreError> {
        let mut live = Vec::new();
        self.coins_tbl.for_each(|_, payload| {
            let entry = CoinEntry::parse(payload);
            if let Some(off) = entry.off {
                live.push((off, entry.spent_index));
            }
        })?;
        let mut out = Vec::new();
        for (off, spent) in live {
            let cr = decode_coin(&self.coins.read(off).await?, spent)?;
            if keep(&cr) {
                out.push(cr);
            }
        }
        Ok(out)
    }
}

#[async_trait]
impl CoinStore for MmapStore {
    async fn get_coin_record(&self, coin_name: &Bytes32) -> Result<Option<CoinRecord>, StoreError> {
        self.coin_by_name(coin_name).await
    }

    async fn get_coin_records(&self, names: &[Bytes32]) -> Result<Vec<CoinRecord>, StoreError> {
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if let Some(cr) = self.coin_by_name(name).await? {
                out.push(cr);
            }
        }
        Ok(out)
    }

    async fn apply_block(
        &self,
        height: u32,
        timestamp: u64,
        additions: &[CoinRecord],
        removals: &[Bytes32],
    ) -> Result<(), StoreError> {
        let _w = self.write_lock.lock().await;
        // Two-phase sync-before-link (the per-block form of the batched rule): append every new
        // coin's log frame first, then ONE log fsync, then all the table links. The previous
        // shape fsynced the log per NEW coin — one ~100 ms iSCSI sync per coin, the measured
        // 1.7 s/block mm-leg confirm cost at dust-era addition counts.
        let mut pending: Vec<(Bytes32, [u8; 16])> = Vec::new();
        let mut pending_index: std::collections::HashMap<Bytes32, usize> =
            std::collections::HashMap::new();
        for cr in additions {
            let name = cr.coin.name();
            let frame = encode_coin(cr, height, timestamp)?;
            let off = self.coins.append(&frame).await?;
            let entry = CoinEntry {
                off: Some(off),
                spent_index: 0,
                confirmed_index: height,
            };
            match self.coins_tbl.find(&name)? {
                // Re-apply/replay of an existing coin: in-place repoint, no ordering constraint
                // (the payload update is idempotent and the frame is already appended).
                Some(index) => self.coins_tbl.set_payload(index, &entry.pack())?,
                None => {
                    let slot = pending.len();
                    pending.push((name, entry.pack()));
                    pending_index.insert(name, slot);
                }
            }
        }
        for name in removals {
            // A spend of a coin added in this same block (ephemeral) updates the staged payload;
            // anything else is an in-place update to an existing entry.
            if let Some(&slot) = pending_index.get(name) {
                let mut entry = CoinEntry::parse(&pending[slot].1);
                entry.spent_index = height;
                pending[slot].1 = entry.pack();
            } else if let Some(index) = self.coins_tbl.find(name)? {
                let mut entry = CoinEntry::parse(&self.coins_tbl.payload(index)?);
                entry.spent_index = height;
                self.coins_tbl.set_payload(index, &entry.pack())?;
            }
        }
        if !pending.is_empty() {
            // libbitcoin ordering at the block boundary: log durable before any link lands.
            self.coins.sync().await?;
            self.coins_tbl.insert_batch(&pending)?;
        }
        // One durability point per block batch.
        self.coins.sync().await?;
        self.coins_tbl.sync()
    }

    async fn apply_block_in(
        &self,
        batch: &mut crate::types::BatchHandle,
        height: u32,
        timestamp: u64,
        additions: &[CoinRecord],
        removals: &[Bytes32],
    ) -> Result<(), StoreError> {
        // Batched sync-before-link with FULL deferral: log frames append now (unsynced); EVERY
        // table mutation — new-coin links, replays, and in-place spends of pre-existing coins —
        // stages an absolute payload image in the batch and lands at its durability point
        // (set_peak_in / commit) behind ONE log fsync + ONE ordering msync for the whole batch —
        // see [`crate::types::MmapBatch`]. Nothing writes through: a dropped batch (a failed
        // reorg, a mid-window error) leaves the logical store untouched (T0-4) — only
        // unreferenced log frames leak, invisible to every read path.
        let _w = self.write_lock.lock().await;
        let b = batch.mmap_batch()?;
        for cr in additions {
            let name = cr.coin.name();
            let frame = encode_coin(cr, height, timestamp)?;
            let off = self.coins.append(&frame).await?;
            let entry = CoinEntry {
                off: Some(off),
                spent_index: 0,
                confirmed_index: height,
            };
            if let Some(&slot) = b.pending_index.get(&name) {
                // Replay within one batch: shadow the staged image with the fresh frame.
                b.pending_coins[slot].1 = entry.pack();
            } else {
                // New key or replay of an existing one — either way the staged image is absolute,
                // so publish resolves it with one find (set_payload vs insert) at drain time.
                let slot = b.pending_coins.len();
                b.pending_coins.push((name, entry.pack()));
                b.pending_index.insert(name, slot);
            }
        }
        for name in removals {
            if let Some(&slot) = b.pending_index.get(name) {
                let mut entry = CoinEntry::parse(&b.pending_coins[slot].1);
                entry.spent_index = height;
                b.pending_coins[slot].1 = entry.pack();
            } else if b.sweep.as_ref().is_some_and(|s| s.deletions.contains(name)) {
                // The staged reorg sweep deletes this coin (an old-branch creation above the
                // fork): the removal must stay a no-op — staging a spend image from the live
                // entry would resurrect the swept coin at publish time. The SQL backends get the
                // same answer free: their in-transaction DELETE runs first, so the branch's
                // UPDATE matches nothing.
            } else if let Some(index) = self.coins_tbl.find(name)? {
                // Spend of a pre-existing coin: stage the updated image (off/confirmed copied from
                // the live entry) instead of writing through.
                let mut entry = CoinEntry::parse(&self.coins_tbl.payload(index)?);
                entry.spent_index = height;
                let slot = b.pending_coins.len();
                b.pending_coins.push((*name, entry.pack()));
                b.pending_index.insert(*name, slot);
            }
        }
        Ok(())
    }

    async fn rollback_to(&self, fork_height: u32) -> Result<u64, StoreError> {
        let _w = self.write_lock.lock().await;
        // Sweep the table: sentinel-delete additions above the fork, un-spend removals above it.
        let mut deletions: Vec<Bytes32> = Vec::new();
        let mut unspends: Vec<Bytes32> = Vec::new();
        self.coins_tbl.for_each(|key, payload| {
            let entry = CoinEntry::parse(payload);
            if entry.off.is_none() {
                return;
            }
            if entry.confirmed_index > fork_height {
                deletions.push(*key);
            } else if entry.spent_index > fork_height {
                unspends.push(*key);
            }
        })?;
        let mut touched = 0u64;
        for key in &deletions {
            if let Some(index) = self.coins_tbl.find(key)? {
                let mut entry = CoinEntry::parse(&self.coins_tbl.payload(index)?);
                entry.off = None;
                entry.spent_index = 0;
                self.coins_tbl.set_payload(index, &entry.pack())?;
                touched += 1;
            }
        }
        for key in &unspends {
            if let Some(index) = self.coins_tbl.find(key)? {
                let mut entry = CoinEntry::parse(&self.coins_tbl.payload(index)?);
                entry.spent_index = 0;
                self.coins_tbl.set_payload(index, &entry.pack())?;
                touched += 1;
            }
        }
        self.coins_tbl.sync()?;
        Ok(touched)
    }

    async fn rollback_to_in(
        &self,
        batch: &mut crate::types::BatchHandle,
        fork_height: u32,
    ) -> Result<u64, StoreError> {
        // The reorg's fork revert, STAGED: scan now (read-only — the count is the return value and
        // the pre-branch table is what the sweep must revert), publish at the batch's durability
        // point bracketed by the reorg journal (see MmapStore::flush_coin_batch). Nothing staged
        // is published before then, so stage-time and publish-time scans see the same table; a
        // dropped batch leaves the store untouched (T0-4).
        let _w = self.write_lock.lock().await;
        let mut deletions: std::collections::HashSet<Bytes32> = std::collections::HashSet::new();
        let mut unspends: Vec<Bytes32> = Vec::new();
        self.coins_tbl.for_each(|key, payload| {
            let entry = CoinEntry::parse(payload);
            if entry.off.is_none() {
                return;
            }
            if entry.confirmed_index > fork_height {
                deletions.insert(*key);
            } else if entry.spent_index > fork_height {
                unspends.push(*key);
            }
        })?;
        let count = (deletions.len() + unspends.len()) as u64;
        // The convergence peak a crashed publish rolls back to: the main chain at the fork,
        // captured before any mutation.
        let fork_hash = self.heights.get(fork_height)?;
        let b = batch.mmap_batch()?;
        b.sweep = Some(crate::types::StagedSweep {
            fork_height,
            fork_hash,
            deletions,
            unspends,
        });
        Ok(count)
    }

    #[cfg(feature = "coin-index")]
    async fn get_unspent_by_puzzle_hash(
        &self,
        ph: &Bytes32,
    ) -> Result<Vec<CoinRecord>, StoreError> {
        self.scan_coins(|cr| !cr.spent && cr.coin.puzzle_hash == *ph)
            .await
    }

    #[cfg(feature = "coin-index")]
    async fn get_coins_by_parent(&self, parent: &Bytes32) -> Result<Vec<CoinRecord>, StoreError> {
        self.scan_coins(|cr| cr.coin.parent_coin_info == *parent)
            .await
    }

    #[cfg(feature = "coin-index")]
    async fn get_coin_states_by_puzzle_hashes(
        &self,
        puzzle_hashes: &[Bytes32],
        min_height: u32,
        include_spent: bool,
        max_items: usize,
    ) -> Result<Vec<CoinState>, StoreError> {
        if puzzle_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let set: std::collections::HashSet<Bytes32> = puzzle_hashes.iter().copied().collect();
        let records = self
            .scan_coins(|cr| {
                set.contains(&cr.coin.puzzle_hash)
                    && (cr.confirmed_block_index >= min_height
                        || cr.spent_block_index >= min_height)
                    && (include_spent || cr.spent_block_index == 0)
            })
            .await?;
        Ok(records
            .iter()
            .take(max_items)
            .map(coin_state_from_record)
            .collect())
    }

    #[cfg(feature = "coin-index")]
    async fn batch_coin_states_by_puzzle_hashes(
        &self,
        puzzle_hashes: &[Bytes32],
        min_height: u32,
        filters: &CoinStateFilters,
        max_items: usize,
    ) -> Result<(Vec<CoinState>, Option<u32>), StoreError> {
        if puzzle_hashes.is_empty() || (!filters.include_spent && !filters.include_unspent) {
            return Ok((Vec::new(), None));
        }
        let set: std::collections::HashSet<Bytes32> = puzzle_hashes.iter().copied().collect();
        #[cfg(feature = "hint")]
        let hinted: std::collections::HashSet<Bytes32> = if filters.include_hinted {
            use dg_xch_core::traits::SizedBytes;
            let mut out = std::collections::HashSet::new();
            let mut off = 0u64;
            while let Ok(frame) = self.hints.read(off).await {
                if frame.len() == 64 {
                    let mut hint = [0u8; 32];
                    hint.copy_from_slice(&frame[..32]);
                    if set.contains(&Bytes32::new(hint)) {
                        let mut id = [0u8; 32];
                        id.copy_from_slice(&frame[32..]);
                        out.insert(Bytes32::new(id));
                    }
                }
                off += 4 + frame.len() as u64;
            }
            out
        } else {
            std::collections::HashSet::new()
        };
        #[cfg(not(feature = "hint"))]
        let hinted: std::collections::HashSet<Bytes32> = std::collections::HashSet::new();
        let include_spent = filters.include_spent;
        let include_unspent = filters.include_unspent;
        let min_amount = filters.min_amount;
        let records = self
            .scan_coins(|cr| {
                (set.contains(&cr.coin.puzzle_hash)
                    || (!hinted.is_empty() && hinted.contains(&cr.coin.name())))
                    && (cr.confirmed_block_index >= min_height
                        || cr.spent_block_index >= min_height)
                    && (if cr.spent_block_index > 0 {
                        include_spent
                    } else {
                        include_unspent
                    })
                    && cr.coin.amount >= min_amount
            })
            .await?;
        let mut merged = std::collections::HashMap::new();
        merge_coin_states_bounded(
            &mut merged,
            records.iter().map(coin_state_from_record),
            max_items + 1,
        );
        Ok(page_coin_states(merged, max_items))
    }

    #[cfg(feature = "coin-index")]
    async fn get_coins_added_at_height(&self, height: u32) -> Result<Vec<CoinRecord>, StoreError> {
        self.scan_coins(|cr| cr.confirmed_block_index == height)
            .await
    }

    #[cfg(feature = "coin-index")]
    async fn get_coins_removed_at_height(
        &self,
        height: u32,
    ) -> Result<Vec<CoinRecord>, StoreError> {
        self.scan_coins(|cr| cr.spent_block_index == height).await
    }

    // The mmap hint tier is an append-only 64-byte-frame log scanned on query (see
    // `stores/src/mmap/hint.rs`); it is not part of the coin batch's staged links, so hints are
    // appended directly here rather than through `MmapBatch`. `batch.require_mmap()` keeps the
    // cross-backend fail-closed guard even though the frames land outside the batch's msync.
    // Scan the append-only 64-byte-frame hint log `[32B hint][32B coin_id]` (see the module note
    // on the mmap hint tier being a scan tier, not an index).
    #[cfg(feature = "hint")]
    async fn get_coins_for_hint(
        &self,
        hint: &Bytes32,
        max_items: usize,
    ) -> Result<Vec<Bytes32>, StoreError> {
        use dg_xch_core::traits::SizedBytes;
        let mut out = Vec::new();
        let mut off = 0u64;
        while out.len() < max_items {
            let Ok(frame) = self.hints.read(off).await else {
                break;
            };
            let hb: &[u8] = hint.as_ref();
            if frame.len() == 64 && frame[..32] == *hb {
                let mut id = [0u8; 32];
                id.copy_from_slice(&frame[32..]);
                out.push(Bytes32::new(id));
            }
            off += 4 + frame.len() as u64;
        }
        Ok(out)
    }

    async fn apply_hints_in(
        &self,
        batch: &mut crate::types::BatchHandle,
        pairs: &[(Bytes32, Bytes32)],
    ) -> Result<(), StoreError> {
        batch.require_mmap()?;
        self.apply_hints(pairs).await
    }

    async fn apply_hints(&self, pairs: &[(Bytes32, Bytes32)]) -> Result<(), StoreError> {
        #[cfg(feature = "hint")]
        {
            let _w = self.write_lock.lock().await;
            for (hint, coin_id) in pairs {
                let mut frame = Vec::with_capacity(64);
                frame.extend_from_slice(hint);
                frame.extend_from_slice(coin_id);
                self.hints.append(&frame).await?;
            }
            self.hints.sync().await
        }
        #[cfg(not(feature = "hint"))]
        {
            let _ = pairs;
            Ok(())
        }
    }
}
