-- Manual one-shot history is owned by the retained study, never a fabricated
-- continuous generation/slot or a disposable frozen-input row.
CREATE TABLE study_manual_original_outcomes (
    study_id bigint PRIMARY KEY REFERENCES optimization_studies(id) ON DELETE CASCADE,
    owner_id text NOT NULL,
    source_key text NOT NULL CHECK(length(source_key)=64),
    study_identity text NOT NULL CHECK(length(study_identity)=32),
    kernel text NOT NULL CHECK(kernel='original-manual-close-1'),
    source jsonb NOT NULL CHECK(jsonb_typeof(source)='object' AND octet_length(source::text)<=65536),
    minutes jsonb NOT NULL CHECK(jsonb_typeof(minutes)='array' AND jsonb_array_length(minutes)<=100000
        AND octet_length(minutes::text)<=33554432),
    payload_sha256 text NOT NULL CHECK(length(payload_sha256)=64),
    payload_bytes bigint NOT NULL CHECK(payload_bytes BETWEEN 0 AND 33554432),
    utc_daily jsonb,
    utc_daily_sha256 text,
    utc_source_payload_sha256 text,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TRIGGER study_manual_original_outcomes_no_update BEFORE UPDATE ON study_manual_original_outcomes
    FOR EACH ROW EXECUTE FUNCTION study_original_outcomes_immutable();

-- Optional immutable materialized UTC projection. Legacy archives stay byte-
-- stable and use the bounded read-only resolver when this projection is absent.
ALTER TABLE study_original_outcomes ADD COLUMN utc_daily jsonb,
    ADD COLUMN utc_daily_sha256 text, ADD COLUMN utc_source_payload_sha256 text;
ALTER TABLE study_original_outcomes ADD CONSTRAINT original_utc_projection CHECK (
    (utc_daily IS NULL AND utc_daily_sha256 IS NULL AND utc_source_payload_sha256 IS NULL) OR
    (utc_daily IS NOT NULL AND utc_daily_sha256 IS NOT NULL AND utc_source_payload_sha256 IS NOT NULL
     AND jsonb_typeof(utc_daily)='array' AND jsonb_array_length(utc_daily)<=7322
     AND octet_length(utc_daily::text)<=4194304 AND length(utc_daily_sha256)=64
     AND utc_source_payload_sha256=payload_sha256));
ALTER TABLE study_manual_original_outcomes ADD CONSTRAINT manual_original_utc_projection CHECK (
    (utc_daily IS NULL AND utc_daily_sha256 IS NULL AND utc_source_payload_sha256 IS NULL) OR
    (utc_daily IS NOT NULL AND utc_daily_sha256 IS NOT NULL AND utc_source_payload_sha256 IS NOT NULL
     AND jsonb_typeof(utc_daily)='array' AND jsonb_array_length(utc_daily)<=7322
     AND octet_length(utc_daily::text)<=4194304 AND length(utc_daily_sha256)=64
     AND utc_source_payload_sha256=payload_sha256));
