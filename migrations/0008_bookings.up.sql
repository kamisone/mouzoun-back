-- 0008_bookings: customers, bookings, items, allocations, guest register, payments, invoices.

CREATE TABLE customers (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id              uuid UNIQUE REFERENCES users (id) ON DELETE SET NULL,
    email                citext,
    phone                text,
    first_name           text NOT NULL,
    last_name            text,
    country_code         char(2) CHECK (country_code ~ '^[A-Z]{2}$'),
    preferred_locale     text REFERENCES locales (code) ON UPDATE CASCADE ON DELETE SET NULL,
    marketing_consent    boolean NOT NULL DEFAULT false,
    marketing_consent_at timestamptz,
    internal_notes       text,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    deleted_at           timestamptz
);
CREATE UNIQUE INDEX customers_email_unique ON customers (email) WHERE email IS NOT NULL AND deleted_at IS NULL;

-- Kept as specified; NOT used for invoice numbers: sequences can leave gaps (rolled-back
-- transactions), and invoice numbers must be gap-free per year (see invoice_counters).
CREATE SEQUENCE invoice_number_seq;

CREATE TABLE bookings (
    id                           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    reference                    text NOT NULL UNIQUE CHECK (reference ~ '^MZ-[0-9]{2}-[0-9]{6,}$'),
    customer_id                  uuid NOT NULL REFERENCES customers (id) ON DELETE RESTRICT,
    status                       text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'pending', 'awaiting_payment',
                                     'confirmed', 'checked_in', 'completed', 'cancelled', 'no_show', 'expired')),
    source                       text NOT NULL DEFAULT 'website' CHECK (source IN ('website', 'admin', 'phone', 'email',
                                     'whatsapp', 'walk_in', 'ota', 'agency')),
    external_reference           text,
    locale                       text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    currency                     char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    arrival_date                 date,
    departure_date               date,
    adults                       smallint NOT NULL DEFAULT 0 CHECK (adults >= 0),
    children                     smallint NOT NULL DEFAULT 0 CHECK (children >= 0),
    infants                      smallint NOT NULL DEFAULT 0 CHECK (infants >= 0),
    subtotal_minor               bigint NOT NULL DEFAULT 0 CHECK (subtotal_minor >= 0),
    discount_minor               bigint NOT NULL DEFAULT 0 CHECK (discount_minor >= 0),
    tax_minor                    bigint NOT NULL DEFAULT 0 CHECK (tax_minor >= 0),
    total_minor                  bigint NOT NULL DEFAULT 0 CHECK (total_minor >= 0),
    amount_paid_minor            bigint NOT NULL DEFAULT 0,   -- cache, recomputed with payments in one transaction
    deposit_due_minor            bigint NOT NULL DEFAULT 0 CHECK (deposit_due_minor >= 0),
    balance_due_on               date,
    rate_plan_id                 uuid REFERENCES rate_plans (id) ON DELETE RESTRICT,
    cancellation_policy_snapshot jsonb,
    hold_expires_at              timestamptz,
    estimated_arrival_time       time,
    special_requests             text,
    internal_notes               text,
    confirmed_at                 timestamptz,
    cancelled_at                 timestamptz,
    cancellation_reason          text,
    cancelled_by                 text CHECK (cancelled_by IN ('customer', 'staff', 'system')),
    created_by                   uuid REFERENCES users (id) ON DELETE SET NULL,
    ip_address                   inet,
    user_agent                   text,
    created_at                   timestamptz NOT NULL DEFAULT now(),
    updated_at                   timestamptz NOT NULL DEFAULT now(),
    CHECK (departure_date > arrival_date)
);
CREATE INDEX bookings_customer_idx ON bookings (customer_id);
CREATE INDEX bookings_status_idx ON bookings (status);
CREATE INDEX bookings_arrival_idx ON bookings (arrival_date);
CREATE INDEX bookings_hold_idx ON bookings (hold_expires_at) WHERE status IN ('pending', 'awaiting_payment');

