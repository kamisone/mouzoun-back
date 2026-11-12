-- 0015_booking_system: what the booking funnel, payments, self-service and back-office need on
-- top of 0008. Additive only (new columns, tables, permissions and settings). See
-- docs/booking/README.md.

-- ---- Customers ---------------------------------------------------------------------------------
ALTER TABLE customers
    -- Stripe customer that owns a saved card (balance charged off-session, with consent).
    ADD COLUMN stripe_customer_id text UNIQUE,
    ADD COLUMN whatsapp_preferred boolean NOT NULL DEFAULT false,
    -- GDPR / Law 09-08 erasure: personal fields are blanked, accounting records are kept.
    ADD COLUMN anonymized_at      timestamptz;

-- ---- Bookings ----------------------------------------------------------------------------------
ALTER TABLE bookings
    -- What the guest chose at checkout among the options of the rate plan.
    ADD COLUMN payment_option          text CHECK (payment_option IN ('full', 'deposit', 'on_site')),
    -- How the balance of a deposit booking is collected.
    ADD COLUMN balance_collection      text CHECK (balance_collection IN ('saved_card', 'payment_link', 'on_site')),
    -- Provider payment method saved with the guest's explicit consent (off-session balance).
    ADD COLUMN saved_payment_method_id text,
    ADD COLUMN balance_consent_at      timestamptz,
    -- Version of the terms and conditions the guest accepted (legal proof).
    ADD COLUMN terms_version           text,
    ADD COLUMN terms_accepted_at       timestamptz,
    ADD COLUMN checked_in_at           timestamptz,
    ADD COLUMN checked_out_at          timestamptz,
    -- Online pre-check-in (guest register) completed by the guest or staff.
    ADD COLUMN precheckin_completed_at timestamptz,
    -- Phone / agency bookings may be created without any email to the guest.
    ADD COLUMN skip_guest_emails       boolean NOT NULL DEFAULT false,
    -- Request id of the checkout that created it (logs and idempotency audits).
    ADD COLUMN checkout_request_id     text;
CREATE INDEX bookings_balance_due_idx ON bookings (balance_due_on)
    WHERE status = 'confirmed' AND balance_due_on IS NOT NULL;
CREATE INDEX bookings_created_idx ON bookings (created_at DESC);

ALTER TABLE booking_items
    -- Guide language chosen for an activity, preferred time for 'anytime' / 'on_request' ones.
    ADD COLUMN guide_language  text REFERENCES locales (code) ON UPDATE CASCADE ON DELETE SET NULL,
    ADD COLUMN requested_time  time,
    -- Extra added to a stay item ("pitch of your choice", breakfast...): the item it belongs to
    -- is parent_item_id; the specific unit the guest picked is kept here for display.
    ADD COLUMN requested_unit_id uuid REFERENCES accommodation_units (id) ON DELETE SET NULL,
    ADD COLUMN cancelled_at    timestamptz;

-- Magic links ("Find my booking"): random tokens, stored hashed, expiring, one booking each.
CREATE TABLE booking_access_tokens (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id  uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    token_hash  bytea NOT NULL UNIQUE,
    expires_at  timestamptz NOT NULL,
    used_at     timestamptz,
    created_ip  inet,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX booking_access_tokens_booking_idx ON booking_access_tokens (booking_id);

-- Changes of dates / guests / items asked by the guest (or prepared by staff) that need a
-- decision, with the re-quote that was shown.
CREATE TABLE booking_change_requests (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id       uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    kind             text NOT NULL CHECK (kind IN ('dates', 'guests', 'items', 'other')),
    status           text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'declined',
                         'cancelled', 'applied')),
    requested        jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(requested) = 'object'),
    quote            jsonb,
    -- New total minus old total (negative = refund due), in the booking currency.
    difference_minor bigint,
    message          text,
    requested_by     text NOT NULL CHECK (requested_by IN ('customer', 'staff')),
    decided_by       uuid REFERENCES users (id) ON DELETE SET NULL,
    decided_at       timestamptz,
    decision_note    text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX booking_change_requests_booking_idx ON booking_change_requests (booking_id);
CREATE INDEX booking_change_requests_pending_idx ON booking_change_requests (created_at) WHERE status = 'pending';
CREATE TRIGGER booking_change_requests_set_updated_at BEFORE UPDATE ON booking_change_requests
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Internal notes timeline (bookings.internal_notes stays the pinned note).
CREATE TABLE booking_notes (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    booking_id uuid NOT NULL REFERENCES bookings (id) ON DELETE CASCADE,
    author_id  uuid REFERENCES users (id) ON DELETE SET NULL,
    body       text NOT NULL CHECK (length(btrim(body)) > 0),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX booking_notes_booking_idx ON booking_notes (booking_id, created_at);

-- Work the system hands to people: refunds of offline payments, failed balance charges,
-- no-show proposals (never automatic), payments that arrived after a hold expired...
CREATE TABLE staff_tasks (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind        text NOT NULL CHECK (kind IN ('refund_offline', 'balance_failed', 'no_show_review',
                    'change_request', 'payment_after_expiry', 'webhook_error', 'other')),
    status      text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'done', 'dismissed')),
    booking_id  uuid REFERENCES bookings (id) ON DELETE CASCADE,
    payment_id  uuid REFERENCES payments (id) ON DELETE SET NULL,
    title       text NOT NULL,
    details     jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(details) = 'object'),
    -- One open task per (kind, booking) when set: makes job-created tasks idempotent.
    dedupe_key  text UNIQUE,
    due_at      timestamptz,
    assigned_to uuid REFERENCES users (id) ON DELETE SET NULL,
    resolved_by uuid REFERENCES users (id) ON DELETE SET NULL,
    resolved_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX staff_tasks_open_idx ON staff_tasks (created_at) WHERE status = 'open';
