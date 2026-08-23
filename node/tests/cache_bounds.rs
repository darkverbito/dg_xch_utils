mod common;

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_core::blockchain::sub_epoch_summary::SubEpochSummary;
use dg_xch_node::BlockRecordCache;

fn ses() -> SubEpochSummary {
    SubEpochSummary {
        prev_subepoch_summary_hash: Bytes32::from([1u8; 32]),
        reward_chain_hash: Bytes32::from([2u8; 32]),
        num_blocks_overflow: 0,
        new_difficulty: Some(1000),
        new_sub_slot_iters: Some(1 << 20),
    }
}

// Synthesize a record at a given height from a real mainnet template; the header hash is derived from the
// height so each is distinct and re-derivable.
fn record_at(
    template: &BlockRecord,
    height: u32,
    sub_epoch: Option<SubEpochSummary>,
) -> BlockRecord {
    let mut r = template.clone();
    r.height = height;
    let mut hh = [0u8; 32];
    hh[28..32].copy_from_slice(&height.to_be_bytes());
    r.header_hash = Bytes32::from(hh);
    r.sub_epoch_summary_included = sub_epoch;
    r
}

// The height-window stays bounded, hit/miss is correct across eviction, and sub-epoch-summary
// bookkeeping resolves across the boundary.
#[test]
fn window_is_bounded_with_correct_hit_miss_across_sub_epoch_boundary() {
    let records = common::load_records();
    let template = &records[0];
    let capacity = 8usize;
    let mut cache = BlockRecordCache::new(capacity);

    // Insert heights 100..=115 (16 records); a sub-epoch summary lands at height 112.
    let boundary = 112u32;
    for h in 100..=115u32 {
        let se = if h == boundary { Some(ses()) } else { None };
        cache.insert(record_at(template, h, se));
    }

    // Bound holds: exactly `capacity` records, the most recent window 108..=115.
    assert_eq!(cache.len(), capacity, "window bounded at capacity");
    assert!(cache.get_by_height(115).is_some(), "recent height hits");
    assert!(cache.get_by_height(108).is_some(), "window floor hits");
    assert!(cache.get_by_height(107).is_none(), "evicted height misses");
    assert!(cache.get_by_height(100).is_none(), "oldest height evicted");

    // Point-get by hash matches the by-height view.
    let mut hh = [0u8; 32];
    hh[28..32].copy_from_slice(&110u32.to_be_bytes());
    assert!(
        cache.get(&Bytes32::from(hh)).is_some(),
        "hash hit inside window"
    );

    // Sub-epoch bookkeeping across the boundary: at/above 112 resolves to the summary; below it, none.
    assert!(
        cache.sub_epoch_summary_at_or_below(111).is_none(),
        "below the boundary there is no summary in-window"
    );
    let (sh, _) = cache
        .sub_epoch_summary_at_or_below(115)
        .expect("summary at/above boundary");
    assert_eq!(sh, boundary, "resolves to the boundary height");
    assert_eq!(
        cache.latest_sub_epoch_summary().map(|(h, _)| h),
        Some(boundary)
    );

    // A reorg overwriting a height evicts the old hash at that height.
    let replaced = record_at(template, 115, None);
    let mut alt = [0xffu8; 32];
    alt[28..32].copy_from_slice(&115u32.to_be_bytes());
    let mut replaced = replaced;
    replaced.header_hash = Bytes32::from(alt);
    let mut orig_115 = [0u8; 32];
    orig_115[28..32].copy_from_slice(&115u32.to_be_bytes());
    cache.insert(replaced);
    assert!(
        cache.get(&Bytes32::from(orig_115)).is_none(),
        "old hash at a reorged height is evicted"
    );
    assert!(cache.get(&Bytes32::from(alt)).is_some(), "new hash present");
    assert_eq!(cache.len(), capacity, "still bounded after overwrite");
}

// The production window is fixed at ~5,120 records (~5–6 MB) — bounded, not unbounded.
#[test]
fn default_window_is_the_fixed_bound() {
    assert_eq!(dg_xch_node::BLOCK_RECORD_WINDOW, 5120);
    let mut cache = BlockRecordCache::with_default_window();
    let records = common::load_records();
    let template = &records[0];
    for h in 0..6000u32 {
        cache.insert(record_at(template, h, None));
    }
    assert_eq!(cache.len(), 5120, "never grows past the fixed window");
}
