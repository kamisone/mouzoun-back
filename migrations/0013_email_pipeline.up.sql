-- 0013_email_pipeline: outbox dispatcher (dead events, de-duplication, instant wake-up) and
-- idempotent email delivery. See docs/email/README.md.

-- ---- Outbox ------------------------------------------------------------------------------------
ALTER TABLE outbox_events
    -- Set when the event is given up on (permanent error or MAX_ATTEMPTS reached). A dead event
    -- is never claimed again until an operator replays it (failed_at back to NULL).
    ADD COLUMN failed_at  timestamptz,
    -- Optional idempotency key for events that may be produced more than once (scheduled emails:
    -- 'booking.pre_arrival_due:<booking_id>'). INSERT ... ON CONFLICT (dedupe_key) DO NOTHING.
    ADD COLUMN dedupe_key text UNIQUE,
    ADD CONSTRAINT outbox_events_single_outcome CHECK (processed_at IS NULL OR failed_at IS NULL);

DROP INDEX outbox_events_pending_idx;
CREATE INDEX outbox_events_pending_idx ON outbox_events (available_at, id)
    WHERE processed_at IS NULL AND failed_at IS NULL;
CREATE INDEX outbox_events_dead_idx ON outbox_events (failed_at) WHERE failed_at IS NOT NULL;

-- Wakes the dispatchers (LISTEN outbox_events) as soon as an inserting transaction commits;
-- they also poll, so a missed notification only delays an event by one poll interval.
CREATE FUNCTION notify_outbox_events() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_notify('outbox_events', '');
    RETURN NULL;
END;
$$;

CREATE TRIGGER outbox_events_notify AFTER INSERT ON outbox_events
    FOR EACH STATEMENT EXECUTE FUNCTION notify_outbox_events();

-- ---- Email log: one row per (event, recipient, template) = at most one email -------------------
ALTER TABLE email_log
    ADD COLUMN outbox_event_id bigint REFERENCES outbox_events (id) ON DELETE SET NULL,
    ADD COLUMN locale          text REFERENCES locales (code) ON UPDATE CASCADE ON DELETE SET NULL,
    ADD COLUMN subject         text,
    ADD COLUMN attempts        integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    ADD COLUMN updated_at      timestamptz NOT NULL DEFAULT now(),
    ADD CONSTRAINT email_log_event_recipient_template_key UNIQUE (outbox_event_id, to_email, template_code);

-- 'sending' is committed right before the provider is called: a row found in that state after a
-- crash means "delivery unknown", and it is never sent again automatically.
ALTER TABLE email_log DROP CONSTRAINT email_log_status_check;
ALTER TABLE email_log ADD CONSTRAINT email_log_status_check
    CHECK (status IN ('queued', 'sending', 'sent', 'delivered', 'bounced', 'failed'));

CREATE INDEX email_log_event_idx ON email_log (outbox_event_id) WHERE outbox_event_id IS NOT NULL;

CREATE TRIGGER email_log_set_updated_at BEFORE UPDATE ON email_log
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();
