-- Optional Telegram delivery is independent of WebPush and has no historical backfill.
ALTER TABLE live_setups ADD COLUMN telegram_delivery_status text NOT NULL DEFAULT 'not_requested'
    CHECK (telegram_delivery_status IN ('not_requested','disabled','pending','expired','skipped_capacity','sent','failed'));
ALTER TABLE live_setups ADD COLUMN telegram_delivery_error jsonb
    CHECK (octet_length(telegram_delivery_error::text)<=2048);
ALTER TABLE live_setups ADD COLUMN telegram_delivery_updated_at timestamptz;

CREATE TABLE telegram_relay_config (
    id smallint PRIMARY KEY CHECK(id=1),
    enabled boolean NOT NULL DEFAULT false,
    endpoint_url text NOT NULL DEFAULT '' CHECK(octet_length(endpoint_url)<=2048),
    revision bigint NOT NULL DEFAULT 0 CHECK(revision>=0),
    state text NOT NULL DEFAULT 'disabled' CHECK(state IN ('disabled','not_configured','checking','connected','error','unknown')),
    telegram_channel text NOT NULL DEFAULT 'unknown' CHECK(telegram_channel IN ('unknown','ready','error')),
    checked_at timestamptz,
    reason jsonb CHECK(octet_length(reason::text)<=2048),
    probe_started_at timestamptz,
    test_started_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO telegram_relay_config(id) VALUES(1);
CREATE TABLE telegram_delivery_budget (
    id smallint PRIMARY KEY CHECK(id=1),
    instance_id uuid NOT NULL DEFAULT gen_random_uuid(),
    max_jobs bigint NOT NULL DEFAULT 10000 CHECK(max_jobs BETWEEN 1 AND 100000),
    max_bytes bigint NOT NULL DEFAULT 67108864 CHECK(max_bytes BETWEEN 65536 AND 1073741824),
    pending_jobs bigint NOT NULL DEFAULT 0 CHECK(pending_jobs>=0),
    pending_bytes bigint NOT NULL DEFAULT 0 CHECK(pending_bytes>=0),
    capacity_skipped bigint NOT NULL DEFAULT 0 CHECK(capacity_skipped>=0),
    last_capacity_skip_at timestamptz
);
INSERT INTO telegram_delivery_budget(id) VALUES(1);
CREATE TABLE telegram_delivery_outbox (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    setup_id integer NOT NULL UNIQUE REFERENCES live_setups(id) ON DELETE CASCADE,
    delivery_id text NOT NULL UNIQUE CHECK(length(delivery_id) BETWEEN 1 AND 128),
    endpoint_url text NOT NULL CHECK(octet_length(endpoint_url)<=2048),
    settings_revision bigint NOT NULL,
    owner_id text NOT NULL,
    algorithm_id text NOT NULL,
    slot_id bigint REFERENCES continuous_slots(id),
    generation_id bigint REFERENCES continuous_generations(id),
    config_revision bigint,
    fixed_config jsonb CHECK(octet_length(fixed_config::text)<=16384),
    raw_close_unix bigint NOT NULL,
    expires_at timestamptz NOT NULL,
    payload jsonb NOT NULL CHECK(octet_length(payload::text)<=32768),
    request_body bytea CHECK(octet_length(request_body)<=32768),
    request_body_hash text CHECK(length(request_body_hash)=64),
    reserved_bytes bigint NOT NULL DEFAULT 0,
    attempts integer NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 5),
    not_before timestamptz NOT NULL DEFAULT now(),
    lease_owner text CHECK(length(lease_owner) BETWEEN 1 AND 200),
    lease_token bigint NOT NULL DEFAULT 0,
    lease_until timestamptz,
    last_error jsonb CHECK(octet_length(last_error::text)<=2048),
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK((request_body IS NULL)=(request_body_hash IS NULL)),
    CHECK((slot_id IS NOT NULL AND generation_id IS NOT NULL AND config_revision IS NOT NULL AND fixed_config IS NULL)
        OR (slot_id IS NULL AND generation_id IS NULL AND config_revision IS NULL AND fixed_config IS NOT NULL))
);
CREATE INDEX telegram_delivery_due ON telegram_delivery_outbox(not_before,id);
CREATE INDEX telegram_delivery_expiry ON telegram_delivery_outbox(expires_at);
CREATE FUNCTION telegram_delivery_account() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='INSERT' THEN
        PERFORM 1 FROM telegram_delivery_budget WHERE id=1 FOR UPDATE;
        IF NOT FOUND THEN RAISE EXCEPTION 'telegram_delivery_budget_missing'; END IF;
        IF NEW.expires_at<date_trunc('second',clock_timestamp()) THEN
            UPDATE live_setups SET telegram_delivery_status='expired',telegram_delivery_updated_at=clock_timestamp() WHERE id=NEW.setup_id;
            RETURN NULL;
        END IF;
        -- Reserve the full protocol body allowance before formatting, so freezing
        -- exact raw bytes cannot exceed the admitted queue/RAM budget later.
        NEW.reserved_bytes:=octet_length(NEW.payload::text)+32768;
        UPDATE telegram_delivery_budget SET pending_jobs=pending_jobs+1,pending_bytes=pending_bytes+NEW.reserved_bytes
          WHERE id=1 AND pending_jobs<max_jobs AND pending_bytes+NEW.reserved_bytes<=max_bytes;
        IF NOT FOUND THEN
            UPDATE live_setups SET telegram_delivery_status='skipped_capacity',telegram_delivery_updated_at=clock_timestamp() WHERE id=NEW.setup_id;
            UPDATE telegram_delivery_budget SET capacity_skipped=capacity_skipped+1,last_capacity_skip_at=clock_timestamp() WHERE id=1;
            RETURN NULL;
        END IF;
        UPDATE live_setups SET telegram_delivery_status='pending',telegram_delivery_updated_at=clock_timestamp() WHERE id=NEW.setup_id;
        RETURN NEW;
    END IF;
    UPDATE telegram_delivery_budget SET pending_jobs=pending_jobs-1,pending_bytes=pending_bytes-OLD.reserved_bytes WHERE id=1;
    RETURN OLD;
END;
$$;
CREATE TRIGGER telegram_delivery_insert_budget BEFORE INSERT ON telegram_delivery_outbox
    FOR EACH ROW EXECUTE FUNCTION telegram_delivery_account();
CREATE TRIGGER telegram_delivery_delete_budget AFTER DELETE ON telegram_delivery_outbox
    FOR EACH ROW EXECUTE FUNCTION telegram_delivery_account();

-- Bounded receipts for explicit one-shot tests, separate from live jobs/retries.
CREATE TABLE telegram_relay_tests (
    request_id text PRIMARY KEY CHECK(length(request_id) BETWEEN 16 AND 128),
    settings_revision bigint NOT NULL,
    endpoint_url text NOT NULL CHECK(octet_length(endpoint_url)<=2048),
    delivery_id text NOT NULL CHECK(length(delivery_id)<=128),
    request_body bytea NOT NULL CHECK(octet_length(request_body)<=32768),
    -- The original diagnostic deadline, not the 24-hour receipt retention.
    -- Cleared on every completed outcome; never renewed by duplicate requests.
    in_flight_until timestamptz,
    state text NOT NULL DEFAULT 'unknown' CHECK(state IN ('sent','unknown','error')),
    reason jsonb CHECK(octet_length(reason::text)<=2048),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX telegram_relay_tests_expiry ON telegram_relay_tests(created_at);
