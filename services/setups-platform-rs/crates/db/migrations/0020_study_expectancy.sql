-- Derived numbers are independent of scientific results and disposable frozen input.
-- Original IDs are immutable provenance, not cascading foreign keys: confirmed
-- history cleanup retains completed replay sets and published recommendations.
CREATE TABLE study_expectancy_artifacts (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    artifact_key text NOT NULL UNIQUE CHECK(length(artifact_key)=64),
    owner_id text NOT NULL CHECK(length(owner_id) BETWEEN 1 AND 120),
    source_generation_id bigint NOT NULL,
    source_study_id bigint NOT NULL,
    source_slot_id bigint NOT NULL,
    calculation_version text NOT NULL CHECK(length(calculation_version) BETWEEN 1 AND 64),
    tp_pct double precision NOT NULL CHECK(tp_pct>0 AND tp_pct<'Infinity'::float8),
    sl_pct double precision NOT NULL CHECK(sl_pct>0 AND sl_pct<'Infinity'::float8),
    identity jsonb NOT NULL CHECK(jsonb_typeof(identity)='object' AND octet_length(identity::text)<=32768),
    numerical_snapshot jsonb NOT NULL CHECK(jsonb_typeof(numerical_snapshot)='object' AND octet_length(numerical_snapshot::text)<=1048576),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(owner_id,source_generation_id,source_study_id,source_slot_id,tp_pct,sl_pct,calculation_version)
);
CREATE INDEX study_expectancy_by_generation ON study_expectancy_artifacts(owner_id,source_generation_id);

-- Optional image is generated later from numerical_snapshot; one copy per
-- immutable numeric artifact and renderer version. The renderer validates PNG.
CREATE TABLE study_expectancy_pngs (
    artifact_id bigint NOT NULL REFERENCES study_expectancy_artifacts(id) ON DELETE CASCADE,
    rendering_version text NOT NULL CHECK(length(rendering_version) BETWEEN 1 AND 64),
    image_key jsonb NOT NULL CHECK(jsonb_typeof(image_key)='object' AND octet_length(image_key::text)<=4096),
    png bytea NOT NULL CHECK(octet_length(png) BETWEEN 33 AND 262144),
    sha256 text NOT NULL CHECK(length(sha256)=64),
    width integer NOT NULL DEFAULT 900 CHECK(width=900),
    height integer NOT NULL DEFAULT 320 CHECK(height=320),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(artifact_id,rendering_version)
);

-- Small durable render claims, independent of raw input and live commit.
-- Runtime admits one CPU render; expired claims are retried, at most 3 attempts.
CREATE TABLE study_expectancy_render_attempts (
    artifact_id bigint NOT NULL REFERENCES study_expectancy_artifacts(id) ON DELETE CASCADE,
    render_version integer NOT NULL CHECK(render_version>0),
    attempts smallint NOT NULL CHECK(attempts BETWEEN 1 AND 3),
    lease_owner text CHECK(length(lease_owner)<=200),
    lease_token bigint NOT NULL DEFAULT 1,
    lease_until timestamptz,
    next_attempt_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    error text CHECK(length(error)<=256),
    PRIMARY KEY(artifact_id,render_version)
);

-- These real owners protect both history and pending jobs (which reference a
-- live_setup). No TTL evicts a referenced artifact; no copy in each recommendation.
ALTER TABLE continuous_runtime_artifacts ADD COLUMN gate_expectancy_required boolean NOT NULL DEFAULT false;
ALTER TABLE continuous_generations ADD COLUMN expectancy_artifact_id bigint REFERENCES study_expectancy_artifacts(id);
ALTER TABLE replay_batches ADD COLUMN expectancy_artifact_id bigint REFERENCES study_expectancy_artifacts(id);
ALTER TABLE live_setups ADD COLUMN expectancy_artifact_id bigint REFERENCES study_expectancy_artifacts(id);
CREATE INDEX continuous_expectancy_reference ON continuous_generations(expectancy_artifact_id) WHERE expectancy_artifact_id IS NOT NULL;
CREATE INDEX replay_expectancy_reference ON replay_batches(expectancy_artifact_id) WHERE expectancy_artifact_id IS NOT NULL;
CREATE INDEX live_expectancy_reference ON live_setups(expectancy_artifact_id) WHERE expectancy_artifact_id IS NOT NULL;

-- Only NEW setup references are attached. Never rewrite an old recommendation
-- or already pending delivery body when a legacy profile gains enrichment.
CREATE FUNCTION live_setup_expectancy_reference() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.adaptive_generation_id IS NOT NULL AND NEW.expectancy_artifact_id IS NULL THEN
        SELECT expectancy_artifact_id INTO NEW.expectancy_artifact_id
          FROM continuous_generations WHERE id=NEW.adaptive_generation_id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER live_setup_attach_expectancy BEFORE INSERT ON live_setups FOR EACH ROW EXECUTE FUNCTION live_setup_expectancy_reference();

CREATE FUNCTION study_expectancy_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'study expectancy artifacts are immutable; use a new version' USING ERRCODE='23514';
END;
$$;
CREATE TRIGGER study_expectancy_no_update BEFORE UPDATE ON study_expectancy_artifacts FOR EACH ROW EXECUTE FUNCTION study_expectancy_immutable();
CREATE TRIGGER study_expectancy_png_no_update BEFORE UPDATE ON study_expectancy_pngs FOR EACH ROW EXECUTE FUNCTION study_expectancy_immutable();
