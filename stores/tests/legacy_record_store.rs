//! Restart-resume over a pre-#155 store: a sqlite store whose `block_record.record` blobs are in
//! the legacy layout (length-prefixed VDF outputs) must open and serve correctly under the
//! chia-layout code, and keep following as new (chia-layout) records land on top — no resync.
//!
//! The legacy layout is pinned structurally IN THIS TEST (`legacy_blob_of`): the pre-#155 encoder
//! wrote a u32-BE `0x00000064` prefix ahead of each bare 100-byte VDF output and was otherwise
//! byte-identical. The store's fallback decoder is proven against that pinned form, not against
//! its own inverse.

mod common;

use common::{load_records, new_store_at, unique_db_path};
use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
use dg_xch_stores::BlockStore;
use sqlx::Row;

// Offset of challenge_vdf_output in both layouts: 32 (header_hash) + 32 (prev_hash) + 4 (height)
// + 16 (weight) + 16 (total_iters) + 1 (signage_point_index).
const VDF_OFFSET: usize = 101;

fn legacy_blob_of(chia: &[u8]) -> Vec<u8> {
    const PREFIX: [u8; 4] = 100u32.to_be_bytes();
    let mut out = Vec::with_capacity(chia.len() + 8);
    out.extend_from_slice(&chia[..VDF_OFFSET]);
    out.extend_from_slice(&PREFIX);
    out.extend_from_slice(&chia[VDF_OFFSET..VDF_OFFSET + 100]);
    let tag_at = VDF_OFFSET + 100;
    out.push(chia[tag_at]);
    let rest = if chia[tag_at] == 1 {
        out.extend_from_slice(&PREFIX);
        out.extend_from_slice(&chia[tag_at + 1..tag_at + 101]);
        &chia[tag_at + 101..]
    } else {
        &chia[tag_at + 1..]
    };
    out.extend_from_slice(rest);
    out
}

/// Downgrade every stored record blob to the legacy layout in place — fabricating the on-disk
/// state of a store leg written entirely before #155.
async fn downgrade_store_records(path: &std::path::Path) {
    let url = format!("sqlite://{}", path.display());
    let pool = sqlx::SqlitePool::connect(&url).await.expect("rw connect");
    let rows = sqlx::query("SELECT header_hash, record FROM block_record")
        .fetch_all(&pool)
        .await
        .expect("read records");
    assert!(!rows.is_empty(), "store has records to downgrade");
    for row in rows {
        let hh: Vec<u8> = row.try_get("header_hash").expect("header_hash");
        let blob: Vec<u8> = row.try_get("record").expect("record");
        let legacy = legacy_blob_of(&blob);
        assert_ne!(legacy, blob, "downgrade changes the blob");
        sqlx::query("UPDATE block_record SET record = ? WHERE header_hash = ?")
            .bind(legacy)
            .bind(hh)
            .execute(&pool)
            .await
            .expect("write legacy blob");
    }
    pool.close().await;
}

#[tokio::test]
async fn legacy_layout_store_opens_and_follows() {
    let path = unique_db_path();
    let records = load_records();
    assert!(records.len() >= 3, "fixture has a usable chain");
    let mut chain: Vec<BlockRecord> = records;
    chain.sort_by_key(|r| r.height);
    let peak = chain.last().expect("peak").clone();

    // Phase 1: write the chain the way the pre-#155 fleet did — records + peak — then downgrade
    // every blob to the legacy byte layout and close the store.
    {
        let store = new_store_at(&path).await;
        store.add_block_records(&chain).await.expect("add records");
        store.set_peak(&peak.header_hash).await.expect("set peak");
        drop(store);
    }
    downgrade_store_records(&path).await;

    // Phase 2: reopen under the chia-layout code. Every read path must decode the legacy blobs.
    let store = new_store_at(&path).await;
    assert_eq!(
        store.get_peak().await.expect("peak"),
        Some((peak.header_hash, peak.height)),
        "peak survives the restart"
    );
    for rec in &chain {
        let by_hash = store
            .get_block_record(&rec.header_hash)
            .await
            .expect("get by hash")
            .expect("record present");
        assert_eq!(&by_hash, rec, "legacy blob decodes to the original record");
        let by_height = store
            .get_block_record_by_height(rec.height)
            .await
            .expect("get by height")
            .expect("main-chain record present");
        assert_eq!(&by_height, rec);
    }
    let hashes: Vec<_> = chain.iter().map(|r| r.header_hash).collect();
    let batch = store
        .get_block_records_by_hash(&hashes)
        .await
        .expect("batch get");
    assert_eq!(batch.len(), chain.len());

    // Phase 3: follow. New records land in the chia layout on top of the legacy blobs; the store
    // must extend the chain and serve both generations.
    let mut next = peak.clone();
    next.prev_hash = peak.header_hash;
    next.height = peak.height + 1;
    next.weight = peak.weight + 1;
    next.total_iters = peak.total_iters + 1;
    let mut hh = [0u8; 32];
    hh.copy_from_slice(AsRef::<[u8]>::as_ref(&peak.header_hash));
    hh[0] ^= 0xFF;
    next.header_hash = Bytes32::from(hh);
    next.sub_epoch_summary_included = None;
    store
        .add_block_records(std::slice::from_ref(&next))
        .await
        .expect("append new-layout record");
    store
        .set_peak(&next.header_hash)
        .await
        .expect("advance peak");
    assert_eq!(
        store.get_peak().await.expect("peak"),
        Some((next.header_hash, next.height)),
        "the chain follows past the legacy records"
    );
    let reread = store
        .get_block_record(&next.header_hash)
        .await
        .expect("get new record")
        .expect("present");
    assert_eq!(reread, next);
    // And the legacy ancestry is still served alongside.
    let again = store
        .get_block_record(&peak.header_hash)
        .await
        .expect("get old peak")
        .expect("present");
    assert_eq!(again, peak);
}
