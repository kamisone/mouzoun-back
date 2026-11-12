-- 0017_guest_accounts: guest accounts (users with is_staff = false, linked 1:1 to customers),
-- rotating sessions, staff 2FA, social identities, wishlists, saved travellers, notification
-- preferences, data export and deletion. Additive only. See docs/accounts/README.md.

-- ---- One-time tokens ---------------------------------------------------------------------------
-- email_change: the new address is in `data`; booking_claim: the booking is in `data`.
-- (Sign-in codes and magic links live in Redis: they are sent to addresses that may not have an
-- account yet, and a users row must never be created before the address is verified.)
ALTER TABLE auth_tokens DROP CONSTRAINT auth_tokens_purpose_check;
ALTER TABLE auth_tokens ADD CONSTRAINT auth_tokens_purpose_check
    CHECK (purpose IN ('password_reset', 'email_verification', 'invitation', 'email_change', 'booking_claim'));
ALTER TABLE auth_tokens
    ADD COLUMN data     jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(data) = 'object'),
    ADD COLUMN attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0);

-- ---- Users -------------------------------------------------------------------------------------
ALTER TABLE users
    ADD COLUMN date_of_birth    date,
    ADD COLUMN display_currency char(3) REFERENCES currencies (code) ON UPDATE CASCADE ON DELETE SET NULL,
    ADD COLUMN avatar_media_id  uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    -- Staff only: when the TOTP secret was confirmed (2FA enrolled).
    ADD COLUMN totp_enabled_at  timestamptz;

-- ---- Sessions: one row per sign-in (a refresh-token family) ------------------------------------
ALTER TABLE user_sessions
    ADD COLUMN kind                        text NOT NULL DEFAULT 'guest' CHECK (kind IN ('guest', 'staff')),
    -- The token rotated away at the last refresh: presenting it again means it was stolen.
    ADD COLUMN previous_refresh_token_hash bytea,
    -- false = "This is a shared device": browser-session cookies only.
    ADD COLUMN persistent                  boolean NOT NULL DEFAULT true,
    -- Last time the user proved who they are (code, password, passkey): sensitive actions need
    -- it within 10 minutes.
    ADD COLUMN reauthenticated_at          timestamptz,
    ADD COLUMN revoked_reason              text;
CREATE INDEX user_sessions_previous_idx ON user_sessions (previous_refresh_token_hash)
    WHERE previous_refresh_token_hash IS NOT NULL;

-- ---- Social sign-in --------------------------------------------------------------------------
CREATE TABLE user_identities (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id          uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    provider         text NOT NULL CHECK (provider IN ('google', 'apple', 'facebook')),
    provider_subject text NOT NULL,
    email            citext,
    email_verified   boolean NOT NULL DEFAULT false,
    created_at       timestamptz NOT NULL DEFAULT now(),
    last_used_at     timestamptz,
    UNIQUE (provider, provider_subject)
);
CREATE INDEX user_identities_user_idx ON user_identities (user_id);

-- ---- Wishlists -------------------------------------------------------------------------------
CREATE TABLE wishlists (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name        text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 80),
    is_default  boolean NOT NULL DEFAULT false,
    -- Read-only share link (random, NULL = not shared).
    share_token text UNIQUE,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX wishlists_single_default ON wishlists (user_id) WHERE is_default;
CREATE TRIGGER wishlists_set_updated_at BEFORE UPDATE ON wishlists FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE wishlist_items (
    wishlist_id uuid NOT NULL REFERENCES wishlists (id) ON DELETE CASCADE,
    product_id  uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    note        text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (wishlist_id, product_id)
);
CREATE INDEX wishlist_items_product_idx ON wishlist_items (product_id);

