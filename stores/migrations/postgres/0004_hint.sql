CREATE TABLE IF NOT EXISTS coin_hint (
    hint      BYTEA NOT NULL,
    coin_name BYTEA NOT NULL,
    PRIMARY KEY (hint, coin_name)
);

-- The coin_hint_coin_name secondary is SERVICE tier (zero scans measured on every live leg;
-- wallet-only at tip) and phases with the coin_record service indexes: created by
-- build_indexes at the sync->tip transition, dropped by shed_service_indexes on a deep
-- fall-behind. Not created here — an at-open create would silently rebuild it on a restart
-- mid-catch-up after a shed.
