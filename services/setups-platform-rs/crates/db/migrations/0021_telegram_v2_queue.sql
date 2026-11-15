-- Existing rows, including unfrozen work, stay on the original wire contract.
ALTER TABLE telegram_delivery_outbox ADD COLUMN wire_version smallint NOT NULL DEFAULT 1
    CHECK(wire_version IN (1,2));
ALTER TABLE telegram_delivery_outbox ADD COLUMN render_diagnostic jsonb
    CHECK(octet_length(render_diagnostic::text)<=2048);
ALTER TABLE telegram_delivery_outbox DROP CONSTRAINT telegram_delivery_outbox_request_body_check;
ALTER TABLE telegram_delivery_outbox ADD CONSTRAINT telegram_delivery_outbox_request_body_check
    CHECK(octet_length(request_body)<=CASE wire_version WHEN 1 THEN 32768 ELSE 393216 END);

-- Do not recalculate existing reservations: their original allowance must be
-- released verbatim on deletion. Only newly inserted work reserves the v2 cap.
CREATE OR REPLACE FUNCTION telegram_delivery_account() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='INSERT' THEN
        PERFORM 1 FROM telegram_delivery_budget WHERE id=1 FOR UPDATE;
        IF NOT FOUND THEN RAISE EXCEPTION 'telegram_delivery_budget_missing'; END IF;
        IF NEW.expires_at<date_trunc('second',clock_timestamp()) THEN
            UPDATE live_setups SET telegram_delivery_status='expired',telegram_delivery_updated_at=clock_timestamp() WHERE id=NEW.setup_id;
            RETURN NULL;
        END IF;
        NEW.reserved_bytes:=octet_length(NEW.payload::text)+CASE NEW.wire_version WHEN 1 THEN 32768 ELSE 393216 END;
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

CREATE FUNCTION telegram_delivery_frozen_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.wire_version<>OLD.wire_version THEN
        RAISE EXCEPTION 'telegram_wire_version_immutable';
    END IF;
    IF OLD.request_body IS NOT NULL AND
        (NEW.request_body IS DISTINCT FROM OLD.request_body OR NEW.request_body_hash IS DISTINCT FROM OLD.request_body_hash
         OR NEW.render_diagnostic IS DISTINCT FROM OLD.render_diagnostic) THEN
        RAISE EXCEPTION 'telegram_frozen_body_immutable';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER telegram_delivery_frozen_identity BEFORE UPDATE ON telegram_delivery_outbox
    FOR EACH ROW EXECUTE FUNCTION telegram_delivery_frozen_identity();
