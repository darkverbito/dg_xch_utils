#![allow(dead_code)]

use dg_xch_core::blockchain::block_record::BlockRecord;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::full_block::FullBlock;
use dg_xch_stores::SqliteStore;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
struct AddsRems {
    additions: Vec<CoinRecord>,
    removals: Vec<CoinRecord>,
}

#[must_use]
pub fn load_records() -> Vec<BlockRecord> {
    serde_json::from_str(include_str!("../fixtures/block_records.json")).expect("records fixture")
}

#[must_use]
pub fn load_full_block(height: u32) -> FullBlock {
    let raw = match height {
        5_000_000 => include_str!("../fixtures/full_block_5000000.json"),
        5_000_004 => include_str!("../fixtures/full_block_5000004.json"),
        other => panic!("no full-block fixture for height {other}"),
    };
    serde_json::from_str(raw).expect("full block fixture")
}

/// Real mainnet additions/removals for a transaction block; removals are returned as coin ids.
#[must_use]
pub fn load_adds_rems(height: u32) -> (Vec<CoinRecord>, Vec<CoinRecord>) {
    let raw = match height {
        5_000_000 => include_str!("../fixtures/adds_rems_5000000.json"),
        5_000_004 => include_str!("../fixtures/adds_rems_5000004.json"),
        5_000_007 => include_str!("../fixtures/adds_rems_5000007.json"),
        5_000_012 => include_str!("../fixtures/adds_rems_5000012.json"),
        other => panic!("no adds/rems fixture for height {other}"),
    };
    let ar: AddsRems = serde_json::from_str(raw).expect("adds/rems fixture");
    (ar.additions, ar.removals)
}

#[must_use]
pub fn unique_db_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "dg_xch_stores_test_{}_{n}.sqlite",
        std::process::id()
    ))
}

pub async fn new_store() -> SqliteStore {
    SqliteStore::open(&unique_db_path())
        .await
        .expect("open store")
}

pub async fn new_store_at(path: &std::path::Path) -> SqliteStore {
    SqliteStore::open(path).await.expect("open store")
}

/// True if a named object exists in the schema of the database at `path` (test-side schema probe).
pub async fn index_exists(path: &std::path::Path, name: &str) -> bool {
    let url = format!("sqlite://{}?mode=ro", path.display());
    let pool = sqlx::SqlitePool::connect(&url)
        .await
        .expect("probe connect");
    let row = sqlx::query("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?")
        .bind(name)
        .fetch_optional(&pool)
        .await
        .expect("probe query");
    row.is_some()
}
