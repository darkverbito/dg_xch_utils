CREATE TABLE IF NOT EXISTS coin_hint (
    hint      BLOB NOT NULL,
    coin_name BLOB NOT NULL,
    PRIMARY KEY (hint, coin_name)
) WITHOUT ROWID;

-- The coin_hint_coin_name secondary is SERVICE tier and phases with the coin_record service
-- indexes (see the postgres twin): created by build_indexes at the sync->tip transition,
-- dropped by shed_service_indexes on a deep fall-behind. Not created here — an at-open create
-- would silently rebuild it on a restart mid-catch-up after a shed.
