CREATE TABLE IF NOT EXISTS coin_hint (
    hint      BLOB NOT NULL,
    coin_name BLOB NOT NULL,
    PRIMARY KEY (hint, coin_name)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS coin_hint_coin_name ON coin_hint (coin_name);
