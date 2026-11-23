-- Private manual execution identity. Queue order is independent of history ID:
-- explicit Play of an old study appends to the same owner FIFO.
CREATE SEQUENCE manual_study_queue_sequence;
ALTER TABLE optimization_studies
    DROP CONSTRAINT optimization_studies_status_check,
    ADD CONSTRAINT optimization_studies_status_check CHECK
        (status IN ('queued','running','pausing','paused','cancel_requested','completed','cancelled','failed')),
    ADD COLUMN manual_format_version integer CHECK (manual_format_version=1),
    ADD COLUMN manual_control_revision bigint NOT NULL DEFAULT 1 CHECK (manual_control_revision>0),
    ADD COLUMN manual_queue_sequence bigint NOT NULL DEFAULT nextval('manual_study_queue_sequence'),
    ADD COLUMN manual_execution_epoch bigint NOT NULL DEFAULT 0 CHECK (manual_execution_epoch>=0),
    ADD COLUMN manual_execution_reserved boolean NOT NULL DEFAULT false,
    ADD COLUMN manual_spent_ms bigint NOT NULL DEFAULT 0 CHECK (manual_spent_ms>=0),
    ADD COLUMN manual_segment_started_at timestamptz,
    ADD COLUMN manual_limits jsonb CHECK (octet_length(manual_limits::text)<=8192),
    ADD COLUMN manual_idempotency_key varchar(128);
UPDATE optimization_studies SET manual_execution_reserved=true
    WHERE origin='manual' AND status IN ('running','cancel_requested');
DROP INDEX optimization_studies_one_active;
CREATE UNIQUE INDEX optimization_studies_one_active ON optimization_studies(owner_id)
    WHERE origin='manual' AND manual_execution_reserved;
CREATE UNIQUE INDEX manual_study_idempotency ON optimization_studies(owner_id,manual_idempotency_key)
    WHERE origin='manual' AND manual_idempotency_key IS NOT NULL;
CREATE INDEX manual_study_fifo ON optimization_studies(owner_id,manual_queue_sequence)
    WHERE origin='manual' AND status='queued';
ALTER TABLE optimization_studies ADD CONSTRAINT manual_execution_reservation CHECK
    (origin<>'manual' OR
      ((NOT manual_execution_reserved OR status IN ('running','pausing','cancel_requested')) AND
       (status NOT IN ('running','pausing') OR manual_execution_reserved)));

CREATE TABLE manual_runtime_artifacts (
    study_id bigint PRIMARY KEY REFERENCES optimization_studies(id) ON DELETE CASCADE,
    identity jsonb NOT NULL CHECK (octet_length(identity::text)<=4096),
    reserved_bytes bigint NOT NULL CHECK (reserved_bytes>=0),
    snapshot_manifest jsonb CHECK (octet_length(snapshot_manifest::text)<=1048576),
    snapshot_bytes bigint NOT NULL DEFAULT 0 CHECK (snapshot_bytes>=0),
    observation_cutoff bigint,
    checkpoint bytea CHECK (octet_length(checkpoint)<=16777216),
    checkpoint_hash text,
    checkpoint_sequence bigint NOT NULL DEFAULT 0 CHECK (checkpoint_sequence>=0)
);
CREATE TABLE manual_frozen_chunks (
    study_id bigint NOT NULL REFERENCES manual_runtime_artifacts(study_id) ON DELETE CASCADE,
    chunk_index integer NOT NULL CHECK (chunk_index>=0),
    payload bytea NOT NULL CHECK (octet_length(payload) BETWEEN 1 AND 1048576),
    checksum text NOT NULL CHECK (length(checksum)=64),
    PRIMARY KEY(study_id,chunk_index)
);
