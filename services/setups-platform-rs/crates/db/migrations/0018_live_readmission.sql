-- Explicit operator grants extend only operational live initialization budgets.
-- Existing attempts/anchors/scientific results are never reset or rewritten.
ALTER TABLE continuous_generations ADD COLUMN live_init_attempt_limit integer NOT NULL DEFAULT 3;
UPDATE continuous_generations SET live_init_attempt_limit=greatest(3,live_init_attempts);
ALTER TABLE continuous_generations DROP CONSTRAINT continuous_generations_live_init_attempts_check;
ALTER TABLE continuous_generations ADD CONSTRAINT continuous_generations_live_init_budget_check
    CHECK (live_init_attempts>=0 AND live_init_attempt_limit>=3 AND live_init_attempts<=live_init_attempt_limit);

CREATE TABLE continuous_live_readmissions (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    slot_id bigint NOT NULL REFERENCES continuous_slots(id),
    generation_id bigint NOT NULL REFERENCES continuous_generations(id) ON DELETE CASCADE,
    config_revision bigint NOT NULL,
    request_id text NOT NULL CHECK (length(request_id) BETWEEN 16 AND 128),
    attempts_before integer NOT NULL CHECK (attempts_before>=0),
    limit_before integer NOT NULL CHECK (limit_before=attempts_before),
    attempt_limit integer NOT NULL CHECK (attempt_limit::bigint=limit_before::bigint+3),
    prior_state jsonb NOT NULL CHECK (jsonb_typeof(prior_state)='object' AND octet_length(prior_state::text)<=16384),
    granted_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    UNIQUE(owner_id,request_id),
    FOREIGN KEY(owner_id,algorithm_id) REFERENCES continuous_processes(owner_id,algorithm_id)
);
CREATE INDEX continuous_live_readmissions_generation ON continuous_live_readmissions(generation_id);
-- Latest authoritative outcome is slot+revision scoped, not algorithm-wide.
CREATE INDEX continuous_generations_completed_slot ON continuous_generations(owner_id,algorithm_id,slot_id,config_revision,id DESC)
    WHERE status='completed';
