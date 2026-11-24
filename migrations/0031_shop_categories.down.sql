DROP FUNCTION shop_visible_categories();
DROP TRIGGER categories_tree_check ON categories;
DROP FUNCTION categories_tree_check();
DELETE FROM settings WHERE key = 'shop.category_max_depth';
DROP INDEX product_categories_order_idx;
ALTER TABLE product_categories DROP COLUMN is_pinned, DROP COLUMN sort_order;
DROP TRIGGER categories_first_activation ON categories;
DROP FUNCTION categories_first_activation();
ALTER TABLE categories DROP COLUMN first_activated_at, DROP COLUMN show_in_menu, DROP COLUMN default_sort;
