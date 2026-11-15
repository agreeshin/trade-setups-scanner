-- Formula version 2 is unchanged. Original identities/rows/references are immutable.
ALTER TABLE study_expectancy_artifacts ADD COLUMN revision_key text NOT NULL DEFAULT 'original'
    CHECK (revision_key='original' OR revision_key ~ '^[0-9a-f]{64}$');
DO $$
DECLARE original_unique text;
BEGIN
    SELECT conname INTO STRICT original_unique FROM pg_constraint
      WHERE conrelid='study_expectancy_artifacts'::regclass AND contype='u'
      AND pg_get_constraintdef(oid)='UNIQUE (owner_id, source_generation_id, source_study_id, source_slot_id, tp_pct, sl_pct, calculation_version)';
    EXECUTE format('ALTER TABLE study_expectancy_artifacts DROP CONSTRAINT %I', original_unique);
END;
$$;
ALTER TABLE study_expectancy_artifacts ADD CONSTRAINT study_expectancy_revision_unique
    UNIQUE(owner_id,source_generation_id,source_study_id,source_slot_id,tp_pct,sl_pct,calculation_version,revision_key);

-- Bounded operation evidence, including mismatches that cannot be published.
-- No generation FK: immutable provenance survives confirmed science cleanup.
CREATE TABLE study_expectancy_recalculations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id text NOT NULL CHECK(length(owner_id) BETWEEN 1 AND 120),
    generation_id bigint NOT NULL,
    study_id bigint NOT NULL,
    slot_id bigint NOT NULL,
    config_revision bigint NOT NULL,
    revision_key text NOT NULL CHECK(revision_key ~ '^[0-9a-f]{64}$'),
    original_artifact_id bigint NOT NULL REFERENCES study_expectancy_artifacts(id),
    artifact_id bigint REFERENCES study_expectancy_artifacts(id) ON DELETE CASCADE,
    provenance jsonb NOT NULL CHECK(jsonb_typeof(provenance)='object' AND octet_length(provenance::text)<=262144),
    comparison jsonb NOT NULL CHECK(jsonb_typeof(comparison)='object' AND octet_length(comparison::text)<=65536),
    matched boolean NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(owner_id,generation_id,revision_key),
    CHECK(matched=(artifact_id IS NOT NULL))
);
CREATE TRIGGER study_expectancy_recalculation_no_update BEFORE UPDATE ON study_expectancy_recalculations
    FOR EACH ROW EXECUTE FUNCTION study_expectancy_immutable();

-- New readers alone consult this binding. Staging never changes live selection.
CREATE TABLE study_expectancy_publications (
    owner_id text NOT NULL,
    generation_id bigint NOT NULL,
    slot_id bigint NOT NULL,
    recalculation_id bigint NOT NULL REFERENCES study_expectancy_recalculations(id) ON DELETE CASCADE,
    artifact_id bigint NOT NULL REFERENCES study_expectancy_artifacts(id) ON DELETE CASCADE,
    available_from_unix bigint NOT NULL CHECK(available_from_unix>=0),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(owner_id,generation_id)
);
