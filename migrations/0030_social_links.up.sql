-- The camp's social profiles: YouTube joins Instagram, Facebook and TikTok, and the empty
-- placeholders get the real URLs (values an admin already set are kept).
INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('social.youtube', 'social', 'string', '""', true, 'YouTube URL')
ON CONFLICT (key) DO NOTHING;

UPDATE settings SET value = v.url, updated_at = now()
FROM (VALUES
    ('social.instagram', '"https://www.instagram.com/mouzoun_mssici/"'::jsonb),
    ('social.tiktok',    '"https://www.tiktok.com/@mouzoun.mssici"'::jsonb),
    ('social.youtube',   '"https://www.youtube.com/@MouzounMssici"'::jsonb),
    ('social.facebook',  '"https://www.facebook.com/profile.php?id=61596592589247"'::jsonb)
) AS v(key, url)
WHERE settings.key = v.key AND settings.value = '""'::jsonb;