-- Gap-free invoice numbering: one counter row per year, locked FOR UPDATE by the numbering
-- service inside the transaction that issues the invoice (see deviations in docs/database).
-- Booking reference MZ-<yy>-<number>: the number restarts at 1 every year. Incremented by one
-- atomic upsert outside the booking transaction (gaps are harmless, no long-held lock).
CREATE TABLE booking_reference_counters (
    year        smallint PRIMARY KEY CHECK (year BETWEEN 2000 AND 2999),
    last_number integer NOT NULL DEFAULT 0 CHECK (last_number >= 0),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE invoice_counters (
    year        smallint PRIMARY KEY CHECK (year BETWEEN 2000 AND 2999),
    last_number integer NOT NULL DEFAULT 0 CHECK (last_number >= 0),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE booking_items (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id       uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    parent_item_id   uuid REFERENCES booking_items (id) ON DELETE CASCADE,   -- extras of a stay, package parts
    product_id       uuid NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    product_kind     text NOT NULL CHECK (product_kind IN ('accommodation', 'activity', 'extra', 'package')),
    rate_plan_id     uuid REFERENCES rate_plans (id) ON DELETE RESTRICT,
    activity_slot_id uuid REFERENCES activity_slots (id) ON DELETE RESTRICT,
    service_period   daterange,
    starts_at        timestamptz,
    quantity         integer NOT NULL DEFAULT 1 CHECK (quantity > 0),
    unit_price_minor bigint NOT NULL DEFAULT 0 CHECK (unit_price_minor >= 0),
    subtotal_minor   bigint NOT NULL DEFAULT 0 CHECK (subtotal_minor >= 0),
    discount_minor   bigint NOT NULL DEFAULT 0 CHECK (discount_minor >= 0),
    tax_minor        bigint NOT NULL DEFAULT 0 CHECK (tax_minor >= 0),
    total_minor      bigint NOT NULL DEFAULT 0 CHECK (total_minor >= 0),
    price_breakdown  jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(price_breakdown) = 'array'),
    product_snapshot jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(product_snapshot) = 'object'),
    status           text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'cancelled')),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX booking_items_booking_idx ON booking_items (booking_id);
CREATE INDEX booking_items_slot_idx ON booking_items (activity_slot_id) WHERE activity_slot_id IS NOT NULL;
CREATE INDEX booking_items_product_idx ON booking_items (product_id);

CREATE TABLE booking_item_guests (
    booking_item_id   uuid NOT NULL REFERENCES booking_items (id) ON DELETE CASCADE,
    guest_category_id uuid NOT NULL REFERENCES guest_categories (id) ON DELETE RESTRICT,
    guest_count       integer NOT NULL CHECK (guest_count > 0),
    created_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (booking_item_id, guest_category_id)
);

CREATE TABLE unit_allocations (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_item_id uuid NOT NULL REFERENCES booking_items (id) ON DELETE CASCADE,
    unit_id         uuid NOT NULL REFERENCES accommodation_units (id) ON DELETE RESTRICT,
    stay            daterange NOT NULL CHECK (NOT isempty(stay)),
    released_at     timestamptz,
    created_at      timestamptz NOT NULL DEFAULT now(),
    EXCLUDE USING gist (unit_id WITH =, stay WITH &&) WHERE (released_at IS NULL)
);
CREATE INDEX unit_allocations_item_idx ON unit_allocations (booking_item_id);

CREATE TABLE resource_assignments (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    resource_id      uuid NOT NULL REFERENCES resources (id) ON DELETE RESTRICT,
    booking_item_id  uuid REFERENCES booking_items (id) ON DELETE CASCADE,
    activity_slot_id uuid REFERENCES activity_slots (id) ON DELETE CASCADE,
    period           tstzrange NOT NULL CHECK (NOT isempty(period)),
    released_at      timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now(),
    CHECK (booking_item_id IS NOT NULL OR activity_slot_id IS NOT NULL),
    EXCLUDE USING gist (resource_id WITH =, period WITH &&) WHERE (released_at IS NULL)
);
CREATE INDEX resource_assignments_item_idx ON resource_assignments (booking_item_id);

