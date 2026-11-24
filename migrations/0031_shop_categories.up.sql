-- 0031_shop_categories: shop categories get their own admin (tree, full form, safe delete).
--
-- Reuses categories + category_translations (scope 'shop'). Adds the default product sort, the
-- "show in menu" flag, the first activation date (the slug is locked once a category went live),
-- manual product order and pins inside a category, and the tree rules (no cycle, same scope as
-- the parent, at most `shop.category_max_depth` levels) enforced at commit time, so a whole tree
-- move saved in one transaction is checked on its final state. See docs/shop/README.md.
--
-- Cover alt text comes from the media asset translations (media_asset_translations.alt_text):
-- no cover_alt column. No deleted_at: categories are deleted for real (through the API flow).

ALTER TABLE categories
    ADD COLUMN default_sort text NOT NULL DEFAULT 'manual'
        CHECK (default_sort IN ('manual', 'best_selling', 'newest', 'price_asc', 'price_desc', 'name')),
    ADD COLUMN show_in_menu boolean NOT NULL DEFAULT true,
    ADD COLUMN first_activated_at timestamptz;

UPDATE categories SET first_activated_at = created_at WHERE is_active;

-- Set once, the first time a category is active (insert or update).
CREATE FUNCTION categories_first_activation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.is_active AND NEW.first_activated_at IS NULL THEN
        NEW.first_activated_at := now();
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER categories_first_activation BEFORE INSERT OR UPDATE OF is_active ON categories
    FOR EACH ROW EXECUTE FUNCTION categories_first_activation();

ALTER TABLE product_categories
    ADD COLUMN sort_order integer NOT NULL DEFAULT 0,
    ADD COLUMN is_pinned boolean NOT NULL DEFAULT false;
CREATE INDEX product_categories_order_idx ON product_categories (category_id, is_pinned DESC, sort_order);

-- Existing links keep the product order they had (product sort order, then name).
UPDATE product_categories pc SET sort_order = o.n
FROM (SELECT pc2.product_id, pc2.category_id,
             (row_number() OVER (PARTITION BY pc2.category_id ORDER BY p.sort_order, p.created_at, p.id) - 1)::int AS n
      FROM product_categories pc2 JOIN products p ON p.id = pc2.product_id) o
WHERE o.product_id = pc.product_id AND o.category_id = pc.category_id;

-- Every shop product with categories has exactly one primary (its first, by category order).
UPDATE product_categories pc SET is_primary = true
FROM (SELECT DISTINCT ON (pc2.product_id) pc2.product_id, pc2.category_id
      FROM product_categories pc2 JOIN categories c ON c.id = pc2.category_id AND c.scope = 'shop'
      WHERE NOT EXISTS (SELECT 1 FROM product_categories x WHERE x.product_id = pc2.product_id AND x.is_primary)
      ORDER BY pc2.product_id, c.sort_order, c.id) f
WHERE f.product_id = pc.product_id AND f.category_id = pc.category_id;

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('shop.category_max_depth', 'shop', 'number', '3', false, 'Maximum depth of the shop category tree (1 = no subcategories)');

-- ---- Tree rules ------------------------------------------------------------------------------
-- A constraint trigger, deferred to commit: `PUT /admin/shop/categories/tree` moves many rows in
-- one transaction, and only the final tree has to be valid. Errors carry a stable message the
-- API maps to its error codes: category_scope, category_cycle, category_too_deep.
CREATE FUNCTION categories_tree_check() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    parent_scope text;
    max_depth    integer;
    depth        integer;
    height       integer;
    looped       boolean;
BEGIN
    -- The row may have been deleted later in the same transaction.
    IF NOT EXISTS (SELECT 1 FROM categories WHERE id = NEW.id) THEN
        RETURN NULL;
    END IF;
    IF NEW.parent_id IS NOT NULL THEN
        SELECT scope INTO parent_scope FROM categories WHERE id = NEW.parent_id;
        IF parent_scope IS DISTINCT FROM NEW.scope THEN
            RAISE EXCEPTION 'category_scope' USING ERRCODE = 'check_violation';
        END IF;
    END IF;
    -- Walk up from the row: meeting itself again is a cycle (the path stops the walk).
    WITH RECURSIVE up AS (
        SELECT c.id, c.parent_id, 1 AS n, ARRAY[c.id] AS path, false AS looped
        FROM categories c WHERE c.id = NEW.id
        UNION ALL
        SELECT p.id, p.parent_id, up.n + 1, up.path || p.id, p.id = ANY(up.path)
        FROM up JOIN categories p ON p.id = up.parent_id
        WHERE NOT up.looped
    )
    SELECT max(n), bool_or(up.looped) INTO depth, looped FROM up;
    IF looped THEN
        RAISE EXCEPTION 'category_cycle' USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.scope <> 'shop' THEN
        RETURN NULL;
    END IF;
    SELECT COALESCE((SELECT (value #>> '{}')::integer FROM settings WHERE key = 'shop.category_max_depth'), 3)
    INTO max_depth;
    -- Levels below the row (its own subtree moves with it).
    WITH RECURSIVE down AS (
        SELECT c.id, 0 AS n FROM categories c WHERE c.id = NEW.id
        UNION ALL
        SELECT c.id, down.n + 1 FROM categories c JOIN down ON c.parent_id = down.id WHERE down.n < 50
    )
    SELECT max(n) INTO height FROM down;
    IF depth + height > max_depth THEN
        RAISE EXCEPTION 'category_too_deep' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NULL;
END $$;

CREATE CONSTRAINT TRIGGER categories_tree_check
    AFTER INSERT OR UPDATE OF parent_id, scope ON categories
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION categories_tree_check();

-- ---- Visible shop categories ---------------------------------------------------------------------
-- Active categories whose ancestors are all active (an inactive parent hides its subtree), with
-- their depth, path from the root and whether they show in menus (themselves and ancestors).
CREATE FUNCTION shop_visible_categories()
RETURNS TABLE (id uuid, parent_id uuid, in_menu boolean, depth integer, path uuid[])
LANGUAGE sql STABLE AS $$
    WITH RECURSIVE vis AS (
        SELECT c.id, c.parent_id, c.show_in_menu AS in_menu, 1 AS depth, ARRAY[c.id] AS path
        FROM categories c WHERE c.scope = 'shop' AND c.parent_id IS NULL AND c.is_active
        UNION ALL
        SELECT c.id, c.parent_id, vis.in_menu AND c.show_in_menu, vis.depth + 1, vis.path || c.id
        FROM categories c JOIN vis ON c.parent_id = vis.id
        WHERE c.is_active AND vis.depth < 20
    )
    SELECT id, parent_id, in_menu, depth, path FROM vis
$$;
