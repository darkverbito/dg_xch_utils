-- Per-table autovacuum: the global default (scale_factor 0.2) lets millions of dead tuples
-- accumulate at coin_record scale before a vacuum triggers, and confirm latency degrades with
-- them (e.g. millions of dead tuples against tens of millions live with autovacuum nominally
-- healthy). 0.02 plus a count floor vacuums at
-- roughly every 2% of table growth. ALTER TABLE ... SET is idempotent; replayed on every open.
ALTER TABLE coin_record SET (
    autovacuum_vacuum_scale_factor  = 0.02,
    autovacuum_vacuum_threshold     = 1000,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_analyze_threshold    = 1000
);
ALTER TABLE block_record SET (
    autovacuum_vacuum_scale_factor  = 0.02,
    autovacuum_vacuum_threshold     = 1000,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_analyze_threshold    = 1000
);
