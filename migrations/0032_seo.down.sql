DELETE FROM settings WHERE key IN ('seo.robots_extra', 'seo.sitemap_built_at');
DELETE FROM role_permissions WHERE permission_code = 'seo.manage';
DELETE FROM permissions WHERE code = 'seo.manage';
DROP TABLE not_found_log;
DROP TRIGGER redirects_no_loop ON redirects;
DROP FUNCTION redirects_no_loop();
DROP INDEX redirects_entity_idx;
DROP INDEX redirects_to_path_idx;
-- External targets cannot survive without the source column.
DELETE FROM redirects WHERE to_path !~ '^/';
ALTER TABLE redirects
    DROP CONSTRAINT redirects_external_manual,
    DROP COLUMN created_by,
    DROP COLUMN last_hit_at,
    DROP COLUMN note,
    DROP COLUMN locale,
    DROP COLUMN entity_id,
    DROP COLUMN entity_type,
    DROP COLUMN source;
