use crate::error::StoreError;
use crate::sqlite::{SqliteStore, amount_be, row_to_coin_record};
use crate::traits::CoinStore;
#[cfg(feature = "coin-index")]
use crate::traits::{coin_state_from_record, merge_coin_states_bounded, page_coin_states};
use async_trait::async_trait;
use dg_xch_core::blockchain::coin_record::CoinRecord;
use dg_xch_core::blockchain::sized_bytes::Bytes32;
#[cfg(feature = "coin-index")]
use dg_xch_core::protocols::wallet::{CoinState, CoinStateFilters};
use sqlx::Connection;
use std::sync::atomic::Ordering;

const INSERT_COIN: &str = "INSERT OR REPLACE INTO coin_record \
    (coin_name, confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, timestamp) \
    VALUES (?, ?, 0, ?, ?, ?, ?, ?)";
const SELECT_COIN: &str = "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, \
    amount, timestamp FROM coin_record WHERE coin_name = ?";

// One block's coin deltas, parameterized over the connection: a self-contained transaction from
// `apply_block`, or joined onto an open batch from `apply_block_in` (one fsync per block).
async fn apply_block_on(
    conn: &mut sqlx::SqliteConnection,
    height: u32,
    timestamp: u64,
    additions: &[CoinRecord],
    removals: &[Bytes32],
) -> Result<(), StoreError> {
    for cr in additions {
        sqlx::query(INSERT_COIN)
            .bind(cr.coin.name())
            .bind(i64::from(height))
            .bind(i64::from(cr.coinbase))
            .bind(cr.coin.puzzle_hash)
            .bind(cr.coin.parent_coin_info)
            .bind(amount_be(cr.coin.amount))
            .bind(timestamp as i64)
            .execute(&mut *conn)
            .await?;
    }
    for name in removals {
        sqlx::query("UPDATE coin_record SET spent_index = ? WHERE coin_name = ?")
            .bind(i64::from(height))
            .bind(*name)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

// The fork revert, parameterized over the connection: a self-contained transaction from
// `rollback_to`, or the FIRST statements of the engine's single-transaction reorg from
// `rollback_to_in` (rollback + branch re-applies + peak flip commit as one unit — chia
// blockchain.py `add_block`'s `async with self.block_store.transaction():`).
async fn rollback_to_on(
    conn: &mut sqlx::SqliteConnection,
    fork_height: u32,
) -> Result<u64, StoreError> {
    let deleted = sqlx::query("DELETE FROM coin_record WHERE confirmed_index > ?")
        .bind(i64::from(fork_height))
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let unspent = sqlx::query("UPDATE coin_record SET spent_index = 0 WHERE spent_index > ?")
        .bind(i64::from(fork_height))
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(deleted + unspent)
}

#[async_trait]
impl CoinStore for SqliteStore {
    async fn get_coin_record(&self, coin_name: &Bytes32) -> Result<Option<CoinRecord>, StoreError> {
        self.telemetry.coin_reads.fetch_add(1, Ordering::Relaxed);
        let row = sqlx::query(SELECT_COIN)
            .bind(*coin_name)
            .fetch_optional(&self.read)
            .await?;
        row.as_ref().map(row_to_coin_record).transpose()
    }

    async fn get_coin_records(&self, names: &[Bytes32]) -> Result<Vec<CoinRecord>, StoreError> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        self.telemetry
            .coin_reads
            .fetch_add(names.len() as u64, Ordering::Relaxed);
        // Point-get each name over the primary key rather than one `coin_name IN (...)` statement.
        //
        // A dynamic `IN (...)` list defeats the query planner once it grows past SQLite's
        // search-vs-scan cost crossover: measured on the live mainnet store (WITHOUT ROWID,
        // coin_name PRIMARY KEY), a 2-name list plans as `SEARCH USING PRIMARY KEY` but a 100-name
        // list collapses to a full `SCAN coin_record` — every element compared against every one of
        // millions of rows. On the near-tip mempool/confirm path (`Mempool::check`, one call per
        // spend set) that scan degraded to tens of seconds per call, pinned a WAL read snapshot for
        // its whole duration (blocking checkpoint reset, unbounding the WAL), and froze the confirmed
        // peak until the liveness probe restarted the pod. A single `= ?` lookup can never regress to
        // a scan, so this is stable regardless of table size or planner statistics. All lookups share
        // one pooled read connection: one snapshot, held only for the (now sub-millisecond) batch.
        let mut conn = self.read.acquire().await?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            if let Some(row) = sqlx::query(SELECT_COIN)
                .bind(*name)
                .fetch_optional(&mut *conn)
                .await?
            {
                out.push(row_to_coin_record(&row)?);
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
        let mut guard = self.writer.lock().await;
        let mut tx = guard.begin().await?;
        apply_block_on(&mut tx, height, timestamp, additions, removals).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn apply_block_in(
        &self,
        batch: &mut crate::types::BatchHandle,
        height: u32,
        timestamp: u64,
        additions: &[CoinRecord],
        removals: &[Bytes32],
    ) -> Result<(), StoreError> {
        apply_block_on(batch.sqlite_conn()?, height, timestamp, additions, removals).await
    }

    async fn rollback_to(&self, fork_height: u32) -> Result<u64, StoreError> {
        let mut guard = self.writer.lock().await;
        let mut tx = guard.begin().await?;
        let reverted = rollback_to_on(&mut tx, fork_height).await?;
        tx.commit().await?;
        Ok(reverted)
    }

    async fn rollback_to_in(
        &self,
        batch: &mut crate::types::BatchHandle,
        fork_height: u32,
    ) -> Result<u64, StoreError> {
        rollback_to_on(batch.sqlite_conn()?, fork_height).await
    }

    async fn ensure_reorg_indexes(&self) -> Result<(), StoreError> {
        // The reorg-tier indexes (migration 0006) that back rollback_to's confirmed_index/
        // spent_index predicates. Deferred to `build_indexes` at the sync->tip transition (pure
        // write-amp during forward sync), but a reorg below tip needs them now — so ensure them on
        // the reorg path, idempotently (`IF NOT EXISTS`). One statement per writer-lock acquisition,
        // mirroring `build_indexes`, so confirms interleave rather than stalling behind one guard.
        for stmt in [
            "CREATE INDEX IF NOT EXISTS coin_record_confirmed_index ON coin_record (confirmed_index)",
            "CREATE INDEX IF NOT EXISTS coin_record_spent_index ON coin_record (spent_index)",
        ] {
            let mut guard = self.writer.lock().await;
            sqlx::query(stmt).execute(&mut *guard).await?;
        }
        Ok(())
    }

    #[cfg(feature = "coin-index")]
    async fn get_unspent_by_puzzle_hash(
        &self,
        ph: &Bytes32,
    ) -> Result<Vec<CoinRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE puzzle_hash = ? AND spent_index = 0",
        )
        .bind(*ph)
        .fetch_all(&self.read)
        .await?;
        rows.iter().map(row_to_coin_record).collect()
    }

    #[cfg(feature = "coin-index")]
    async fn get_coins_by_parent(&self, parent: &Bytes32) -> Result<Vec<CoinRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE coin_parent = ?",
        )
        .bind(*parent)
        .fetch_all(&self.read)
        .await?;
        rows.iter().map(row_to_coin_record).collect()
    }

    #[cfg(feature = "coin-index")]
    async fn get_coins_added_at_height(&self, height: u32) -> Result<Vec<CoinRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE confirmed_index = ?",
        )
        .bind(i64::from(height))
        .fetch_all(&self.read)
        .await?;
        rows.iter().map(row_to_coin_record).collect()
    }

    #[cfg(feature = "coin-index")]
    async fn get_coins_removed_at_height(
        &self,
        height: u32,
    ) -> Result<Vec<CoinRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE spent_index = ?",
        )
        .bind(i64::from(height))
        .fetch_all(&self.read)
        .await?;
        rows.iter().map(row_to_coin_record).collect()
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
        // Per-puzzle-hash point query over the `coin_puzzle_hash` index (the same scan-avoidance
        // discipline as get_coin_records), with a running LIMIT budget so the whole reply stays bounded
        // by the caller's `max_items` (chia's max_items parameter, coin_store.py:486-509 — the same
        // decremented-LIMIT-per-batch shape).
        let spent_clause = if include_spent {
            ""
        } else {
            " AND spent_index <= 0"
        };
        let sql = format!(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE puzzle_hash = ? \
             AND (confirmed_index >= ? OR spent_index >= ?){spent_clause} LIMIT ?"
        );
        let mut conn = self.read.acquire().await?;
        let mut out = Vec::new();
        for ph in puzzle_hashes {
            if out.len() >= max_items {
                break;
            }
            let remaining = i64::try_from(max_items - out.len()).unwrap_or(i64::MAX);
            let rows = sqlx::query(&sql)
                .bind(*ph)
                .bind(i64::from(min_height))
                .bind(i64::from(min_height))
                .bind(remaining)
                .fetch_all(&mut *conn)
                .await?;
            for r in &rows {
                out.push(coin_state_from_record(&row_to_coin_record(r)?));
            }
        }
        Ok(out)
    }

    #[cfg(feature = "coin-index")]
    async fn batch_coin_states_by_puzzle_hashes(
        &self,
        puzzle_hashes: &[Bytes32],
        min_height: u32,
        filters: &CoinStateFilters,
        max_items: usize,
    ) -> Result<(Vec<CoinState>, Option<u32>), StoreError> {
        // chia coin_store.py:610-633: nothing requested, or filters that admit nothing, finish empty.
        if puzzle_hashes.is_empty() || (!filters.include_spent && !filters.include_unspent) {
            return Ok((Vec::new(), None));
        }
        // chia's spent/unspent predicates (coin_store.py:621-630) and the >= min_amount filter
        // (:623, amount stored as an 8-byte big-endian blob, so bytewise >= IS numeric >= — the
        // zero blob makes the predicate a no-op exactly like chia's omitted filter).
        let height_filter = match (filters.include_spent, filters.include_unspent) {
            (true, true) => "",
            (true, false) => " AND spent_index > 0",
            (false, true) => " AND spent_index <= 0",
            (false, false) => unreachable!("handled above"),
        };
        // Per-puzzle-hash point probes over the coin_puzzle_hash index (the same scan-avoidance
        // discipline as get_coin_records: a long dynamic IN-list collapses the planner to a full
        // scan). Each probe is ORDERED by activity height and LIMITed to max_items + 1 IN SQL —
        // chia's single-query `ORDER BY MAX(confirmed_index, spent_index) ASC LIMIT max_items + 1`
        // (coin_store.py:635-648) applied per hash, with the bounded merge keeping only the
        // smallest max_items + 1 overall.
        let sql = format!(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE puzzle_hash = ? \
             AND (confirmed_index >= ? OR spent_index >= ?){height_filter} AND amount >= ? \
             ORDER BY MAX(confirmed_index, spent_index) ASC LIMIT ?"
        );
        // The hint join (chia coin_store.py:655-671): coins whose HINT is one of the requested
        // hashes — the CAT/NFT discovery read. Same ordering + limit + filters.
        #[cfg(feature = "hint")]
        let hint_sql = format!(
            "SELECT confirmed_index, spent_index, coinbase, puzzle_hash, coin_parent, amount, \
             timestamp FROM coin_record WHERE coin_name IN \
             (SELECT coin_name FROM coin_hint WHERE hint = ?) \
             AND (confirmed_index >= ? OR spent_index >= ?){height_filter} AND amount >= ? \
             ORDER BY MAX(confirmed_index, spent_index) ASC LIMIT ?"
        );
        let limit = i64::try_from(max_items.saturating_add(1)).unwrap_or(i64::MAX);
        let min_amount = amount_be(filters.min_amount);
        let mut conn = self.read.acquire().await?;
        let mut merged = std::collections::HashMap::new();
        for ph in puzzle_hashes {
            let rows = sqlx::query(&sql)
                .bind(*ph)
                .bind(i64::from(min_height))
                .bind(i64::from(min_height))
                .bind(min_amount.clone())
                .bind(limit)
                .fetch_all(&mut *conn)
                .await?;
            let states = rows
                .iter()
                .map(|r| Ok(coin_state_from_record(&row_to_coin_record(r)?)))
                .collect::<Result<Vec<_>, StoreError>>()?;
            merge_coin_states_bounded(&mut merged, states, max_items + 1);
            #[cfg(feature = "hint")]
            if filters.include_hinted {
                let rows = sqlx::query(&hint_sql)
                    .bind(*ph)
                    .bind(i64::from(min_height))
                    .bind(i64::from(min_height))
                    .bind(min_amount.clone())
                    .bind(limit)
                    .fetch_all(&mut *conn)
                    .await?;
                let states = rows
                    .iter()
                    .map(|r| Ok(coin_state_from_record(&row_to_coin_record(r)?)))
                    .collect::<Result<Vec<_>, StoreError>>()?;
                merge_coin_states_bounded(&mut merged, states, max_items + 1);
            }
        }
        Ok(page_coin_states(merged, max_items))
    }

    #[cfg(feature = "hint")]
    async fn get_coins_for_hint(
        &self,
        hint: &Bytes32,
        max_items: usize,
    ) -> Result<Vec<Bytes32>, StoreError> {
        use sqlx::Row;
        // LIMIT in the query, like chia's hint store (hint_store.py:26/42) — never fetch-then-truncate.
        let rows = sqlx::query("SELECT coin_name FROM coin_hint WHERE hint = ? LIMIT ?")
            .bind(*hint)
            .bind(i64::try_from(max_items).unwrap_or(i64::MAX))
            .fetch_all(&self.read)
            .await?;
        rows.iter()
            .map(|r| Ok(r.try_get::<Bytes32, _>("coin_name")?))
            .collect()
    }

    async fn apply_hints_in(
        &self,
        batch: &mut crate::types::BatchHandle,
        pairs: &[(Bytes32, Bytes32)],
    ) -> Result<(), StoreError> {
        #[cfg(feature = "hint")]
        {
            apply_hints_on(batch.sqlite_conn()?, pairs).await
        }
        #[cfg(not(feature = "hint"))]
        {
            let _ = (batch, pairs);
            Ok(())
        }
    }

    async fn apply_hints(&self, pairs: &[(Bytes32, Bytes32)]) -> Result<(), StoreError> {
        #[cfg(feature = "hint")]
        {
            let mut guard = self.writer.lock().await;
            let mut tx = guard.begin().await?;
            apply_hints_on(&mut tx, pairs).await?;
            tx.commit().await?;
            Ok(())
        }
        #[cfg(not(feature = "hint"))]
        {
            let _ = pairs;
            Ok(())
        }
    }
}

// One block's create-coin hints, parameterized over the connection: joined onto the block's open
// batch from `apply_hints_in`, or its own transaction from `apply_hints`. `INSERT OR IGNORE` keeps
// re-apply/replay idempotent against the `(hint, coin_name)` primary key.
#[cfg(feature = "hint")]
async fn apply_hints_on(
    conn: &mut sqlx::SqliteConnection,
    pairs: &[(Bytes32, Bytes32)],
) -> Result<(), StoreError> {
    for (hint, coin_name) in pairs {
        sqlx::query("INSERT OR IGNORE INTO coin_hint (hint, coin_name) VALUES (?, ?)")
            .bind(*hint)
            .bind(*coin_name)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}
