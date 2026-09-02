-- Persisted weight-proof challenge segments, keyed by the ses-carrying block's header hash.
-- Mirror of chia's table (chia/full_node/block_store.py:85-88), BYTEA for BLOB as elsewhere:
--   CREATE TABLE IF NOT EXISTS sub_epoch_segments_v3(
--       ses_block_hash blob PRIMARY KEY,
--       challenge_segments blob)
-- challenge_segments holds the ChiaSerialize bytes of a SubEpochSegments wrapper
-- (block_store.py:170 stores bytes(SubEpochSegments(segments))).
CREATE TABLE IF NOT EXISTS sub_epoch_segments_v3 (
    ses_block_hash     BYTEA NOT NULL PRIMARY KEY,
    challenge_segments BYTEA NOT NULL
);
