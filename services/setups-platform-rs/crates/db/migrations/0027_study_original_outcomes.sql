-- A bounded native CLOSE-entry outcome sidecar, independent of disposable frozen chunks.
-- The original numerical artifact remains byte-stable, including its UTC daily model.
CREATE TABLE study_original_outcomes (
    artifact_id bigint PRIMARY KEY REFERENCES study_expectancy_artifacts(id) ON DELETE CASCADE,
    source_key text NOT NULL CHECK(length(source_key)=64),
    kernel text NOT NULL CHECK(kernel='original-historical-close-1'),
    fidelity text NOT NULL DEFAULT 'native_original' CHECK(fidelity IN ('native_original','verified_original_reconstruction')),
    evidence jsonb NOT NULL DEFAULT '{}' CHECK(jsonb_typeof(evidence)='object' AND octet_length(evidence::text)<=32768),
    evidence_sha256 text CHECK(length(evidence_sha256)=64),
    minutes jsonb NOT NULL CHECK(jsonb_typeof(minutes)='array' AND jsonb_array_length(minutes)<=100000
        AND octet_length(minutes::text)<=33554432),
    payload_sha256 text NOT NULL CHECK(length(payload_sha256)=64),
    payload_bytes bigint NOT NULL CHECK(payload_bytes BETWEEN 0 AND 33554432),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK((fidelity='native_original' AND evidence='{}' AND evidence_sha256 IS NULL)
       OR (fidelity='verified_original_reconstruction' AND evidence<>'{}' AND evidence_sha256 IS NOT NULL))
);
CREATE FUNCTION study_original_outcomes_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'study_original_outcomes_immutable';
END;
$$;
CREATE TRIGGER study_original_outcomes_no_update BEFORE UPDATE ON study_original_outcomes
    FOR EACH ROW EXECUTE FUNCTION study_original_outcomes_immutable();

-- PostgreSQL's default chooses the later occurrence of an ambiguous midnight.
-- The browser grid chooses the earliest. Test a candidate using the preceding
-- day's offset; accept it ONLY when it maps back to this exact local midnight.
CREATE FUNCTION study_local_day_start(zone text, local_day date) RETURNS bigint
LANGUAGE sql STABLE STRICT PARALLEL SAFE AS $$
    WITH nominal AS (
        SELECT local_day::timestamp AS local_midnight,
               local_day::timestamp AT TIME ZONE zone AS at
    ), candidates AS (
        SELECT *, at + (timezone(zone, at) - timezone('UTC', at))
                     - (timezone(zone, at - interval '1 day') - timezone('UTC', at - interval '1 day')) AS earlier
          FROM nominal
    )
    SELECT extract(epoch FROM CASE WHEN earlier < at AND timezone(zone, earlier)=local_midnight
                                   THEN earlier ELSE at END)::bigint FROM candidates
$$;
