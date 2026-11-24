-- 0032_seo: site-wide redirects (served by the Nuxt server before any page renders), the 404
-- log, the SEO permission and settings. Sitemaps are built from the route registry
-- (back/src/modules/seo/routes.rs), nothing is stored for them. See back/docs/seo/README.md.

-- ---- Redirects -----------------------------------------------------------------------------------
-- `auto` rows are written by `seo::redirects` inside the transaction that changed a URL; `manual`
-- rows by staff (Admin › SEO). Paths are stored normalised: decoded, NFC, no trailing slash.
ALTER TABLE redirects
    ADD COLUMN source      text NOT NULL DEFAULT 'manual' CHECK (source IN ('auto', 'manual')),
    ADD COLUMN entity_type text,
    ADD COLUMN entity_id   uuid,
    ADD COLUMN locale      text REFERENCES locales (code) ON UPDATE CASCADE,
    ADD COLUMN note        text,
    ADD COLUMN last_hit_at timestamptz,
    ADD COLUMN created_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    -- an external target only for manual rows
    ADD CONSTRAINT redirects_external_manual CHECK (to_path ~ '^/' OR (source = 'manual' AND to_path ~ '^https://'));
-- Rows from the shop categories delta (0031) were automatic.
UPDATE redirects SET source = 'auto', entity_type = 'shop_category'
WHERE from_path ~ '^(/[a-z]{2})?/(boutique|shop)/[^/]+$';
-- (from_path <> to_path is already checked since 0009.)
CREATE INDEX redirects_to_path_idx ON redirects (to_path);
CREATE INDEX redirects_entity_idx ON redirects (entity_type, entity_id) WHERE entity_id IS NOT NULL;

-- No loop, ever: an active redirect may not lead back to its own source, directly or through
-- other active redirects. Chains are flattened when written, so the walk is short.
CREATE FUNCTION redirects_no_loop() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    hop  text := NEW.to_path;
    seen text[] := ARRAY[NEW.from_path];
    n    integer := 0;
BEGIN
    IF NOT NEW.is_active THEN
        RETURN NEW;
    END IF;
    LOOP
        IF hop = ANY(seen) THEN
            RAISE EXCEPTION 'redirect_loop' USING ERRCODE = 'check_violation';
        END IF;
        seen := seen || hop;
        SELECT to_path INTO hop FROM redirects WHERE from_path = hop AND is_active AND id <> NEW.id;
        EXIT WHEN NOT FOUND OR hop IS NULL;
        n := n + 1;
        EXIT WHEN n > 20;
    END LOOP;
    RETURN NEW;
END $$;
CREATE TRIGGER redirects_no_loop BEFORE INSERT OR UPDATE OF from_path, to_path, is_active ON redirects
    FOR EACH ROW EXECUTE FUNCTION redirects_no_loop();

-- ---- 404 log ----------------------------------------------------------------------------------------
-- Counted in Redis by the API and flushed every minute; at most 5,000 rows, rows unseen for 90
-- days purged daily (jobs::seo).
CREATE TABLE not_found_log (
    path          text PRIMARY KEY CHECK (path ~ '^/'),
    hits          bigint NOT NULL DEFAULT 0 CHECK (hits >= 0),
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    last_referrer text,
    is_ignored    boolean NOT NULL DEFAULT false
);
CREATE INDEX not_found_log_hits_idx ON not_found_log (hits DESC, last_seen_at DESC);

-- ---- Permission and settings ----------------------------------------------------------------------
INSERT INTO permissions (code, module, description) VALUES
    ('seo.manage', 'content', 'Manage redirects, the 404 log and the sitemap');
INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, 'seo.manage' FROM roles r WHERE r.code IN ('super_admin', 'manager', 'editor')
ON CONFLICT DO NOTHING;

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('seo.robots_extra', 'seo', 'string', '""', false, 'Lines appended to robots.txt in production'),
    ('seo.sitemap_built_at', 'seo', 'json', 'null', false, 'Last sitemap build (set by the API)');
