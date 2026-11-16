-- 0023_page_builder: section type registry, richer sections (content_blocks), repeatable items,
-- references held inside jsonb, and draft → published revisions of pages.
-- See docs/page-builder/README.md.

-- ---- Section type registry -------------------------------------------------------------------
-- System types come from section_types/<code>.json in the repository; the API upserts them at
-- startup. The rows inserted here only exist so the content_blocks foreign key can be created
-- before the first startup; their schemas are filled by that sync.
CREATE TABLE section_types (
    code                 text PRIMARY KEY CHECK (code ~ '^[a-z][a-z0-9_]*$'),
    name                 text NOT NULL,
    description          text,
    icon                 text,
    preview_media_id     uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    category             text NOT NULL CHECK (category IN ('hero', 'listing', 'content', 'media', 'social_proof',
                             'conversion', 'layout', 'utility')),
    settings_schema      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(settings_schema) = 'object'),
    content_schema       jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(content_schema) = 'object'),
    item_settings_schema jsonb CHECK (jsonb_typeof(item_settings_schema) = 'object'),
    item_content_schema  jsonb CHECK (jsonb_typeof(item_content_schema) = 'object'),
    min_items            integer NOT NULL DEFAULT 0 CHECK (min_items >= 0),
    max_items            integer CHECK (max_items >= 0),
    allowed_owners       text[] NOT NULL DEFAULT '{page}'
                             CHECK (cardinality(allowed_owners) > 0 AND allowed_owners <@ ARRAY['page', 'product', 'place']),
    allowed_page_types   text[] CHECK (allowed_page_types <@ ARRAY['home', 'page', 'post', 'legal', 'landing']),
    max_per_page         integer CHECK (max_per_page > 0),
    preset               jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(preset) = 'object'),
    required_permission  text REFERENCES permissions (code) ON UPDATE CASCADE ON DELETE RESTRICT,
    schema_version       integer NOT NULL DEFAULT 1 CHECK (schema_version > 0),
    is_system            boolean NOT NULL DEFAULT true,
    is_active            boolean NOT NULL DEFAULT true,
    sort_order           integer NOT NULL DEFAULT 0,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    CHECK (max_items IS NULL OR max_items >= min_items),
    -- a type without items cannot require any
    CHECK (item_settings_schema IS NOT NULL OR item_content_schema IS NOT NULL OR min_items = 0)
);
CREATE TRIGGER section_types_set_updated_at BEFORE UPDATE ON section_types
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---- Permissions and settings ------------------------------------------------------------------
INSERT INTO permissions (code, module, description) VALUES
    ('content.publish', 'content', 'Publish, schedule and restore page revisions'),
    ('content.html',    'content', 'Use the raw HTML section and edit section type definitions');

INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, p.code
FROM roles r
JOIN permissions p ON p.code IN ('content.publish', 'content.html')
WHERE r.code = 'super_admin' OR (r.code = 'manager' AND p.code = 'content.publish')
ON CONFLICT DO NOTHING;

-- Empty until the camp has a date; the seed and the admin set it (ISO date, YYYY-MM-DD).
INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('site.opening_date', 'general', 'string', '""', true, 'Opening date of the camp (YYYY-MM-DD), shown while coming soon')
ON CONFLICT (key) DO NOTHING;

INSERT INTO section_types (code, name, category, max_per_page, required_permission, sort_order) VALUES
    ('hero',               'Hero',                'hero',         1,    NULL,           10),
    ('hero_slider',        'Hero slider',         'hero',         1,    NULL,           20),
    ('booking_search',     'Booking search',      'conversion',   1,    NULL,           30),
    ('product_carousel',   'Products',            'listing',      NULL, NULL,           40),
    ('category_tiles',     'Categories',          'listing',      NULL, NULL,           50),
    ('image_text',         'Image and text',      'content',      NULL, NULL,           60),
    ('features',           'Features',            'content',      NULL, NULL,           70),
    ('stats',              'Key figures',         'content',      NULL, NULL,           80),
    ('timeline',           'Timeline',            'content',      NULL, NULL,           90),
    ('seasonal_calendar',  'Seasonal calendar',   'content',      NULL, NULL,          100),
    ('gallery',            'Gallery',             'media',        NULL, NULL,          110),
    ('video',              'Video',               'media',        NULL, NULL,          120),
    ('offers',             'Offers',              'conversion',   NULL, NULL,          130),
    ('testimonials',       'Testimonials',        'social_proof', NULL, NULL,          140),
    ('logos',              'Logos',               'social_proof', NULL, NULL,          150),
    ('map',                'Map',                 'utility',      NULL, NULL,          160),
    ('faq',                'FAQ',                 'utility',      NULL, NULL,          170),
    ('cta_band',           'Call to action',      'conversion',   NULL, NULL,          180),
    ('newsletter',         'Newsletter',          'conversion',   NULL, NULL,          190),
    ('contact',            'Contact',             'conversion',   NULL, NULL,          200),
    ('posts_feed',         'Posts',               'listing',      NULL, NULL,          210),
    ('rich_text',          'Rich text',           'content',      NULL, NULL,          220),
    ('html',               'HTML',                'utility',      NULL, 'content.html', 230),
    ('spacer',             'Spacer',              'layout',       NULL, NULL,          240);

