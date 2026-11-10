-- Media library backed by object storage (GCS).
CREATE TYPE media_status AS ENUM ('pending', 'ready');
CREATE TYPE media_visibility AS ENUM ('public', 'private');

CREATE TABLE media (
    id                UUID PRIMARY KEY,
    visibility        media_visibility NOT NULL,
    status            media_status NOT NULL DEFAULT 'pending',
    object_key        TEXT NOT NULL,
    content_type      TEXT NOT NULL,
    size_bytes        BIGINT NOT NULL CHECK (size_bytes > 0),
    original_filename TEXT NOT NULL,
    uploaded_by       UUID REFERENCES users (id) ON DELETE SET NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at      TIMESTAMPTZ,
    UNIQUE (visibility, object_key)
);

-- Library listing (ready, newest first) and abandoned-upload cleanup.
CREATE INDEX media_ready_idx ON media (created_at DESC, id DESC) WHERE status = 'ready';
CREATE INDEX media_pending_idx ON media (created_at) WHERE status = 'pending';

-- Activity covers now reference the media library instead of free-form URLs.
-- RESTRICT: a media in use cannot be deleted (API answers 409).
ALTER TABLE activities DROP COLUMN cover_image_url;
ALTER TABLE activities ADD COLUMN cover_media_id UUID REFERENCES media (id) ON DELETE RESTRICT;
CREATE INDEX activities_cover_media_idx ON activities (cover_media_id) WHERE cover_media_id IS NOT NULL;
