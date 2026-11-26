DELETE FROM role_permissions
WHERE permission_code = 'products.read' AND role_id = (SELECT id FROM roles WHERE code = 'guide');
ALTER TABLE accommodation_types DROP COLUMN pool_capacity;
