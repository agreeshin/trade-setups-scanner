-- Private generation-owned execution state, never public running StudyOutcome/history.
CREATE TABLE continuous_maintenance (
    owner_id text PRIMARY KEY CHECK (length(owner_id) BETWEEN 1 AND 120),
    admission_paused boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE continuous_generations ADD COLUMN resume_version integer;
ALTER TABLE continuous_generations ADD COLUMN research_deadline timestamptz;
-- Refencing for housekeeping never grants execution of a recovery attempt.
ALTER TABLE continuous_generations ADD COLUMN recovery_lease_token bigint;
ALTER TABLE continuous_generations ADD COLUMN recovery_pending boolean NOT NULL DEFAULT false;
CREATE TABLE continuous_runtime_artifacts (
    generation_id bigint PRIMARY KEY REFERENCES continuous_generations(id) ON DELETE CASCADE,
    identity jsonb NOT NULL CHECK (octet_length(identity::text) <= 4096),
    reserved_bytes bigint NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    snapshot_manifest jsonb CHECK (octet_length(snapshot_manifest::text) <= 1048576),
    snapshot_bytes bigint NOT NULL DEFAULT 0 CHECK (snapshot_bytes >= 0),
    checkpoint bytea CHECK (octet_length(checkpoint) <= 16777216),
    checkpoint_hash text,
    checkpoint_sequence bigint NOT NULL DEFAULT 0 CHECK (checkpoint_sequence >= 0),
    gate_result jsonb CHECK (octet_length(gate_result::text) <= 8388608),
    gate_accepted boolean,
    gate_mean double precision,
    gate_selected jsonb,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE continuous_frozen_chunks (
    generation_id bigint NOT NULL REFERENCES continuous_runtime_artifacts(generation_id) ON DELETE CASCADE,
    chunk_index integer NOT NULL CHECK (chunk_index >= 0),
    payload bytea NOT NULL CHECK (octet_length(payload) BETWEEN 1 AND 1048576),
    checksum text NOT NULL CHECK (length(checksum)=64),
    PRIMARY KEY(generation_id,chunk_index)
);
