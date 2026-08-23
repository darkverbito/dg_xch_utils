-- Reorg-speed indexes: rollback_to prunes additions by "confirmed_index > $1" and un-spends
-- removals by "spent_index > $1". A tip-following node wants them for reorgs; a bulk sync never
-- reads them (measured zero scans against 65-78M PK scans across three live legs) — so they are
-- built by build_indexes at the sync->tip transition, not at open.
--
-- Index TYPE is chosen per Postgres access pattern, not a one-size btree:
--   * confirmed_index is assigned in strictly block-sequential (append) order, so the column is
--     almost perfectly correlated with physical heap order (~0.98 measured). A BRIN index serves
--     the "confirmed_index > $1" range in kilobytes and is near-free to maintain on insert, versus
--     a multi-GB btree whose random-free maintenance is not needed here (the column is never
--     point-looked-up by an unindexed path — coin lookups go through the coin_name key).
--   * spent_index is NOT heap-correlated (a coin created early can be spent much later), so it
--     keeps a btree.
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_confirmed_index ON coin_record USING BRIN (confirmed_index);
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_spent_index ON coin_record (spent_index);
