-- One replaceable, explicitly owned retrospective pair per study. Not live data.
ALTER TABLE optimization_studies ADD COLUMN execution_started_at timestamptz;
ALTER TABLE replay_batches ADD COLUMN actual_study_id bigint REFERENCES optimization_studies(id);
ALTER TABLE replay_batches ADD COLUMN actual_revision bigint;
ALTER TABLE replay_batches ADD COLUMN actual_job_id text;
ALTER TABLE replay_runs ADD COLUMN actual_study_id bigint REFERENCES optimization_studies(id);
ALTER TABLE replay_runs ADD COLUMN actual_revision bigint;
ALTER TABLE replay_runs ADD COLUMN actual_job_id text;
ALTER TABLE replay_runs DROP CONSTRAINT replay_runs_origin_check;
ALTER TABLE replay_runs ADD CONSTRAINT replay_runs_origin_check CHECK (origin IN ('manual','continuous','study_actual'));
ALTER TABLE replay_batches DROP CONSTRAINT replay_batches_origin_check;
ALTER TABLE replay_batches ADD CONSTRAINT replay_batches_origin_check CHECK (origin IN ('manual','continuous','study_actual'));
CREATE TABLE study_actualizations (
    owner_id text NOT NULL,
    study_id bigint NOT NULL REFERENCES optimization_studies(id),
    job_id text NOT NULL UNIQUE,
    revision bigint NOT NULL CHECK (revision>0),
    token bigint NOT NULL DEFAULT 0,
    status text NOT NULL CHECK (status IN ('queued','running','completed','failed')),
    stage text NOT NULL,
    execution_started_at timestamptz NOT NULL,
    observed_through_unix bigint NOT NULL CHECK (observed_through_unix%60=0),
    source jsonb NOT NULL CHECK (octet_length(source::text)<=1048576),
    source_identity text NOT NULL,
    progress jsonb NOT NULL DEFAULT '{}',
    report jsonb CHECK (octet_length(report::text)<=4194304),
    totals jsonb,
    error jsonb,
    batch_id integer REFERENCES replay_batches(id),
    work_minutes bigint NOT NULL CHECK (work_minutes>=0),
    reserved_bytes bigint NOT NULL CHECK (reserved_bytes>=0),
    requested_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    started_at timestamptz,
    completed_at timestamptz,
    PRIMARY KEY(owner_id,study_id),
    CHECK ((status='completed')=(report IS NOT NULL)),
    CHECK (status IN ('queued','running') OR reserved_bytes=0)
);
CREATE INDEX study_actualizations_queue ON study_actualizations(requested_at) WHERE status='queued';
CREATE INDEX study_actualizations_source ON study_actualizations(study_id);
CREATE INDEX replay_actual_run_source ON replay_runs(actual_study_id) WHERE actual_study_id IS NOT NULL;
CREATE INDEX replay_actual_batch_source ON replay_batches(actual_study_id) WHERE actual_study_id IS NOT NULL;
CREATE TABLE study_actual_setups (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id text NOT NULL,
    study_id bigint NOT NULL,
    revision bigint NOT NULL,
    run_id integer NOT NULL REFERENCES replay_runs(id),
    signal_close_unix bigint NOT NULL,
    instrument_key text NOT NULL CHECK (length(instrument_key)<=400),
    row_data jsonb NOT NULL CHECK (octet_length(row_data::text)<=16384),
    FOREIGN KEY(owner_id,study_id) REFERENCES study_actualizations(owner_id,study_id)
);
CREATE INDEX study_actual_setups_page ON study_actual_setups(owner_id,study_id,revision,signal_close_unix DESC,instrument_key,id);
CREATE INDEX study_actual_setups_run ON study_actual_setups(run_id);
