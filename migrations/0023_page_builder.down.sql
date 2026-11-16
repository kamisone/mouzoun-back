-- 0023_page_builder (down). Shared sections (owned by no page) and their placements cannot
-- exist under the old owner CHECK: they are deleted. Items and revisions are dropped with their
-- tables. Block codes are mapped back to the pre-0023 names.

DROP TABLE page_revisions;
ALTER TABLE pages
    DROP COLUMN draft_version,
    DROP COLUMN has_unpublished_changes,
    DROP COLUMN revision_counter;

DROP TABLE content_block_refs;
DROP TABLE content_block_item_translations;
DROP TABLE content_block_items;

DELETE FROM content_blocks WHERE shared_block_id IS NOT NULL;
DELETE FROM content_blocks WHERE is_shared;
-- Soft-deleted sections did not exist before: remove them for good.
DELETE FROM content_blocks WHERE deleted_at IS NOT NULL;

DROP INDEX content_blocks_anchor_uniq;
DROP INDEX content_blocks_active_period_idx;
DROP INDEX content_blocks_shared_block_idx;
DROP INDEX content_blocks_library_idx;

ALTER TABLE content_blocks
    DROP CONSTRAINT content_blocks_block_type_fkey,
    DROP CONSTRAINT content_blocks_owner_check,
    DROP CONSTRAINT content_blocks_shared_check,
    DROP CONSTRAINT content_blocks_not_self_check,
    DROP COLUMN name,
    DROP COLUMN anchor_id,
    DROP COLUMN active_period,
    DROP COLUMN visible_on,
    DROP COLUMN hidden_locales,
    DROP COLUMN appearance,
    DROP COLUMN analytics_label,
    DROP COLUMN shared_block_id,
    DROP COLUMN is_shared,
    DROP COLUMN created_by,
    DROP COLUMN updated_by,
    DROP COLUMN deleted_at,
    ADD CONSTRAINT content_blocks_check CHECK (num_nonnulls(page_id, product_id, place_id) = 1);

UPDATE content_block_translations t
SET content = (t.content - 'body') || jsonb_build_object('html', t.content->'body')
FROM content_blocks b
WHERE b.id = t.content_block_id AND b.block_type = 'rich_text' AND t.content ? 'body';
UPDATE content_blocks
SET block_type = CASE WHEN settings->'source'->'rule'->>'kind' = 'activity' THEN 'activity_list' ELSE 'product_grid' END,
    settings = jsonb_strip_nulls(jsonb_build_object(
        'kind', settings->'source'->'rule'->'kind',
        'featured', settings->'source'->'rule'->'featured_only',
        'limit', settings->'source'->'rule'->'limit'))
WHERE block_type = 'product_carousel';
UPDATE content_blocks SET block_type = 'faq_list',
    settings = jsonb_strip_nulls(jsonb_build_object('limit', settings->'source'->'rule'->'limit'))
WHERE block_type = 'faq';
UPDATE content_blocks SET block_type = 'cta'          WHERE block_type = 'cta_band';
UPDATE content_blocks SET block_type = 'reviews'      WHERE block_type = 'testimonials';
UPDATE content_blocks SET block_type = 'contact_form' WHERE block_type = 'contact';
UPDATE content_blocks SET block_type = 'image'        WHERE block_type = 'image_text';

DROP TABLE section_types;

DELETE FROM settings WHERE key = 'site.opening_date';
DELETE FROM role_permissions WHERE permission_code IN ('content.publish', 'content.html');
DELETE FROM permissions WHERE code IN ('content.publish', 'content.html');
