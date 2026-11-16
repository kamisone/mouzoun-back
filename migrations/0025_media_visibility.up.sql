-- Public and private files share one bucket with fine-grained access control: each object gets
-- its ACL when it is uploaded (`public-read` or `private`). The row records which one, so the
-- API serves a public URL or a short-lived signed URL, and public pages skip private files.
-- Existing rows were all uploaded to the public bucket.
ALTER TABLE media_assets
    ADD COLUMN visibility text NOT NULL DEFAULT 'public' CHECK (visibility IN ('public', 'private'));
