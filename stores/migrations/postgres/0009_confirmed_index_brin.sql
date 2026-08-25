-- coin_record_confirmed_index btree -> BRIN conversion for legs that predate the BRIN decision.
--
-- 0006/ensure_reorg_indexes specify USING BRIN: confirmed_index is assigned in strictly
-- block-sequential (append) order, so the column is ~0.98 heap-correlated and a BRIN serves the
-- rollback's "confirmed_index > $1" range in kilobytes while being near-free to maintain per
-- insert. But legs whose index was built under the old 0001 schema carry a multi-GB BTREE under
-- the same name (measured live: 3.0-4.0 GB per leg), maintained on EVERY coin insert — and both
-- creation paths use IF NOT EXISTS, which silently keeps the stale btree forever.
--
-- This migration is NOT run at open, and NOT split-executed as a whole: the swap must be
-- guarded (convert only when the live index is a stale btree or an invalid leftover, so
-- BRIN-native legs never pay a heap scan) and built CONCURRENTLY (which cannot run inside a
-- transaction, so it cannot live in a DO block). The Rust driver
-- (postgres::block::convert_confirmed_index_to_brin, invoked from build_indexes at the
-- sync->tip rising edge and from shed_service_indexes at the deep-fall falling edge) checks
-- pg_class/pg_am/pg_index first, then executes the statements below one at a time on
-- autocommit, then swaps transactionally:
--
--   1. (below) drop any leftover temp from an interrupted earlier conversion — an interrupted
--      CREATE INDEX CONCURRENTLY leaves an INVALID index behind, which must be cleared, never
--      reused;
--   2. (below) build the replacement BRIN CONCURRENTLY under the temp name — online, no write
--      lock, confirms continue underneath;
--   3. (driver) in ONE transaction: DROP INDEX coin_record_confirmed_index;
--      ALTER INDEX coin_record_confirmed_index_brin RENAME TO coin_record_confirmed_index.
--      Catalog-only, momentary ACCESS EXCLUSIVE; a crash anywhere leaves either the old btree
--      (plus at worst a temp the next run clears) or the finished BRIN — never neither.

DROP INDEX CONCURRENTLY IF EXISTS coin_record_confirmed_index_brin;
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_confirmed_index_brin ON coin_record USING BRIN (confirmed_index);
