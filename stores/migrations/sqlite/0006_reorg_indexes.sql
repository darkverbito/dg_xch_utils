-- Reorg-speed indexes (see the postgres twin): deferred to build_indexes at the sync->tip
-- transition. Never maintained during bulk sync.
CREATE INDEX IF NOT EXISTS coin_record_confirmed_index ON coin_record (confirmed_index);
CREATE INDEX IF NOT EXISTS coin_record_spent_index ON coin_record (spent_index);
