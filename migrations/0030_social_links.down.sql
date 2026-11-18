UPDATE settings SET value = '""'::jsonb, updated_at = now()
WHERE (key, value) IN (
    ('social.instagram', '"https://www.instagram.com/mouzoun_mssici/"'::jsonb),
    ('social.tiktok',    '"https://www.tiktok.com/@mouzoun.mssici"'::jsonb),
    ('social.facebook',  '"https://www.facebook.com/profile.php?id=61596592589247"'::jsonb)
);
DELETE FROM settings WHERE key = 'social.youtube';
