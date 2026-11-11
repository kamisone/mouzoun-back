DROP TRIGGER email_log_set_updated_at ON email_log;
DROP INDEX email_log_event_idx;
DELETE FROM email_log WHERE status = 'sending';
ALTER TABLE email_log DROP CONSTRAINT email_log_status_check;
ALTER TABLE email_log ADD CONSTRAINT email_log_status_check
    CHECK (status IN ('queued', 'sent', 'delivered', 'bounced', 'failed'));
ALTER TABLE email_log
    DROP CONSTRAINT email_log_event_recipient_template_key,
    DROP COLUMN updated_at,
    DROP COLUMN attempts,
    DROP COLUMN subject,
    DROP COLUMN locale,
    DROP COLUMN outbox_event_id;

DROP TRIGGER outbox_events_notify ON outbox_events;
DROP FUNCTION notify_outbox_events();
DROP INDEX outbox_events_dead_idx;
DROP INDEX outbox_events_pending_idx;
ALTER TABLE outbox_events
    DROP CONSTRAINT outbox_events_single_outcome,
    DROP COLUMN dedupe_key,
    DROP COLUMN failed_at;
CREATE INDEX outbox_events_pending_idx ON outbox_events (available_at) WHERE processed_at IS NULL;
