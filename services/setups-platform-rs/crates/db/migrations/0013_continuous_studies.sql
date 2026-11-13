-- Logical tenant identities are internal; current authenticated installation is admin.
ALTER TABLE optimization_studies ADD COLUMN origin text NOT NULL DEFAULT 'manual'
    CHECK (origin IN ('manual', 'continuous'));
ALTER TABLE optimization_studies ADD COLUMN owner_id text NOT NULL DEFAULT 'admin';
ALTER TABLE optimization_studies ADD COLUMN continuous_generation_id bigint;
ALTER TABLE optimization_studies ADD COLUMN continuous_slot_id bigint;
DROP INDEX optimization_studies_one_active;
CREATE UNIQUE INDEX optimization_studies_one_active
    ON optimization_studies (owner_id)
    WHERE origin = 'manual' AND status IN ('queued','running','cancel_requested');

CREATE TABLE continuous_owner_lanes (
    owner_id text PRIMARY KEY CHECK (length(owner_id) BETWEEN 1 AND 120),
    lease_owner text,
    lease_until timestamptz,
    lease_token bigint NOT NULL DEFAULT 0,
    last_algorithm_id text NOT NULL DEFAULT ''
);
CREATE TABLE continuous_processes (
    owner_id text NOT NULL REFERENCES continuous_owner_lanes(owner_id),
    algorithm_id text NOT NULL,
    status text NOT NULL DEFAULT 'stopped' CHECK (status IN ('stopped','running','stopping','error')),
    desired_running boolean NOT NULL DEFAULT false,
    config_revision bigint NOT NULL DEFAULT 1,
    config jsonb NOT NULL CHECK (octet_length(config::text) <= 262144),
    next_slot integer NOT NULL DEFAULT 0,
    active_generation_id bigint,
    active_study_id bigint REFERENCES optimization_studies(id) ON DELETE SET NULL,
    phase text,
    error jsonb CHECK (octet_length(error::text) <= 4096),
    retry_count integer NOT NULL DEFAULT 0,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    cleanup_targets jsonb NOT NULL DEFAULT '{"studies":[],"runs":[],"batches":[]}'::jsonb,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (owner_id, algorithm_id)
);
CREATE TABLE continuous_slots (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    min_setups_per_day numeric NOT NULL CHECK (min_setups_per_day >= 0),
    enabled boolean NOT NULL DEFAULT true,
    status text NOT NULL DEFAULT 'idle',
    active_generation_id bigint,
    study_id bigint REFERENCES optimization_studies(id) ON DELETE SET NULL,
    mean_expectancy double precision,
    selected_cell jsonb,
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_id, algorithm_id, min_setups_per_day),
    FOREIGN KEY (owner_id, algorithm_id) REFERENCES continuous_processes(owner_id, algorithm_id)
);
CREATE TABLE continuous_generations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    slot_id bigint NOT NULL REFERENCES continuous_slots(id),
    config_revision bigint NOT NULL,
    lease_token bigint NOT NULL,
    status text NOT NULL DEFAULT 'running' CHECK (status IN ('running','completed','failed','cancelled')),
    phase text NOT NULL DEFAULT 'acquiring',
    spec jsonb NOT NULL CHECK (octet_length(spec::text) <= 524288),
    execution_started_at timestamptz NOT NULL,
    observation_cutoff bigint NOT NULL,
    study_id bigint REFERENCES optimization_studies(id) ON DELETE SET NULL,
    replay_batch_id integer REFERENCES replay_batches(id),
    replay_manifest jsonb CHECK (octet_length(replay_manifest::text) <= 1048576),
    reserved_bytes bigint NOT NULL DEFAULT 0 CHECK (reserved_bytes >= 0),
    result jsonb CHECK (octet_length(result::text) <= 8388608),
    error jsonb CHECK (octet_length(error::text) <= 4096),
    accepted boolean,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (owner_id, algorithm_id) REFERENCES continuous_processes(owner_id, algorithm_id),
    CHECK (observation_cutoff <= extract(epoch FROM execution_started_at))
);
CREATE UNIQUE INDEX continuous_one_chain_per_owner ON continuous_generations(owner_id)
    WHERE status = 'running';
ALTER TABLE continuous_processes ADD FOREIGN KEY (active_generation_id)
    REFERENCES continuous_generations(id);
ALTER TABLE continuous_slots ADD FOREIGN KEY (active_generation_id)
    REFERENCES continuous_generations(id);
ALTER TABLE optimization_studies ADD FOREIGN KEY (continuous_generation_id)
    REFERENCES continuous_generations(id);
ALTER TABLE optimization_studies ADD FOREIGN KEY (continuous_slot_id)
    REFERENCES continuous_slots(id);
ALTER TABLE replay_runs ADD COLUMN origin text NOT NULL DEFAULT 'manual'
    CHECK (origin IN ('manual','continuous'));
ALTER TABLE replay_runs ADD COLUMN owner_id text NOT NULL DEFAULT 'admin';
ALTER TABLE replay_runs ADD COLUMN continuous_generation_id bigint REFERENCES continuous_generations(id);
ALTER TABLE replay_batches ADD COLUMN origin text NOT NULL DEFAULT 'manual'
    CHECK (origin IN ('manual','continuous'));
ALTER TABLE replay_batches ADD COLUMN owner_id text NOT NULL DEFAULT 'admin';
ALTER TABLE replay_batches ADD COLUMN continuous_generation_id bigint REFERENCES continuous_generations(id);

-- Fixed rows keep their historical identity; optimized slots cannot suppress them.
ALTER TABLE live_setups ADD COLUMN adaptive_slot_id bigint REFERENCES continuous_slots(id);
ALTER TABLE live_setups ADD COLUMN adaptive_generation_id bigint REFERENCES continuous_generations(id);
ALTER TABLE live_setups ADD COLUMN recommendation jsonb
    CHECK (octet_length(recommendation::text) <= 16384);
ALTER TABLE live_setups ADD COLUMN external_name text CHECK (length(external_name) <= 200);
ALTER TABLE live_setups DROP CONSTRAINT live_setups_identity_unique;
CREATE UNIQUE INDEX live_setups_identity_unique ON live_setups
    (exchange_code,category,symbol,algorithm_id,algorithm_version,open_time)
    WHERE adaptive_slot_id IS NULL;
CREATE UNIQUE INDEX live_setups_adaptive_identity_unique ON live_setups
    (adaptive_slot_id,exchange_code,category,symbol,open_time)
    WHERE adaptive_slot_id IS NOT NULL;
CREATE TABLE adaptive_live_checkpoints (
    slot_id bigint NOT NULL REFERENCES continuous_slots(id),
    instrument_key text NOT NULL CHECK (length(instrument_key) BETWEEN 1 AND 400),
    generation_id bigint NOT NULL REFERENCES continuous_generations(id),
    state jsonb NOT NULL CHECK (octet_length(state::text) <= 16384),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(slot_id,instrument_key)
);
