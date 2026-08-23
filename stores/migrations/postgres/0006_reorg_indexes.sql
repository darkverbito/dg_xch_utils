-- Reorg-speed indexes: rollback_to prunes additions by "confirmed_index > $1" and un-spends
-- removals by "spent_index > $1". A tip-following node wants them for reorgs; a bulk sync never
-- reads them (measured zero scans against 65-78M PK scans across three live legs) — so they are
-- built by build_indexes at the sync->tip transition, not at open.
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_confirmed_index ON coin_record (confirmed_index);
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_spent_index ON coin_record (spent_index);
