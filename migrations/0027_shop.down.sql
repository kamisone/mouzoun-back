-- 0027_shop (down). Orders and everything hanging off them are dropped; shop rows in shared
-- tables (payments, refunds, invoices, promotions, reviews, categories, products) are deleted
-- first so the original constraints can come back.

DELETE FROM email_template_translations
WHERE email_template_id IN (SELECT id FROM email_templates WHERE code IN ('order_confirmation',
    'order_payment_pending', 'order_confirmed_cod', 'order_shipped', 'order_ready_for_pickup', 'order_delivered',
    'order_cancelled', 'refund_issued', 'return_update', 'abandoned_cart', 'back_in_stock', 'staff_new_order',
    'staff_low_stock', 'staff_payment_issue'));
DELETE FROM email_templates WHERE code IN ('order_confirmation', 'order_payment_pending', 'order_confirmed_cod',
    'order_shipped', 'order_ready_for_pickup', 'order_delivered', 'order_cancelled', 'refund_issued', 'return_update',
    'abandoned_cart', 'back_in_stock', 'staff_new_order', 'staff_low_stock', 'staff_payment_issue');

DELETE FROM settings WHERE key LIKE 'shop.%' OR key LIKE 'payments.stripe.%' OR key LIKE 'payments.cod.%'
    OR key LIKE 'payments.pay_at_pickup.%' OR key LIKE 'payments.bank_transfer.%';
DELETE FROM permissions WHERE module = 'shop';

-- ---- Navigation ----------------------------------------------------------------------------
DELETE FROM nav_menu_items WHERE link_type = 'shop';
ALTER TABLE nav_menu_items
    DROP CONSTRAINT nav_menu_items_link_type_check,
    DROP CONSTRAINT nav_menu_items_target_count,
    DROP CONSTRAINT nav_menu_items_target_check,
    ADD CONSTRAINT nav_menu_items_link_type_check
        CHECK (link_type IN ('page', 'product', 'category', 'place', 'url', 'route')),
    ADD CONSTRAINT nav_menu_items_check CHECK (num_nonnulls(page_id, product_id, category_id, place_id, url, route) = 1),
    ADD CONSTRAINT nav_menu_items_check1 CHECK (CASE link_type
               WHEN 'page'     THEN page_id IS NOT NULL
               WHEN 'product'  THEN product_id IS NOT NULL
               WHEN 'category' THEN category_id IS NOT NULL
               WHEN 'place'    THEN place_id IS NOT NULL
               WHEN 'url'      THEN url IS NOT NULL
               WHEN 'route'    THEN route IS NOT NULL
           END);

-- ---- Shared tables -------------------------------------------------------------------------
ALTER TABLE reviews DROP COLUMN order_id;

DELETE FROM promotion_redemptions WHERE order_id IS NOT NULL;
ALTER TABLE promotion_redemptions
    DROP CONSTRAINT promotion_redemptions_order_uniq,
    DROP CONSTRAINT promotion_redemptions_owner_check,
    DROP COLUMN order_id,
    ALTER COLUMN booking_id SET NOT NULL;

DROP TABLE promotion_categories;
DELETE FROM promotions WHERE discount_type = 'free_shipping' OR scope = 'shop';
ALTER TABLE promotions DROP CONSTRAINT promotions_discount_type_check;
ALTER TABLE promotions ADD CONSTRAINT promotions_discount_type_check
    CHECK (discount_type IN ('percent', 'fixed_amount', 'free_nights'));
ALTER TABLE promotions
    DROP COLUMN scope,
    DROP COLUMN applies_to_shipping,
    DROP COLUMN min_quantity,
    DROP COLUMN customer_eligibility;

DELETE FROM invoices WHERE order_id IS NOT NULL;
DROP INDEX invoices_order_idx;
ALTER TABLE invoices
    DROP CONSTRAINT invoices_owner_check,
    DROP COLUMN order_id,
    ALTER COLUMN booking_id SET NOT NULL;

DELETE FROM refunds WHERE payment_id IN (SELECT id FROM payments WHERE order_id IS NOT NULL);
ALTER TABLE refunds
    DROP COLUMN return_id,
    DROP COLUMN lines,
    DROP COLUMN restock;

DELETE FROM payments WHERE order_id IS NOT NULL;
ALTER TABLE payments DROP CONSTRAINT payments_provider_check;
ALTER TABLE payments ADD CONSTRAINT payments_provider_check CHECK (provider IN ('cmi', 'stripe', 'paypal', 'cash',
    'bank_transfer', 'card_terminal', 'other'));
DROP INDEX payments_order_idx;
ALTER TABLE payments
    DROP CONSTRAINT payments_provider_session_uniq,
    DROP CONSTRAINT payments_owner_check,
    DROP COLUMN provider_session_id,
    DROP COLUMN order_id,
    ALTER COLUMN booking_id SET NOT NULL;

-- ---- Shop tables (children first) ----------------------------------------------------------
DROP TABLE stock_notifications;
DROP TABLE inventory_reservations;
DROP TABLE inventory_movements;
DROP FUNCTION inventory_movements_append_only();
DROP TABLE return_items;
DROP TABLE order_returns;
DROP TABLE fulfillment_items;
DROP TABLE fulfillments;
DROP TABLE order_events;
DROP TABLE order_items;
ALTER TABLE carts DROP CONSTRAINT carts_converted_order_fkey;
DROP TABLE orders;
DROP SEQUENCE order_return_number_seq;
DROP SEQUENCE order_number_seq;
DROP TABLE cart_items;
DROP TABLE carts;
DROP TABLE shipping_rate_translations;
DROP TABLE shipping_rates;
DROP TABLE shipping_zone_translations;
DROP TABLE shipping_zones;
ALTER TABLE customers
    DROP COLUMN default_shipping_address_id,
    DROP COLUMN default_billing_address_id;
DROP TABLE customer_addresses;
DROP TABLE inventory_lots;
DROP TABLE inventory_levels;
DROP TABLE stock_location_translations;
DROP TABLE stock_locations;
DROP TABLE product_variant_option_values;
DROP TABLE product_variant_media;
DROP TABLE product_variants;
DROP TABLE product_option_value_translations;
DROP TABLE product_option_values;
DROP TABLE product_option_translations;
DROP TABLE product_options;
DROP TABLE shop_product_translations;
DROP TABLE shop_products;
DROP TABLE shipping_profiles;

-- ---- Catalog rows of the shop --------------------------------------------------------------
DELETE FROM content_block_refs WHERE ref_type = 'product' AND ref_id IN (SELECT id FROM products WHERE kind = 'shop_product');
DELETE FROM products WHERE kind = 'shop_product';
ALTER TABLE products DROP CONSTRAINT products_kind_check;
ALTER TABLE products ADD CONSTRAINT products_kind_check
    CHECK (kind IN ('accommodation', 'activity', 'extra', 'package'));

DELETE FROM categories WHERE scope = 'shop';
ALTER TABLE categories DROP CONSTRAINT categories_scope_check;
ALTER TABLE categories ADD CONSTRAINT categories_scope_check
    CHECK (scope IN ('product', 'post', 'faq', 'place'));
