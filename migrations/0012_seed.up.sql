-- 0012_seed: reference data only (no demo content: see `cargo run --bin seed_dev`).

INSERT INTO locales (code, name, native_name, is_rtl, is_default, is_active, sort_order) VALUES
    ('fr', 'French',  'Français', false, true,  true, 1),
    ('en', 'English', 'English',  false, false, true, 2),
    ('ar', 'Arabic',  'العربية',  true,  false, true, 3);

-- Prices are set and charged in EUR; MAD and USD exist for display only.
INSERT INTO currencies (code, name, symbol, minor_units, is_default, is_active) VALUES
    ('EUR', 'Euro',             '€',    2, true,  true),
    ('MAD', 'Moroccan dirham',  'MAD',  2, false, true),
    ('USD', 'US dollar',        '$',    2, false, true);

INSERT INTO guest_categories (code, min_age, max_age, counts_toward_capacity, sort_order) VALUES
    ('adult',  12,   NULL, true,  1),
    ('child',  4,    11,   true,  2),
    ('infant', 0,    3,    false, 3);

INSERT INTO guest_category_translations (guest_category_id, locale, name, description)
SELECT g.id, v.locale, v.name, v.description
FROM guest_categories g
JOIN (VALUES
    ('adult',  'fr', 'Adulte',  '12 ans et plus'),
    ('adult',  'en', 'Adult',   '12 years and over'),
    ('adult',  'ar', 'بالغ',    '12 سنة فما فوق'),
    ('child',  'fr', 'Enfant',  'De 4 à 11 ans'),
    ('child',  'en', 'Child',   'Aged 4 to 11'),
    ('child',  'ar', 'طفل',     'من 4 إلى 11 سنة'),
    ('infant', 'fr', 'Bébé',    'Moins de 4 ans'),
    ('infant', 'en', 'Infant',  'Under 4'),
    ('infant', 'ar', 'رضيع',    'أقل من 4 سنوات')
) AS v (code, locale, name, description) ON v.code = g.code::text;

INSERT INTO permissions (code, module, description) VALUES
    ('dashboard.read',     'dashboard', 'View the dashboard'),
    ('products.read',      'products',  'View products'),
    ('products.write',     'products',  'Create and edit products'),
    ('pricing.read',       'pricing',   'View prices, seasons, rate plans, taxes and promotions'),
    ('pricing.write',      'pricing',   'Edit prices, seasons, rate plans, taxes and promotions'),
    ('inventory.read',     'inventory', 'View availability, units, slots and resources'),
    ('inventory.write',    'inventory', 'Edit availability, units, slots, resources and closures'),
    ('bookings.read',      'bookings',  'View bookings'),
    ('bookings.write',     'bookings',  'Create, modify and cancel bookings'),
    ('payments.read',      'payments',  'View payments and invoices'),
    ('payments.write',     'payments',  'Record payments, issue refunds and invoices'),
    ('customers.read',     'customers', 'View customers'),
    ('customers.write',    'customers', 'Edit customers'),
    ('content.read',       'content',   'View pages, menus, FAQs and banners'),
    ('content.write',      'content',   'Edit pages, menus, FAQs and banners'),
    ('media.write',        'media',     'Upload and organise media'),
    ('reviews.moderate',   'reviews',   'Moderate and reply to reviews'),
    ('inquiries.manage',   'inquiries', 'Handle contact and quote requests'),
    ('settings.write',     'settings',  'Edit site settings'),
    ('users.manage',       'users',     'Manage back-office users and roles'),
    ('audit.read',         'audit',     'Read the audit log');

INSERT INTO roles (code, name, description, is_system) VALUES
    ('super_admin', 'Super admin', 'Full access', true),
    ('manager',     'Manager',     'Runs the camp: everything except users and audit', true),
    ('reception',   'Reception',   'Bookings, payments, customers and inquiries', true),
    ('editor',      'Editor',      'Products, content, media and reviews', true),
    ('guide',       'Guide',       'Reads schedules and bookings', true);

INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, p.code
FROM roles r
JOIN permissions p ON
    (r.code = 'super_admin')
    OR (r.code = 'manager' AND p.code NOT IN ('users.manage', 'audit.read'))
    OR (r.code = 'reception' AND p.code IN ('dashboard.read', 'products.read', 'inventory.read',
        'bookings.read', 'bookings.write', 'payments.read', 'payments.write',
        'customers.read', 'customers.write', 'inquiries.manage'))
    OR (r.code = 'editor' AND p.code IN ('dashboard.read', 'products.read', 'products.write',
        'content.read', 'content.write', 'media.write', 'reviews.moderate'))
    OR (r.code = 'guide' AND p.code IN ('inventory.read', 'bookings.read'));

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('site.name',              'general',  'string',         '"Mouzoun"', true, 'Site name'),
    ('site.tagline',           'general',  'localized_text',
        '{"fr": "Camping au cœur du désert, à M''ssici", "en": "Desert camping in M''ssici", "ar": "تخييم في قلب الصحراء بمسيسي"}',
        true, 'Tagline shown under the logo'),
    ('site.status',            'general',  'string',         '"coming_soon"', true, 'coming_soon | live | maintenance'),
    ('contact.email',          'contact',  'string',         '""', true, 'Public contact email'),
    ('contact.phone',          'contact',  'string',         '""', true, 'Public phone number (E.164)'),
    ('contact.whatsapp',       'contact',  'string',         '""', true, 'WhatsApp number (E.164)'),
    ('contact.address',        'contact',  'localized_text',
        '{"fr": "M''ssici, près d''Alnif et de Rissani, Maroc", "en": "M''ssici, near Alnif and Rissani, Morocco", "ar": "مسيسي، قرب ألنيف والريصاني، المغرب"}',
        true, 'Postal address'),
    ('location.latitude',      'location', 'number',         'null', true, 'Camp latitude (WGS84)'),
    ('location.longitude',     'location', 'number',         'null', true, 'Camp longitude (WGS84)'),
    ('social.instagram',       'social',   'string',         '""', true, 'Instagram URL'),
    ('social.facebook',        'social',   'string',         '""', true, 'Facebook URL'),
    ('social.tiktok',          'social',   'string',         '""', true, 'TikTok URL'),
    ('booking.enabled',        'booking',  'boolean',        'false', true, 'Online booking open to the public'),
    ('booking.hold_minutes',   'booking',  'number',         '30', false, 'Minutes a pending booking holds inventory'),
    ('booking.max_days_ahead', 'booking',  'number',         '365', true, 'How far ahead guests can book'),
    ('booking.timezone',       'booking',  'string',         '"Africa/Casablanca"', true, 'Timezone of dates and slots'),
    ('payments.online_enabled','payments', 'boolean',        'false', true, 'Online payment available'),
    ('payments.providers',     'payments', 'json',           '[]', true, 'Enabled online providers, e.g. ["stripe","paypal"]'),
    ('seo.default_og_media_id','seo',      'media_id',       'null', true, 'Default social sharing image');
