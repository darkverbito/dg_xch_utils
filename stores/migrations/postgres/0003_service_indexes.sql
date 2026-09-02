-- Service-tier lookups (RPC by puzzle hash / parent). Built at the sync->tip transition via
-- build_indexes, one autocommit statement per index: CONCURRENTLY cannot run inside a
-- transaction and never takes a write-blocking lock, so the confirm writer is never stalled.
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_puzzle_hash ON coin_record (puzzle_hash);
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_coin_parent ON coin_record (coin_parent);
CREATE INDEX CONCURRENTLY IF NOT EXISTS coin_record_unspent_by_ph ON coin_record (puzzle_hash) WHERE spent_index = 0;
