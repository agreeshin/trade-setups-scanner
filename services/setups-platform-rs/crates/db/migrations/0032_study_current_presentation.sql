-- One replaceable derived presentation per retained study. Original science,
-- artifacts and queued delivery bytes have independent immutable owners.
CREATE TABLE study_current_presentations (
    study_id bigint PRIMARY KEY REFERENCES optimization_studies(id) ON DELETE CASCADE,
    owner_id text NOT NULL,
    source_identity text NOT NULL CHECK(length(source_identity)=32),
    snapshot jsonb CHECK(jsonb_typeof(snapshot)='object' AND octet_length(snapshot::text)<=32768),
    numerical jsonb CHECK(jsonb_typeof(numerical)='object' AND octet_length(numerical::text)<=1048576),
    png bytea CHECK(octet_length(png) BETWEEN 1 AND 262144),
    png_sha256 text CHECK(length(png_sha256)=64),
    error text CHECK(length(error)<=128),
    CHECK ((snapshot IS NULL AND numerical IS NULL AND png IS NULL AND png_sha256 IS NULL AND error IS NOT NULL)
        OR (snapshot IS NOT NULL AND numerical IS NOT NULL AND png IS NOT NULL AND png_sha256 IS NOT NULL AND error IS NULL)),
    CHECK(snapshot IS NULL OR ((snapshot->>'revision')::bigint>0
        AND snapshot->>'source' IN ('initial','actual') AND (snapshot->>'observationCutoff')::bigint>=0))
);

-- Additive future manual source capture includes <=3661 scientific UTC weights;
-- existing immutable archive rows are neither rewritten nor reconstructed.
ALTER TABLE study_manual_original_outcomes DROP CONSTRAINT study_manual_original_outcomes_source_check;
ALTER TABLE study_manual_original_outcomes ADD CONSTRAINT study_manual_original_outcomes_source_check
    CHECK(jsonb_typeof(source)='object' AND octet_length(source::text)<=1048576);

-- New v2 admissions own a bounded base64 copy of the selected current PNG.
-- Existing reservations/payloads are not recalculated or rewritten.
ALTER TABLE telegram_delivery_outbox DROP CONSTRAINT telegram_delivery_outbox_payload_check;
ALTER TABLE telegram_delivery_outbox ADD CONSTRAINT telegram_delivery_outbox_payload_check
    CHECK(octet_length(payload::text)<=CASE WHEN payload->>'chartPresentation'='study_current_v1' THEN 393216 ELSE 32768 END);