CREATE INDEX staff_tasks_booking_idx ON staff_tasks (booking_id);
CREATE TRIGGER staff_tasks_set_updated_at BEFORE UPDATE ON staff_tasks
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---- Payments ----------------------------------------------------------------------------------
ALTER TABLE refunds
    ADD COLUMN idempotency_key text UNIQUE,
    ADD COLUMN failure_reason  text,
    ADD COLUMN processed_at    timestamptz;

-- ---- Catalogue: camping specifics --------------------------------------------------------------
ALTER TABLE accommodation_units
    -- Position on the illustrated site map, in % of its width / height (admin-placed).
    ADD COLUMN map_x numeric(5, 2) CHECK (map_x BETWEEN 0 AND 100),
    ADD COLUMN map_y numeric(5, 2) CHECK (map_y BETWEEN 0 AND 100);

ALTER TABLE accommodation_types
    -- Extra charged when the guest picks a specific pitch on the map (NULL = free choice).
    ADD COLUMN pitch_selection_extra_id uuid REFERENCES extras (product_id) ON DELETE SET NULL;

ALTER TABLE activities
    -- Bookable by day visitors without a stay (otherwise only added to a stay).
    ADD COLUMN standalone_bookable boolean NOT NULL DEFAULT true;

-- EU Omnibus: a struck-through price must be the lowest price of the last 30 days. The daily
-- job records the lowest nightly price seen for each product and service date.
CREATE TABLE price_observations (
    product_id   uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    service_date date NOT NULL,
    observed_on  date NOT NULL,
    amount_minor bigint NOT NULL CHECK (amount_minor >= 0),
    currency     char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    PRIMARY KEY (product_id, service_date, observed_on)
);
CREATE INDEX price_observations_observed_idx ON price_observations (observed_on);

-- ---- Permissions -------------------------------------------------------------------------------
INSERT INTO permissions (code, module, description) VALUES
    ('bookings.cancel',          'bookings',  'Cancel bookings and booking items'),
    ('bookings.override_price',  'bookings',  'Override prices and apply manual discounts'),
    ('payments.refund',          'payments',  'Issue refunds'),
    ('payments.record_offline',  'payments',  'Record cash, bank transfer and card terminal payments'),
    ('rates.write',              'pricing',   'Edit the rates and availability grid'),
    ('guests.read_identity',     'customers', 'See guest identity document numbers'),
    ('reports.read',             'reports',   'View reports'),
    ('activities.manifest',      'inventory', 'View activity manifests');

INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, p.code
FROM roles r
JOIN permissions p ON p.code IN ('bookings.cancel', 'bookings.override_price', 'payments.refund',
        'payments.record_offline', 'rates.write', 'guests.read_identity', 'reports.read', 'activities.manifest')
WHERE r.code IN ('super_admin', 'manager')
   OR (r.code = 'reception' AND p.code IN ('bookings.cancel', 'payments.record_offline',
        'guests.read_identity', 'activities.manifest'))
   OR (r.code = 'guide' AND p.code = 'activities.manifest')
ON CONFLICT DO NOTHING;

-- ---- Settings ----------------------------------------------------------------------------------
INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('booking.request_answer_hours',  'booking', 'number',  '24',    true,  'Hours the camp takes to answer a booking request'),
    ('booking.payment_link_hours',    'booking', 'number',  '48',    false, 'Hours an accepted request stays held while the guest pays'),
    ('booking.balance_due_days',      'booking', 'number',  '14',    true,  'Days before arrival the balance of a deposit booking is due'),
    ('booking.balance_auto_charge',   'booking', 'boolean', 'false', true,  'Offer to charge the balance on the saved card (otherwise a payment link is emailed)'),
    ('booking.balance_reminder_days', 'booking', 'number',  '3',     false, 'Days before the balance due date the payment link is emailed'),
    ('booking.balance_grace_days',    'booking', 'number',  '3',     false, 'Days staff have after a failed balance charge before any cancellation'),
    ('booking.standalone_activities', 'booking', 'boolean', 'true',  true,  'Day visitors may book activities without a stay'),
    ('booking.calendar_show_prices',  'booking', 'boolean', 'true',  true,  'Show the "from" price per night in the date picker'),
    ('booking.urgency_threshold',     'booking', 'number',  '2',     true,  'Show "Only N left" when at most N units remain'),
    ('booking.self_service_changes',  'booking', 'boolean', 'false', true,  'Guests may change dates or guests online when available (otherwise a request)'),
    ('booking.guest_cancellation',    'booking', 'boolean', 'true',  true,  'Guests may cancel online'),
    ('booking.request_card_authorization', 'booking', 'boolean', 'false', false, 'Authorise the card when a booking request is sent'),
    ('booking.precheckin_reminder_days', 'booking', 'number', '5',   false, 'Days before arrival the pre-check-in reminder is sent'),
    ('booking.magic_link_minutes',    'booking', 'number',  '60',    false, 'Validity of "Find my booking" links'),
    ('booking.pitch_selection',       'booking', 'boolean', 'true',  true,  'Guests may pick a specific pitch on the site map'),
    ('booking.id_retention_days',     'booking', 'number',  '365',   false, 'Days after departure guest ID numbers are kept (legal retention)'),
    ('legal.terms_version',           'legal',   'string',  '"2026-11"', true, 'Version of the terms and conditions accepted at checkout');

-- The booking funnel holds inventory for 15 minutes by default (was 30, never customised).
UPDATE settings SET value = '15' WHERE key = 'booking.hold_minutes' AND value = '30'::jsonb;