-- ---- Sections (content_blocks) -----------------------------------------------------------------
-- Old block codes → the v1 library. Settings are rewritten to the new shape where it is known.
UPDATE content_blocks
SET block_type = 'product_carousel',
    settings = jsonb_build_object(
        'source', jsonb_build_object(
            'mode', 'rule',
            'ids', '[]'::jsonb,
            'rule', jsonb_strip_nulls(jsonb_build_object(
                'kind', COALESCE(settings->>'kind', CASE WHEN block_type = 'activity_list' THEN 'activity' END),
                'featured_only', COALESCE((settings->>'featured')::boolean, false),
                'sort', 'sort_order',
                'limit', LEAST(GREATEST(COALESCE((settings->>'limit')::integer, 6), 1), 24)))),
        'card_style', 'photo', 'layout', 'carousel',
        'show_price_from', true, 'show_rating', true, 'show_duration', true)
WHERE block_type IN ('product_grid', 'activity_list');
UPDATE content_blocks SET block_type = 'cta_band'     WHERE block_type = 'cta';
UPDATE content_blocks SET block_type = 'faq',
    settings = jsonb_build_object('source', jsonb_build_object('mode', 'rule', 'ids', '[]'::jsonb,
                   'rule', jsonb_build_object('limit', LEAST(GREATEST(COALESCE((settings->>'limit')::integer, 10), 1), 24))),
                   'first_open', false, 'emit_schema', true)
WHERE block_type = 'faq_list';
UPDATE content_blocks SET block_type = 'testimonials' WHERE block_type = 'reviews';
UPDATE content_blocks SET block_type = 'contact'      WHERE block_type = 'contact_form';
UPDATE content_blocks SET block_type = 'image_text'   WHERE block_type = 'image';
-- rich_text stored its body as content.html: the v1 schema calls it body.
UPDATE content_block_translations t
SET content = (t.content - 'html') || jsonb_build_object('body', t.content->'html')
FROM content_blocks b
WHERE b.id = t.content_block_id AND b.block_type = 'rich_text' AND t.content ? 'html';

-- Any other code found in existing data is kept, as an inactive custom type (rendered by nobody).
INSERT INTO section_types (code, name, category, is_system, is_active)
SELECT DISTINCT block_type, block_type, 'utility', false, false FROM content_blocks
ON CONFLICT (code) DO NOTHING;

ALTER TABLE content_blocks
    ADD COLUMN name            text,
    ADD COLUMN anchor_id       text CHECK (anchor_id ~ '^[a-z][a-z0-9-]*$'),
    ADD COLUMN active_period   tstzrange CHECK (NOT isempty(active_period)),
    ADD COLUMN visible_on      text NOT NULL DEFAULT 'all' CHECK (visible_on IN ('all', 'desktop', 'mobile')),
    ADD COLUMN hidden_locales  text[] NOT NULL DEFAULT '{}',
    ADD COLUMN appearance      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(appearance) = 'object'),
    ADD COLUMN analytics_label text,
    ADD COLUMN shared_block_id uuid REFERENCES content_blocks (id) ON DELETE RESTRICT,
    ADD COLUMN is_shared       boolean NOT NULL DEFAULT false,
    ADD COLUMN created_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    ADD COLUMN updated_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    ADD COLUMN deleted_at      timestamptz,
    ADD CONSTRAINT content_blocks_block_type_fkey FOREIGN KEY (block_type)
        REFERENCES section_types (code) ON UPDATE CASCADE ON DELETE RESTRICT,
    DROP CONSTRAINT content_blocks_check,
    ADD CONSTRAINT content_blocks_owner_check CHECK (
        (is_shared AND num_nonnulls(page_id, product_id, place_id) = 0)
        OR (NOT is_shared AND num_nonnulls(page_id, product_id, place_id) = 1)),
    ADD CONSTRAINT content_blocks_shared_check CHECK (shared_block_id IS NULL OR NOT is_shared),
    ADD CONSTRAINT content_blocks_not_self_check CHECK (shared_block_id <> id);

