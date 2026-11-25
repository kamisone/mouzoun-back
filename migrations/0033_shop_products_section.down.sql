-- Restores the sections changed by 0033 (sections added as `shop_products` later are removed:
-- `product_carousel` v1 can show shop products, nothing else can render them before 0033).
UPDATE content_blocks b SET block_type = 'product_carousel', settings = m.old_settings, updated_at = now()
FROM migration_0033_blocks m WHERE m.block_id = b.id;
UPDATE content_block_translations t SET content = m.old_content -> t.locale
FROM migration_0033_blocks m WHERE m.block_id = t.content_block_id AND m.old_content ? t.locale;
UPDATE content_blocks SET deleted_at = now() WHERE block_type = 'shop_products' AND deleted_at IS NULL;
DROP TABLE migration_0033_blocks;
-- Soft-deleted sections still reference the type: it stays, inactive.
UPDATE section_types SET is_active = false WHERE code = 'shop_products';
