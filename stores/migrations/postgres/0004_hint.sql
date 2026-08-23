CREATE TABLE IF NOT EXISTS coin_hint (
    hint      BYTEA NOT NULL,
    coin_name BYTEA NOT NULL,
    PRIMARY KEY (hint, coin_name)
);

CREATE INDEX IF NOT EXISTS coin_hint_coin_name ON coin_hint (coin_name);
