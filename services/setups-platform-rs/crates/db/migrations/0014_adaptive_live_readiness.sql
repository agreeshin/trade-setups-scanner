-- Additive live readiness: completed research is not an active live generation.
ALTER TABLE continuous_slots ADD COLUMN pending_generation_id bigint REFERENCES continuous_generations(id);
ALTER TABLE continuous_slots ADD COLUMN pending_status text CHECK (pending_status IN ('warming','ready','error'));
ALTER TABLE continuous_slots ADD COLUMN readiness_frontier bigint CHECK (readiness_frontier >= 0 AND readiness_frontier % 60 = 0);
ALTER TABLE continuous_slots ADD COLUMN pending_error jsonb CHECK (octet_length(pending_error::text) <= 4096);
ALTER TABLE continuous_slots ADD COLUMN live_watermark bigint CHECK (live_watermark >= 0 AND live_watermark % 60 = 0);
CREATE INDEX continuous_slots_pending ON continuous_slots(id) WHERE pending_generation_id IS NOT NULL;

-- One bounded diagnostic per saved setup, never a second overflow queue.
ALTER TABLE live_setups ADD COLUMN notification_delivery_status text NOT NULL DEFAULT 'not_requested'
    CHECK (notification_delivery_status IN ('not_requested','pending','sent','skipped_capacity','expired','skipped','failed'));
ALTER TABLE live_setups ADD COLUMN notification_delivery_error text CHECK (octet_length(notification_delivery_error) <= 1024);
ALTER TABLE live_setups ADD COLUMN notification_delivery_updated_at timestamptz;

-- One globally bounded replacement warmup, with an independent durable epoch.
CREATE TABLE adaptive_live_warmup_lane (
    id smallint PRIMARY KEY CHECK (id = 1),
    slot_id bigint REFERENCES continuous_slots(id),
    generation_id bigint REFERENCES continuous_generations(id),
    lease_owner text CHECK (length(lease_owner) BETWEEN 1 AND 200),
    lease_token bigint NOT NULL DEFAULT 0,
    lease_until timestamptz
);
INSERT INTO adaptive_live_warmup_lane(id) VALUES (1);

CREATE TABLE adaptive_notification_budget (
    id smallint PRIMARY KEY CHECK (id = 1),
    max_jobs bigint NOT NULL DEFAULT 10000 CHECK (max_jobs BETWEEN 1 AND 100000),
    max_bytes bigint NOT NULL DEFAULT 67108864 CHECK (max_bytes BETWEEN 32768 AND 1073741824),
    pending_jobs bigint NOT NULL DEFAULT 0 CHECK (pending_jobs >= 0),
    pending_bytes bigint NOT NULL DEFAULT 0 CHECK (pending_bytes >= 0),
    capacity_skipped bigint NOT NULL DEFAULT 0 CHECK (capacity_skipped >= 0),
    last_capacity_skip_at timestamptz
);
INSERT INTO adaptive_notification_budget(id) VALUES (1);
CREATE TABLE adaptive_notification_outbox (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    setup_id integer NOT NULL UNIQUE REFERENCES live_setups(id) ON DELETE CASCADE,
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    slot_id bigint REFERENCES continuous_slots(id),
    generation_id bigint REFERENCES continuous_generations(id),
    config_revision bigint,
    fixed_config jsonb CHECK (octet_length(fixed_config::text) <= 16384),
    raw_close_unix bigint NOT NULL,
    expires_at timestamptz NOT NULL,
    payload jsonb NOT NULL CHECK (octet_length(payload::text) <= 32768),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 5),
    not_before timestamptz NOT NULL DEFAULT now(),
    lease_owner text CHECK (length(lease_owner) BETWEEN 1 AND 200),
    lease_token bigint NOT NULL DEFAULT 0,
    lease_until timestamptz,
    last_error text CHECK (octet_length(last_error) <= 1024),
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((slot_id IS NOT NULL AND generation_id IS NOT NULL AND config_revision IS NOT NULL AND fixed_config IS NULL)
        OR (slot_id IS NULL AND generation_id IS NULL AND config_revision IS NULL AND fixed_config IS NOT NULL))
);
CREATE INDEX adaptive_notification_due ON adaptive_notification_outbox(not_before,id);
CREATE INDEX adaptive_notification_expiry ON adaptive_notification_outbox(expires_at);

-- Counters include leased/retry jobs; cascaded history deletion also releases budget.
CREATE FUNCTION adaptive_notification_account() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        PERFORM 1 FROM adaptive_notification_budget WHERE id = 1 FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'notification_outbox_budget_missing';
        END IF;
        IF NEW.expires_at <= clock_timestamp() THEN
            UPDATE live_setups SET notification_delivery_status = 'expired',
                notification_delivery_updated_at = clock_timestamp() WHERE id = NEW.setup_id;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'notification_outbox_setup_missing' USING ERRCODE = '23503';
            END IF;
            RETURN NULL;
        END IF;
        UPDATE adaptive_notification_budget
           SET pending_jobs = pending_jobs + 1,
               pending_bytes = pending_bytes + octet_length(NEW.payload::text)
         WHERE id = 1 AND pending_jobs < max_jobs
           AND pending_bytes + octet_length(NEW.payload::text) <= max_bytes;
        IF NOT FOUND THEN
            UPDATE live_setups SET notification_delivery_status = 'skipped_capacity',
                notification_delivery_updated_at = clock_timestamp() WHERE id = NEW.setup_id;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'notification_outbox_setup_missing' USING ERRCODE = '23503';
            END IF;
            UPDATE adaptive_notification_budget SET capacity_skipped = capacity_skipped + 1,
                last_capacity_skip_at = clock_timestamp() WHERE id = 1;
            RETURN NULL;
        END IF;
        UPDATE live_setups SET notification_delivery_status = 'pending',
            notification_delivery_updated_at = clock_timestamp() WHERE id = NEW.setup_id;
        RETURN NEW;
    END IF;
    UPDATE adaptive_notification_budget
       SET pending_jobs = pending_jobs - 1,
           pending_bytes = pending_bytes - octet_length(OLD.payload::text)
     WHERE id = 1;
    RETURN OLD;
END;
$$;
CREATE TRIGGER adaptive_notification_insert_budget BEFORE INSERT ON adaptive_notification_outbox
    FOR EACH ROW EXECUTE FUNCTION adaptive_notification_account();
CREATE TRIGGER adaptive_notification_delete_budget AFTER DELETE ON adaptive_notification_outbox
    FOR EACH ROW EXECUTE FUNCTION adaptive_notification_account();
