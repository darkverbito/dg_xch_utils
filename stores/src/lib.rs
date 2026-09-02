mod arc;
pub mod error;
#[cfg(feature = "mmap")]
pub mod mmap;
#[cfg(feature = "postgres")]
pub mod postgres;
mod record_compat;
pub mod sqlite;
pub mod telemetry;
pub mod traits;
pub mod types;

pub use error::StoreError;
#[cfg(feature = "mmap")]
pub use mmap::MmapStore;
#[cfg(feature = "postgres")]
pub use postgres::PostgresStore;
pub use sqlite::SqliteStore;
pub use telemetry::{DURATION_BUCKETS_SECS, HistogramSnapshot, StoreTelemetry};
pub use traits::{BlockStore, CoinStore};
pub use types::{BatchHandle, BlockStatus, Savepoint};

// Sort one block's coin-delta batches by coin_name BEFORE the apply chunks them into
// statements. The names are hashes, so block order is random with respect to the coin_record
// primary key: every row becomes an independent random descent into the multi-GB pkey btree — a
// distinct, uncached leaf page, i.e. a distinct random read, which dominates the confirm on
// high-latency storage. Sorting ONCE over the whole batch and then chunking makes each chunk
// key-contiguous, so its descents share btree path prefixes and touch few distinct leaf pages,
// and gives storage read-ahead something to work with. No semantic effect: the upsert
// (ON CONFLICT / INSERT OR REPLACE over unique names) and the spent-update are
// order-independent within and across chunks.
pub(crate) fn sort_additions_by_name(
    additions: &[dg_xch_core::blockchain::coin_record::CoinRecord],
) -> Vec<(
    dg_xch_core::blockchain::sized_bytes::Bytes32,
    &dg_xch_core::blockchain::coin_record::CoinRecord,
)> {
    use dg_xch_core::traits::SizedBytes;
    // Pair each record with its name so the hash is computed once (it is also the INSERT's key
    // bind), then sort the pairs.
    let mut named: Vec<_> = additions.iter().map(|cr| (cr.coin.name(), cr)).collect();
    named.sort_unstable_by_key(|(name, _)| name.bytes());
    named
}

pub(crate) fn sorted_removal_names(
    removals: &[dg_xch_core::blockchain::sized_bytes::Bytes32],
) -> Vec<dg_xch_core::blockchain::sized_bytes::Bytes32> {
    use dg_xch_core::traits::SizedBytes;
    let mut names = removals.to_vec();
    names.sort_unstable_by_key(|n| n.bytes());
    names
}

// Drop `--` comment lines from a migration file so a ';' inside a comment can never cut a
// statement in half when the deferred index build splits the file into single statements.
pub(crate) fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    for line in sql.lines() {
        if !line.trim_start().starts_with("--") {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod batch_sort_tests {
    use dg_xch_core::blockchain::coin::Coin;
    use dg_xch_core::blockchain::coin_record::CoinRecord;
    use dg_xch_core::blockchain::sized_bytes::Bytes32;
    use dg_xch_core::traits::SizedBytes;

    fn rec(tag: u8) -> CoinRecord {
        CoinRecord {
            coin: Coin {
                parent_coin_info: Bytes32::from([tag; 32]),
                puzzle_hash: Bytes32::from([0x11u8; 32]),
                amount: u64::from(tag),
            },
            confirmed_block_index: 0,
            spent_block_index: 0,
            coinbase: false,
            timestamp: 0,
            spent: false,
        }
    }

    // The whole point of the sort: after it, the batch is key-contiguous, so the fixed-size
    // chunks the apply cuts are runs of adjacent pkey values (shared btree paths, few distinct
    // leaf pages) instead of one random descent per row.
    #[test]
    fn addition_batches_are_key_sorted_with_names_paired() {
        let additions: Vec<CoinRecord> = (0u8..32).map(rec).collect();
        let sorted = crate::sort_additions_by_name(&additions);
        assert_eq!(sorted.len(), additions.len());
        for w in sorted.windows(2) {
            assert!(
                w[0].0.bytes() <= w[1].0.bytes(),
                "chunks must be cut from a key-contiguous batch"
            );
        }
        for (name, cr) in &sorted {
            assert_eq!(*name, cr.coin.name(), "the paired name IS the record's key");
        }
    }

    #[test]
    fn removal_batches_are_key_sorted() {
        let removals: Vec<Bytes32> = (0u8..32).rev().map(|t| rec(t).coin.name()).collect();
        let sorted = crate::sorted_removal_names(&removals);
        assert_eq!(sorted.len(), removals.len());
        for w in sorted.windows(2) {
            assert!(w[0].bytes() <= w[1].bytes());
        }
    }
}
