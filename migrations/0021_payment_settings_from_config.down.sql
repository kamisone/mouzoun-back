INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('payments.online_enabled', 'payments', 'boolean', 'false', true, 'Online payment available'),
    ('payments.providers',      'payments', 'json',    '[]',    true, 'Enabled online providers, e.g. ["stripe","paypal"]')
ON CONFLICT (key) DO NOTHING;
