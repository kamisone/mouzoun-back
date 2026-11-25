-- 0035_legal_entity_admin: the company facts are edited in Admin › Settings › Legal entity (no
-- environment variables), and the legal pages no longer carry a separate review status: the
-- admin writes them in the builder and publishes them like any page.
ALTER TABLE pages
    DROP COLUMN reviewed_by,
    DROP COLUMN reviewed_at,
    DROP COLUMN legal_review_status;
