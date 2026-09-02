-- Secondary indexes live in 0006_reorg_indexes.sql / 0003_service_indexes.sql and are built once
-- at the sync->tip transition (BlockStore::build_indexes) — never during bulk sync, where they
-- are pure write-amplification (each row insert would otherwise maintain every secondary index).
CREATE TABLE IF NOT EXISTS coin_record (
    coin_name       BYTEA  NOT NULL PRIMARY KEY,
    confirmed_index BIGINT NOT NULL,
    spent_index     BIGINT NOT NULL,
    coinbase        BIGINT NOT NULL,
    puzzle_hash     BYTEA  NOT NULL,
    coin_parent     BYTEA  NOT NULL,
    amount          BYTEA  NOT NULL,
    timestamp       BIGINT NOT NULL
);
