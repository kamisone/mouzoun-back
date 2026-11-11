-- 0010_operations: inquiries, newsletter, transactional emails, outbox, audit log.

CREATE TABLE inquiries (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind                 text NOT NULL DEFAULT 'contact' CHECK (kind IN ('contact', 'quote', 'group', 'event',
                             'partnership', 'press')),
    customer_id          uuid REFERENCES customers (id) ON DELETE SET NULL,
    product_id           uuid REFERENCES products (id) ON DELETE SET NULL,
    name                 text NOT NULL,
    email                citext NOT NULL,
    phone                text,
    locale               text REFERENCES locales (code) ON UPDATE CASCADE ON DELETE SET NULL,
    subject              text,
    message              text NOT NULL,
    preferred_dates      daterange,
    group_size           integer CHECK (group_size > 0),
    status               text NOT NULL DEFAULT 'new' CHECK (status IN ('new', 'in_progress', 'answered',
                             'converted', 'closed', 'spam')),
    assigned_to          uuid REFERENCES users (id) ON DELETE SET NULL,
    converted_booking_id uuid REFERENCES bookings (id) ON DELETE SET NULL,
    ip_address           inet,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX inquiries_status_idx ON inquiries (status, created_at DESC);

CREATE TABLE newsletter_subscribers (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email              citext NOT NULL UNIQUE,
    locale             text REFERENCES locales (code) ON UPDATE CASCADE ON DELETE SET NULL,
    status             text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'subscribed', 'unsubscribed', 'bounced')),
    source             text,
    confirm_token_hash bytea UNIQUE,
    confirmed_at       timestamptz,
    unsubscribed_at    timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE email_templates (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       citext NOT NULL UNIQUE,     -- booking_confirmation, payment_receipt, pre_arrival, ...
    is_active  boolean NOT NULL DEFAULT true,
    variables  text[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE email_template_translations (
    email_template_id uuid NOT NULL REFERENCES email_templates (id) ON DELETE CASCADE,
    locale            text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    subject           text NOT NULL,
    body_html         text NOT NULL,
    body_text         text NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (email_template_id, locale)
);

CREATE TABLE email_log (
    id                  bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    template_code       text,
    to_email            citext NOT NULL,
    booking_id          uuid REFERENCES bookings (id) ON DELETE SET NULL,
    provider_message_id text,
    status              text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'sent', 'delivered', 'bounced', 'failed')),
    error               text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    sent_at             timestamptz
);
CREATE INDEX email_log_booking_idx ON email_log (booking_id) WHERE booking_id IS NOT NULL;

-- Transactional outbox: rows are written in the same transaction as the state change, then
-- published asynchronously (see docs/database/README.md).
CREATE TABLE outbox_events (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    aggregate_type text NOT NULL,
    aggregate_id   text NOT NULL,
    event_type     text NOT NULL CHECK (event_type ~ '^[a-z_]+\.[a-z_]+$'),    -- e.g. booking.confirmed
    payload        jsonb NOT NULL DEFAULT '{}',
    available_at   timestamptz NOT NULL DEFAULT now(),
    attempts       integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_error     text,
    processed_at   timestamptz,
    created_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX outbox_events_pending_idx ON outbox_events (available_at) WHERE processed_at IS NULL;

CREATE TABLE audit_logs (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    actor_type    text NOT NULL CHECK (actor_type IN ('user', 'customer', 'system', 'webhook')),
    actor_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    action        text NOT NULL CHECK (action ~ '^[a-z_]+\.[a-z_]+$'),          -- e.g. product.update
    entity_type   text NOT NULL,
    entity_id     text NOT NULL,
    changes       jsonb NOT NULL DEFAULT '{}',                                  -- {before, after}
    ip_address    inet,
    user_agent    text,
    request_id    text,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_logs_entity_idx ON audit_logs (entity_type, entity_id, created_at DESC);
CREATE INDEX audit_logs_actor_idx ON audit_logs (actor_user_id, created_at DESC);
