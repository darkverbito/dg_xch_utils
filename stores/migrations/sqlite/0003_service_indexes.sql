CREATE INDEX IF NOT EXISTS coin_record_puzzle_hash ON coin_record (puzzle_hash);
CREATE INDEX IF NOT EXISTS coin_record_coin_parent ON coin_record (coin_parent);
CREATE INDEX IF NOT EXISTS coin_record_unspent_by_ph ON coin_record (puzzle_hash) WHERE spent_index = 0;
