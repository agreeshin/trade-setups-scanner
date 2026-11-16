-- Keep slot IDs: checkpoints, signal identity, outbox and scientific provenance
-- continue referencing their original rows. Research membership is only enabled.
CREATE TABLE adaptive_live_permissions (
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    enabled boolean NOT NULL DEFAULT false,
    PRIMARY KEY(owner_id,algorithm_id)
);
INSERT INTO adaptive_live_permissions(owner_id,algorithm_id,enabled)
SELECT owner_id,algorithm_id,coalesce((config->>'adaptiveEnabled')::boolean,false)
FROM continuous_processes;

CREATE TABLE adaptive_profiles (
    id bigint PRIMARY KEY REFERENCES continuous_slots(id),
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    min_setups_per_day numeric NOT NULL CHECK(min_setups_per_day>=0),
    desired_running boolean NOT NULL DEFAULT true,
    stop_pending boolean NOT NULL DEFAULT false,
    control_revision bigint NOT NULL DEFAULT 1 CHECK(control_revision>0),
    accepted_generation_id bigint NOT NULL REFERENCES continuous_generations(id),
    error jsonb CHECK(octet_length(error::text)<=4096),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(owner_id,algorithm_id,min_setups_per_day),
    FOREIGN KEY(owner_id,algorithm_id) REFERENCES adaptive_live_permissions(owner_id,algorithm_id)
);
-- Only actual accepted profiles are migrated; configured empty slots are not live.
-- Original revision preserves pre-upgrade delivery fences without rewriting jobs.
INSERT INTO adaptive_profiles(id,owner_id,algorithm_id,min_setups_per_day,
    desired_running,control_revision,accepted_generation_id,updated_at)
SELECT s.id,s.owner_id,s.algorithm_id,s.min_setups_per_day,
    s.enabled AND p.desired_running,p.config_revision,g.id,s.updated_at
FROM continuous_slots s JOIN continuous_processes p USING(owner_id,algorithm_id)
JOIN LATERAL (SELECT g.id FROM continuous_generations g WHERE g.slot_id=s.id
    AND g.status='completed' AND g.accepted
    AND (g.id=s.active_generation_id OR g.id=s.pending_generation_id)
    ORDER BY g.id DESC LIMIT 1) g ON true;

CREATE TABLE adaptive_profile_limits (
    owner_id text PRIMARY KEY,
    total_profiles integer NOT NULL CHECK(total_profiles BETWEEN 1 AND 10000),
    running_profiles integer NOT NULL CHECK(running_profiles BETWEEN 1 AND total_profiles)
);
-- Defaults are deployment configuration, later overridden by external user tiers.
INSERT INTO settings(key,value) VALUES('adaptiveProfileLimits',
    '{"totalProfiles":50,"runningProfiles":30}') ON CONFLICT DO NOTHING;

CREATE TABLE adaptive_profile_handoffs (
    generation_id bigint PRIMARY KEY REFERENCES continuous_generations(id),
    owner_id text NOT NULL,
    slot_id bigint NOT NULL REFERENCES continuous_slots(id),
    version integer NOT NULL DEFAULT 1 CHECK(version=1),
    status text NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','accepted','superseded','blocked')),
    error jsonb CHECK(octet_length(error::text)<=4096),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX adaptive_profile_handoff_due ON adaptive_profile_handoffs(owner_id,generation_id)
    WHERE status='pending';
CREATE TABLE adaptive_profile_commands (
    owner_id text NOT NULL,
    request_id text NOT NULL CHECK(length(request_id) BETWEEN 16 AND 128),
    profile_id bigint NOT NULL REFERENCES adaptive_profiles(id),
    action text NOT NULL CHECK(action IN ('stop','play')),
    expected_revision bigint NOT NULL,
    expected_generation_id bigint NOT NULL,
    receipt jsonb NOT NULL CHECK(octet_length(receipt::text)<=8192),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(owner_id,request_id)
);
-- Last stopped epoch only, bounded by the profile universe. A deliberate Play
-- starts fresh causal warmup; old confirmed cursors are not fabricated/advanced.
CREATE TABLE adaptive_profile_stopped_checkpoints (
    profile_id bigint NOT NULL REFERENCES adaptive_profiles(id),
    instrument_key text NOT NULL,
    generation_id bigint NOT NULL REFERENCES continuous_generations(id),
    state jsonb NOT NULL CHECK(octet_length(state::text)<=16384),
    PRIMARY KEY(profile_id,instrument_key)
);
-- Compatibility projection for the existing bounded live-store SQL. This is
-- profile control state, never research desired-running/config revision.
CREATE VIEW adaptive_live_controls AS
SELECT p.id,p.owner_id,p.algorithm_id,p.desired_running,
    p.control_revision AS config_revision,p.accepted_generation_id,
    CASE WHEN p.desired_running THEN 'running' ELSE 'stopped' END AS status,
    jsonb_build_object('adaptiveEnabled',a.enabled) AS config
FROM adaptive_profiles p JOIN adaptive_live_permissions a USING(owner_id,algorithm_id);
