-- Settings are typed and owner scoped. UI settings remain single-admin legacy data.
CREATE TABLE study_actual_continuous (
    owner_id text NOT NULL,
    algorithm_id text NOT NULL CHECK (algorithm_id IN ('price_volume_spike','price_drop_volume_spike')),
    enabled boolean NOT NULL DEFAULT true,
    from_date date NOT NULL CHECK (from_date BETWEEN DATE '1970-01-01' AND DATE '9999-12-31'),
    from_boundary timestamptz NOT NULL,
    time_zone text NOT NULL CHECK (length(time_zone) BETWEEN 1 AND 128),
    config_revision bigint NOT NULL DEFAULT 1 CHECK (config_revision>0),
    cursor_generation bigint NOT NULL DEFAULT 1 CHECK (cursor_generation>0),
    cursor_study_id bigint,
    cursor_execution_started_at timestamptz,
    reset_pending boolean NOT NULL DEFAULT false,
    scan_study_id bigint,
    scan_execution_started_at timestamptz,
    next_due_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    last_checked_at timestamptz,
    last_admitted_at timestamptz,
    last_skip_reason text CHECK (length(last_skip_reason)<=160),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (owner_id,algorithm_id),
    CHECK ((cursor_study_id IS NULL)=(cursor_execution_started_at IS NULL)),
    CHECK ((scan_study_id IS NULL)=(scan_execution_started_at IS NULL))
);
ALTER TABLE study_actualizations ADD COLUMN request_origin text NOT NULL DEFAULT 'manual'
    CHECK (request_origin IN ('manual','automatic'));
ALTER TABLE study_actualizations ADD COLUMN automatic_config_revision bigint;
ALTER TABLE study_actualizations ADD COLUMN automatic_cursor_generation bigint;
ALTER TABLE study_actualizations ADD COLUMN last_automatic_cutoff bigint;
ALTER TABLE study_actualizations ADD COLUMN queue_sequence bigserial;
-- ALTER bigserial fills legacy rows but physical row order is not FIFO.
WITH ordered AS (SELECT owner_id,study_id,row_number() OVER (ORDER BY requested_at,job_id,owner_id,study_id) seq FROM study_actualizations)
UPDATE study_actualizations a SET queue_sequence=o.seq FROM ordered o WHERE (a.owner_id,a.study_id)=(o.owner_id,o.study_id);
SELECT setval(pg_get_serial_sequence('study_actualizations','queue_sequence'),greatest(coalesce((SELECT max(queue_sequence) FROM study_actualizations),0),1),EXISTS(SELECT 1 FROM study_actualizations));
ALTER TABLE study_actualizations ADD CONSTRAINT study_actual_config_origin CHECK
    ((request_origin='automatic')=(automatic_config_revision IS NOT NULL)
      AND (request_origin='automatic')=(automatic_cursor_generation IS NOT NULL));
CREATE INDEX study_actual_priority ON study_actualizations
    ((request_origin='manual') DESC,queue_sequence) WHERE status='queued';
CREATE INDEX study_actual_owner_lane ON study_actualizations(owner_id,status) WHERE status IN ('queued','running');
CREATE UNIQUE INDEX study_actual_one_automatic ON study_actualizations(owner_id) WHERE request_origin='automatic' AND status IN ('queued','running');
CREATE INDEX study_actual_continuous_due ON study_actual_continuous(next_due_at,last_checked_at,owner_id,algorithm_id) WHERE enabled;
CREATE INDEX study_actual_candidates ON optimization_studies(owner_id,algorithm_id,execution_started_at,id) WHERE status='completed';
CREATE INDEX study_actual_generation_start ON continuous_generations(owner_id,execution_started_at,study_id);

-- Also guard old binaries during a rolling overlap; their queue lock is the
-- same, but their pre-0029 query could otherwise claim two studies for an owner.
-- Existing running rows are not interrupted by DDL; startup recovery fences and
-- cleans them using the pre-existing owned-pair path before fresh claims.
CREATE FUNCTION study_actual_serial_lane() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.status='running' AND (TG_OP='INSERT' OR OLD.status<>'running') THEN
        PERFORM pg_advisory_xact_lock(72423412);
        IF EXISTS(SELECT 1 FROM study_actualizations WHERE owner_id=NEW.owner_id AND study_id<>NEW.study_id AND status='running') THEN
            RAISE EXCEPTION 'study_actual_owner_lane_busy';
        END IF;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER study_actual_serial_lane BEFORE INSERT OR UPDATE ON study_actualizations FOR EACH ROW EXECUTE FUNCTION study_actual_serial_lane();

-- Exact source clock shared by candidate ordering and the existing source loader.
-- Malformed legacy events are ineligible, never a cast failure for the scheduler.
CREATE FUNCTION study_actual_execution_start(events jsonb) RETURNS numeric
LANGUAGE plpgsql STABLE STRICT AS $$
DECLARE e jsonb;
DECLARE fraction text;
DECLARE whole text;
BEGIN
    IF jsonb_typeof(events)<>'array' THEN RETURN NULL; END IF;
    FOR e IN SELECT value FROM jsonb_array_elements(events) LOOP
        IF e->>'kind'='started' AND e->>'at' ~ '^\d{4}-\d{2}-\d{2}T.*(Z|[+-]\d{2}:\d{2})$' THEN
            BEGIN
                -- Keep event nanoseconds exact: timestamptz alone would round
                -- 59.999999999 across the first-legal-candle boundary.
                fraction := substring(e->>'at' from '\.([0-9]+)');
                whole := regexp_replace(e->>'at','\.[0-9]+','');
                RETURN extract(epoch FROM whole::timestamptz)
                    + CASE WHEN fraction IS NULL THEN 0 ELSE ('0.'||fraction)::numeric END;
            EXCEPTION WHEN OTHERS THEN CONTINUE;
            END;
        END IF;
    END LOOP;
    RETURN NULL;
END $$;
