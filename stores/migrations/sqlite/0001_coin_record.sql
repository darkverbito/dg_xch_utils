-- Secondary indexes live in 0006_reorg_indexes.sql / 0003_service_indexes.sql and are built once
-- at the sync->tip transition (BlockStore::build_indexes) — never during bulk sync, where they
-- are pure write-amplification (each row insert would otherwise maintain every secondary index).
CREATE TABLE IF NOT EXISTS coin_record (
    coin_name       BLOB    NOT NULL PRIMARY KEY,
    confirmed_index INTEGER NOT NULL,
    spent_index     INTEGER NOT NULL,
    coinbase        INTEGER NOT NULL,
    puzzle_hash     BLOB    NOT NULL,
    coin_parent     BLOB    NOT NULL,
    amount          BLOB    NOT NULL,
    timestamp       INTEGER NOT NULL
) WITHOUT ROWID;