-- Guest register for the Moroccan police form ("fiche de police").
CREATE TABLE booking_guests (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id             uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    guest_category_id      uuid REFERENCES guest_categories (id) ON DELETE SET NULL,
    is_lead                boolean NOT NULL DEFAULT false,
    first_name             text NOT NULL,
    last_name              text NOT NULL,
    nationality            char(2) CHECK (nationality ~ '^[A-Z]{2}$'),
    date_of_birth          date,
    id_document_type       text CHECK (id_document_type IN ('passport', 'national_id', 'residence_permit',
                               'driving_licence', 'other')),
    id_document_number_enc bytea,        -- encrypted by the application, never stored in clear
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX booking_guests_single_lead ON booking_guests (booking_id) WHERE is_lead;
CREATE INDEX booking_guests_booking_idx ON booking_guests (booking_id);

CREATE TABLE booking_status_history (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    booking_id  uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    from_status text,
    to_status   text NOT NULL,
    changed_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    note        text,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX booking_status_history_booking_idx ON booking_status_history (booking_id, created_at);

CREATE TABLE promotion_redemptions (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    promotion_id   uuid NOT NULL REFERENCES promotions (id) ON DELETE RESTRICT,
    booking_id     uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    customer_id    uuid REFERENCES customers (id) ON DELETE SET NULL,
    discount_minor bigint NOT NULL CHECK (discount_minor >= 0),
    created_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (promotion_id, booking_id)
);

CREATE TABLE payments (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id          uuid NOT NULL REFERENCES bookings (id) ON DELETE RESTRICT,
    provider            text NOT NULL CHECK (provider IN ('cmi', 'stripe', 'paypal', 'cash', 'bank_transfer',
                            'card_terminal', 'other')),
    purpose             text NOT NULL CHECK (purpose IN ('deposit', 'balance', 'full', 'extra')),
    status              text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'requires_action', 'authorized',
                            'succeeded', 'failed', 'cancelled', 'refunded', 'partially_refunded')),
    amount_minor        bigint NOT NULL CHECK (amount_minor > 0),
    currency            char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    provider_payment_id text,
    provider_payload    jsonb NOT NULL DEFAULT '{}',
    idempotency_key     text NOT NULL UNIQUE,
    failure_reason      text,
    paid_at             timestamptz,
    received_by         uuid REFERENCES users (id) ON DELETE SET NULL,   -- staff, for offline payments
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    UNIQUE (provider, provider_payment_id)
);
CREATE INDEX payments_booking_idx ON payments (booking_id);

CREATE TABLE refunds (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    payment_id         uuid NOT NULL REFERENCES payments (id) ON DELETE RESTRICT,
    amount_minor       bigint NOT NULL CHECK (amount_minor > 0),
    status             text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'succeeded', 'failed')),
    reason             text,
    provider_refund_id text,
    created_by         uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX refunds_payment_idx ON refunds (payment_id);

CREATE TABLE payment_webhook_events (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    provider     text NOT NULL,
    event_id     text NOT NULL,
    event_type   text NOT NULL,
    payload      jsonb NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz,
    error        text,
    UNIQUE (provider, event_id)
);

CREATE TABLE invoices (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    number          text NOT NULL UNIQUE,          -- gap-free per year: <yyyy>-<000001>
    booking_id      uuid NOT NULL REFERENCES bookings (id) ON DELETE RESTRICT,
    kind            text NOT NULL DEFAULT 'invoice' CHECK (kind IN ('invoice', 'credit_note', 'receipt')),
    billing_details jsonb NOT NULL DEFAULT '{}',
    lines           jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(lines) = 'array'),
    subtotal_minor  bigint NOT NULL,
    tax_minor       bigint NOT NULL,
    total_minor     bigint NOT NULL,
    currency        char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    pdf_media_id    uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    issued_at       timestamptz NOT NULL DEFAULT now(),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX invoices_booking_idx ON invoices (booking_id);
