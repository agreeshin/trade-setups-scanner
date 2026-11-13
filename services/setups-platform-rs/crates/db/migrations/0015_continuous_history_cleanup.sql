-- Protected historical outputs outlive explicitly deleted research. These are
-- immutable output provenance/results, not complete generation tombstones.
ALTER TABLE replay_batches ADD COLUMN continuous_archive jsonb
    CHECK (octet_length(continuous_archive::text) <= 16384);
ALTER TABLE replay_batches ADD COLUMN continuous_analytics_archive jsonb
    CHECK (octet_length(continuous_analytics_archive::text) <= 8388608);
ALTER TABLE replay_batches ADD COLUMN continuous_analytics_context_archive jsonb
    CHECK (octet_length(continuous_analytics_context_archive::text) <= 16384);
ALTER TABLE replay_batches ADD COLUMN continuous_replay_archive jsonb
    CHECK (octet_length(continuous_replay_archive::text) <= 1048576);
ALTER TABLE replay_runs ADD COLUMN continuous_archive jsonb
    CHECK (octet_length(continuous_archive::text) <= 16384);
ALTER TABLE live_setups ADD COLUMN adaptive_generation_archive jsonb
    CHECK (octet_length(adaptive_generation_archive::text) <= 16384);