-- Soft-deleted sections free their anchor.
CREATE UNIQUE INDEX content_blocks_anchor_uniq ON content_blocks (page_id, anchor_id)
    WHERE page_id IS NOT NULL AND anchor_id IS NOT NULL AND deleted_at IS NULL;
CREATE INDEX content_blocks_active_period_idx ON content_blocks USING gist (active_period)
    WHERE active_period IS NOT NULL;
CREATE INDEX content_blocks_shared_block_idx ON content_blocks (shared_block_id)
    WHERE shared_block_id IS NOT NULL;
CREATE INDEX content_blocks_library_idx ON content_blocks (updated_at DESC)
    WHERE is_shared AND deleted_at IS NULL;

-- ---- Items -------------------------------------------------------------------------------------
CREATE TABLE content_block_items (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    block_id      uuid NOT NULL REFERENCES content_blocks (id) ON DELETE CASCADE,
    settings      jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(settings) = 'object'),
    sort_order    integer NOT NULL DEFAULT 0,
    is_visible    boolean NOT NULL DEFAULT true,
    active_period tstzrange CHECK (NOT isempty(active_period)),
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX content_block_items_block_idx ON content_block_items (block_id, sort_order);
CREATE TRIGGER content_block_items_set_updated_at BEFORE UPDATE ON content_block_items
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE content_block_item_translations (
    item_id    uuid NOT NULL REFERENCES content_block_items (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    content    jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(content) = 'object'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (item_id, locale)
);
CREATE TRIGGER content_block_item_translations_set_updated_at BEFORE UPDATE ON content_block_item_translations
    FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ---- References held inside jsonb (rebuilt by the API on every save) ----------------------------
CREATE TABLE content_block_refs (
    block_id uuid NOT NULL REFERENCES content_blocks (id) ON DELETE CASCADE,
    item_id  uuid REFERENCES content_block_items (id) ON DELETE CASCADE,
    ref_type text NOT NULL CHECK (ref_type IN ('media', 'product', 'category', 'place', 'page', 'faq', 'gallery',
                 'promotion', 'review')),
    ref_id   uuid NOT NULL
);
CREATE UNIQUE INDEX content_block_refs_uniq ON content_block_refs
    (block_id, COALESCE(item_id, '00000000-0000-0000-0000-000000000000'::uuid), ref_type, ref_id);
CREATE INDEX content_block_refs_target_idx ON content_block_refs (ref_type, ref_id);

-- ---- Draft → published revisions ----------------------------------------------------------------
ALTER TABLE pages
    ADD COLUMN draft_version           bigint NOT NULL DEFAULT 1 CHECK (draft_version > 0),
    ADD COLUMN has_unpublished_changes boolean NOT NULL DEFAULT false,
    -- last revision_number handed out: incremented under the page row lock, so numbers are gap-free
    ADD COLUMN revision_counter        integer NOT NULL DEFAULT 0 CHECK (revision_counter >= 0);

CREATE TABLE page_revisions (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    page_id         uuid NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    revision_number integer NOT NULL CHECK (revision_number > 0),
    status          text NOT NULL CHECK (status IN ('scheduled', 'published', 'superseded', 'cancelled')),
    -- full resolved tree at publish time; `refs` lists every referenced id (media, products...)
    snapshot        jsonb NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    publish_at      timestamptz,
    published_at    timestamptz,
    published_by    uuid REFERENCES users (id) ON DELETE SET NULL,
    note            text,
    created_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (page_id, revision_number),
    CHECK (status <> 'scheduled' OR publish_at IS NOT NULL),
    CHECK (status NOT IN ('published', 'superseded') OR published_at IS NOT NULL)
);
CREATE UNIQUE INDEX page_revisions_one_published ON page_revisions (page_id) WHERE status = 'published';
CREATE INDEX page_revisions_due_idx ON page_revisions (publish_at) WHERE status = 'scheduled';
CREATE INDEX page_revisions_page_idx ON page_revisions (page_id, revision_number DESC);
-- "is this media/product used by a live page?"
CREATE INDEX page_revisions_refs_idx ON page_revisions USING gin ((snapshot -> 'refs') jsonb_path_ops)
    WHERE status IN ('published', 'scheduled');