-- ---- Saved travellers -------------------------------------------------------------------------
CREATE TABLE saved_travellers (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id                uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    first_name             text NOT NULL,
    last_name              text NOT NULL,
    guest_category_id      uuid REFERENCES guest_categories (id) ON DELETE SET NULL,
    date_of_birth          date,
    nationality            char(2) CHECK (nationality ~ '^[A-Z]{2}$'),
    id_document_type       text CHECK (id_document_type IN ('passport', 'national_id', 'residence_permit',
                               'driving_licence', 'other')),
    id_document_number_enc bytea,
    -- Explicit consent to keep the ID document (required for id_document_number_enc).
    id_consent_at          timestamptz,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CHECK (id_document_number_enc IS NULL OR id_consent_at IS NOT NULL)
);
CREATE INDEX saved_travellers_user_idx ON saved_travellers (user_id);
CREATE TRIGGER saved_travellers_set_updated_at BEFORE UPDATE ON saved_travellers FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---- Notification preferences ----------------------------------------------------------------
-- Booking updates and security emails are transactional and always sent: not stored here.
CREATE TABLE notification_preferences (
    user_id    uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    category   text NOT NULL CHECK (category IN ('reminders', 'offers', 'reviews')),
    channel    text NOT NULL CHECK (channel IN ('email', 'sms', 'whatsapp')),
    enabled    boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, category, channel)
);
CREATE TRIGGER notification_preferences_set_updated_at BEFORE UPDATE ON notification_preferences FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---- Privacy -----------------------------------------------------------------------------------
CREATE TABLE account_deletion_requests (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id           uuid REFERENCES users (id) ON DELETE SET NULL,
    customer_id       uuid REFERENCES customers (id) ON DELETE SET NULL,
    requested_at      timestamptz NOT NULL DEFAULT now(),
    scheduled_for     timestamptz NOT NULL,
    cancel_token_hash bytea UNIQUE,
    cancelled_at      timestamptz,
    completed_at      timestamptz,
    requested_by      text NOT NULL DEFAULT 'customer' CHECK (requested_by IN ('customer', 'staff'))
);
CREATE UNIQUE INDEX account_deletion_requests_open ON account_deletion_requests (user_id)
    WHERE cancelled_at IS NULL AND completed_at IS NULL;
CREATE INDEX account_deletion_requests_due_idx ON account_deletion_requests (scheduled_for)
    WHERE cancelled_at IS NULL AND completed_at IS NULL;

-- Data exports: generated by a job, downloaded with a token from the email, kept 7 days.
CREATE TABLE account_exports (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id       uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash    bytea UNIQUE,
    data          jsonb,
    requested_at  timestamptz NOT NULL DEFAULT now(),
    ready_at      timestamptz,
    expires_at    timestamptz,
    downloaded_at timestamptz
);
CREATE INDEX account_exports_pending_idx ON account_exports (requested_at) WHERE ready_at IS NULL;

-- ---- Permissions and settings ----------------------------------------------------------------
INSERT INTO permissions (code, module, description) VALUES
    ('customers.merge',       'customers', 'Merge duplicate customers'),
    ('customers.impersonate', 'customers', 'View the site as a customer (read-only, audited)'),
    ('customers.privacy',     'customers', 'Export customer data and process deletions');
INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, p.code FROM roles r
JOIN permissions p ON p.code IN ('customers.merge', 'customers.impersonate', 'customers.privacy')
WHERE r.code IN ('super_admin', 'manager')
ON CONFLICT DO NOTHING;

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('account.deletion_grace_days', 'account', 'number',  '14',    false, 'Days before a requested account deletion is carried out'),
    ('account.review_edit_days',    'account', 'number',  '30',    true,  'Days a guest can edit their review'),
    ('account.facebook_enabled',    'account', 'boolean', 'false', true,  'Offer "Continue with Facebook"'),
    ('account.phone_verification',  'account', 'boolean', 'false', true,  'Verify phone numbers by SMS / WhatsApp code'),
    ('auth.staff_2fa_required',     'account', 'boolean', 'true',  false, 'Staff must use a TOTP code (or passkey) to sign in to the back office');
