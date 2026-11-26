-- 0036_catalog_editors: the admin editors of stays, activities, packages and extras
-- (back/docs/catalog/README.md).
--
-- Pool-mode accommodation types get their own capacity: until now a pool of 20 pitches was 20
-- `accommodation_units` never allocated individually. NULL keeps that behaviour (capacity =
-- active units), so existing data is unchanged; the editor writes the number.

ALTER TABLE accommodation_types
    ADD COLUMN pool_capacity integer CHECK (pool_capacity >= 0);

-- Guides read the departures and the calendar in the catalogue editors (read-only): they need to
-- open them. No new permission code.
INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, 'products.read' FROM roles r WHERE r.code = 'guide'
ON CONFLICT DO NOTHING;
