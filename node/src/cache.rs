use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use std::collections::BTreeMap;
use std::collections::HashMap;

// Fixed height-window of recent block records: ~5,120 records ≈ 5–6 MB, bounded. A
// growing HashMap is the anti-pattern; the ecosystem alternative is `portfu_core::cache::CircularCache`,
// used here as a height-keyed ring so eviction is deterministic (lowest height first) and reorg-stable.
pub const BLOCK_RECORD_WINDOW: usize = 5120;

pub struct BlockRecordCache {
    capacity: usize,
    by_hash: HashMap<Bytes32, BlockRecord>,
    by_height: BTreeMap<u32, Bytes32>,
    sub_epoch_summaries: BTreeMap<u32, SubEpochSummary>,
}

impl BlockRecordCache {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            by_hash: HashMap::new(),
            by_height: BTreeMap::new(),
            sub_epoch_summaries: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn with_default_window() -> Self {
        Self::new(BLOCK_RECORD_WINDOW)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_height.len()
    }

    /// The height-window bound this cache holds (records above it evict lowest-first).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_height.is_empty()
    }

    // Insert a confirmed record; a reorg overwriting a height evicts the old hash at that height. Holds the
    // window at `capacity` by dropping the lowest height once full.
    pub fn insert(&mut self, record: BlockRecord) {
        let height = record.height;
        let hash = record.header_hash;
        if let Some(old) = self.by_height.insert(height, hash)
            && old != hash
        {
            self.by_hash.remove(&old);
        }
        if let Some(ses) = record.sub_epoch_summary_included {
            self.sub_epoch_summaries.insert(height, ses);
        } else {
            self.sub_epoch_summaries.remove(&height);
        }
        self.by_hash.insert(hash, record);
        while self.by_height.len() > self.capacity {
            let Some((&low, &low_hash)) = self.by_height.iter().next() else {
                break;
            };
            self.by_height.remove(&low);
            self.by_hash.remove(&low_hash);
            self.sub_epoch_summaries.remove(&low);
        }
    }

    #[must_use]
    pub fn get(&self, hash: &Bytes32) -> Option<&BlockRecord> {
        self.by_hash.get(hash)
    }

    // The windowed records keyed by header hash — the ancestor set header validation walks.
    #[must_use]
    pub fn records(&self) -> &HashMap<Bytes32, BlockRecord> {
        &self.by_hash
    }

    #[must_use]
    pub fn get_by_height(&self, height: u32) -> Option<&BlockRecord> {
        let hash = self.by_height.get(&height)?;
        self.by_hash.get(hash)
    }

    // Most recent sub-epoch summary at or below `height` — the retarget anchor the engine reads.
    #[must_use]
    pub fn sub_epoch_summary_at_or_below(&self, height: u32) -> Option<(u32, &SubEpochSummary)> {
        self.sub_epoch_summaries
            .range(..=height)
            .next_back()
            .map(|(h, s)| (*h, s))
    }

    #[must_use]
    pub fn latest_sub_epoch_summary(&self) -> Option<(u32, &SubEpochSummary)> {
        self.sub_epoch_summaries
            .iter()
            .next_back()
            .map(|(h, s)| (*h, s))
    }
}
