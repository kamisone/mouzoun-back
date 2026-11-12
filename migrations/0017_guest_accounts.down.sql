DELETE FROM settings WHERE key IN ('account.deletion_grace_days', 'account.review_edit_days', 'account.facebook_enabled',
    'account.phone_verification', 'auth.staff_2fa_required');
DELETE FROM role_permissions WHERE permission_code IN ('customers.merge', 'customers.impersonate', 'customers.privacy');
DELETE FROM permissions WHERE code IN ('customers.merge', 'customers.impersonate', 'customers.privacy');
DROP TABLE account_exports;
DROP TABLE account_deletion_requests;
DROP TABLE notification_preferences;
DROP TABLE saved_travellers;
DROP TABLE wishlist_items;
DROP TABLE wishlists;
DROP TABLE user_identities;
DROP INDEX user_sessions_previous_idx;
ALTER TABLE user_sessions DROP COLUMN kind, DROP COLUMN previous_refresh_token_hash, DROP COLUMN persistent,
    DROP COLUMN reauthenticated_at, DROP COLUMN revoked_reason;
ALTER TABLE users DROP COLUMN date_of_birth, DROP COLUMN display_currency, DROP COLUMN avatar_media_id, DROP COLUMN totp_enabled_at;
ALTER TABLE auth_tokens DROP COLUMN data, DROP COLUMN attempts;
DELETE FROM auth_tokens WHERE purpose IN ('email_change', 'booking_claim');
ALTER TABLE auth_tokens DROP CONSTRAINT auth_tokens_purpose_check;
ALTER TABLE auth_tokens ADD CONSTRAINT auth_tokens_purpose_check CHECK (purpose IN ('password_reset', 'email_verification', 'invitation'));
