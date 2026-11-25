-- 0033_shop_products_section: farm-shop products leave the generic "Produits" section
-- (`product_carousel` v2: bookable kinds only) for their own `shop_products` section.
--
-- Draft sections only (published snapshots are never edited: republish those pages through the
-- publish service, `cargo run --bin republish_pages -- <ids>`):
--   - a `product_carousel` whose rule shows shop products, or whose hand-picked products are all
--     shop products, becomes a `shop_products` section (rule, layout and heading kept; translations,
--     appearance, anchor and schedule untouched; button labels dropped: the new section has none);
--   - one mixing shop and bookable products keeps the bookable ones only.
-- Pages touched get `has_unpublished_changes`. Every change is kept in
-- `migration_0033_blocks` so the down migration restores the sections as they were.

-- The section type itself (its schemas are written by the API's library sync at start-up).
INSERT INTO section_types (code, name, description, icon, category, sort_order)
VALUES ('shop_products', 'Shop products', 'Farm-shop products with the shop''s own card.', 'i-lucide-shopping-bag', 'listing', 41)
ON CONFLICT (code) DO UPDATE SET is_active = true;

CREATE TABLE migration_0033_blocks (
    block_id     uuid PRIMARY KEY,
    page_id      uuid,
    action       text NOT NULL CHECK (action IN ('converted', 'trimmed')),
    old_settings jsonb NOT NULL,
    old_content  jsonb NOT NULL DEFAULT '{}'   -- {locale: content}
);

WITH shop_ids AS (
    SELECT b.id,
           COALESCE((SELECT array_agg(x) FROM jsonb_array_elements_text(COALESCE(b.settings #> '{source,ids}', '[]')) x
                     JOIN products p ON p.id = x::uuid AND p.kind = 'shop_product'), '{}') AS shop,
           (SELECT count(*) FROM jsonb_array_elements_text(COALESCE(b.settings #> '{source,ids}', '[]'))) AS picked
    FROM content_blocks b
    WHERE b.block_type = 'product_carousel' AND b.deleted_at IS NULL
)
INSERT INTO migration_0033_blocks (block_id, page_id, action, old_settings, old_content)
SELECT b.id, b.page_id,
       CASE WHEN b.settings #>> '{source,mode}' = 'rule' OR cardinality(s.shop) = s.picked THEN 'converted' ELSE 'trimmed' END,
       b.settings,
       COALESCE((SELECT jsonb_object_agg(t.locale, t.content) FROM content_block_translations t WHERE t.content_block_id = b.id), '{}')
FROM content_blocks b JOIN shop_ids s ON s.id = b.id
WHERE (b.settings #>> '{source,mode}' = 'rule' AND b.settings #>> '{source,rule,kind}' = 'shop_product')
   OR (b.settings #>> '{source,mode}' = 'manual' AND cardinality(s.shop) > 0);

-- Shop-only sections → shop_products.
UPDATE content_blocks b SET
    block_type = 'shop_products',
    settings = jsonb_strip_nulls(jsonb_build_object(
        'heading_align', b.settings -> 'heading_align',
        'title_level', b.settings -> 'title_level',
        'source', jsonb_build_object(
            'mode', b.settings #>> '{source,mode}',
            'ids', COALESCE(b.settings #> '{source,ids}', '[]'),
            'rule', jsonb_build_object(
                'category_id', b.settings #> '{source,rule,category_id}',
                'include_subcategories', true,
                'featured_only', COALESCE(b.settings #> '{source,rule,featured_only}', 'false'),
                'in_stock_only', true,
                'on_sale_only', false,
                'sort', CASE b.settings #>> '{source,rule,sort}' WHEN 'newest' THEN 'newest' WHEN 'price_asc' THEN 'price_asc' ELSE 'featured' END,
                'limit', COALESCE(b.settings #> '{source,rule,limit}', '8'))),
        'layout', COALESCE(b.settings -> 'layout', '"carousel"'),
        'columns_mobile', 2, 'image_ratio', 'square', 'show_secondary_image', true,
        'show_unit_price', true, 'show_rating', COALESCE(b.settings -> 'show_rating', 'false'),
        'quick_add', true, 'show_view_all', true)),
    updated_at = now()
FROM migration_0033_blocks m
WHERE m.block_id = b.id AND m.action = 'converted';
UPDATE content_block_translations t SET content = t.content - 'cta_primary_label' - 'cta_secondary_label'
FROM migration_0033_blocks m WHERE m.block_id = t.content_block_id AND m.action = 'converted';

-- Mixed hand-picked sections keep their bookable products.
UPDATE content_blocks b SET
    settings = jsonb_set(b.settings, '{source,ids}', COALESCE((
        SELECT jsonb_agg(x) FROM jsonb_array_elements_text(b.settings #> '{source,ids}') x
        WHERE NOT EXISTS (SELECT 1 FROM products p WHERE p.id = x::uuid AND p.kind = 'shop_product')), '[]')),
    updated_at = now()
FROM migration_0033_blocks m
WHERE m.block_id = b.id AND m.action = 'trimmed';

UPDATE pages SET has_unpublished_changes = true
WHERE id IN (SELECT page_id FROM migration_0033_blocks WHERE page_id IS NOT NULL);
