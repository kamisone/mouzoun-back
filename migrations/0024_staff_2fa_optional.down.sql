INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('auth.staff_2fa_required', 'account', 'boolean', 'true', false, 'Staff must use a TOTP code (or passkey) to sign in to the back office')
ON CONFLICT (key) DO NOTHING;
