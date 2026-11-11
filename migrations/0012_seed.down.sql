DELETE FROM settings WHERE key IN ('site.name', 'site.tagline', 'site.status', 'contact.email', 'contact.phone',
    'contact.whatsapp', 'contact.address', 'location.latitude', 'location.longitude', 'social.instagram',
    'social.facebook', 'social.tiktok', 'booking.enabled', 'booking.hold_minutes', 'booking.max_days_ahead',
    'booking.timezone', 'payments.online_enabled', 'payments.providers', 'seo.default_og_media_id');
DELETE FROM role_permissions WHERE role_id IN (SELECT id FROM roles WHERE code IN ('super_admin', 'manager', 'reception', 'editor', 'guide'));
DELETE FROM roles WHERE code IN ('super_admin', 'manager', 'reception', 'editor', 'guide');
DELETE FROM permissions WHERE code IN ('dashboard.read', 'products.read', 'products.write', 'pricing.read',
    'pricing.write', 'inventory.read', 'inventory.write', 'bookings.read', 'bookings.write', 'payments.read',
    'payments.write', 'customers.read', 'customers.write', 'content.read', 'content.write', 'media.write',
    'reviews.moderate', 'inquiries.manage', 'settings.write', 'users.manage', 'audit.read');
DELETE FROM guest_category_translations WHERE guest_category_id IN (SELECT id FROM guest_categories WHERE code IN ('adult', 'child', 'infant'));
DELETE FROM guest_categories WHERE code IN ('adult', 'child', 'infant');
DELETE FROM currencies WHERE code IN ('EUR', 'MAD', 'USD');
DELETE FROM locales WHERE code IN ('fr', 'en', 'ar');
