CREATE TABLE IF NOT EXISTS block_record (
    header_hash          BYTEA  NOT NULL PRIMARY KEY,
    prev_hash            BYTEA  NOT NULL,
    height               BIGINT NOT NULL,
    weight               BYTEA  NOT NULL,
    total_iters          BYTEA  NOT NULL,
    in_main_chain        BIGINT NOT NULL DEFAULT 0,
    is_transaction_block BIGINT NOT NULL,
    sub_epoch_summary    BYTEA,
    status               BIGINT NOT NULL DEFAULT 0,
    record               BYTEA  NOT NULL
);

CREATE INDEX IF NOT EXISTS block_record_height_main ON block_record (height) WHERE in_main_chain = 1;
CREATE INDEX IF NOT EXISTS block_record_prev ON block_record (prev_hash);

CREATE TABLE IF NOT EXISTS block_body (
    header_hash BYTEA NOT NULL PRIMARY KEY,
    body        BYTEA NOT NULL,
    FOREIGN KEY (header_hash) REFERENCES block_record (header_hash)
);

CREATE TABLE IF NOT EXISTS current_peak (
    id          BIGINT NOT NULL PRIMARY KEY CHECK (id = 0),
    header_hash BYTEA  NOT NULL,
    height      BIGINT NOT NULL
);
