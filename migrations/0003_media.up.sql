-- 0003_media: files live in Google Cloud Storage; the database stores metadata only.

CREATE TABLE media_folders (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    parent_id  uuid REFERENCES media_folders (id) ON DELETE CASCADE,
    name       text NOT NULL CHECK (btrim(name) <> ''),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE NULLS NOT DISTINCT (parent_id, name)
);

CREATE TABLE media_assets (
    id                uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    folder_id         uuid REFERENCES media_folders (id) ON DELETE SET NULL,
    storage_provider  text NOT NULL DEFAULT 'gcs',
    bucket            text NOT NULL,
    object_key        text NOT NULL,
    original_filename text NOT NULL,
    mime_type         text NOT NULL,
    kind              text NOT NULL CHECK (kind IN ('image', 'video', 'audio', 'document', 'other')),
    size_bytes        bigint NOT NULL CHECK (size_bytes >= 0),
    width             integer CHECK (width > 0),
    height            integer CHECK (height > 0),
    duration_seconds  integer CHECK (duration_seconds >= 0),
    checksum_sha256   text CHECK (checksum_sha256 ~ '^[0-9a-f]{64}$'),
    blurhash          text,
    focal_x           numeric(4, 3) CHECK (focal_x BETWEEN 0 AND 1),
    focal_y           numeric(4, 3) CHECK (focal_y BETWEEN 0 AND 1),
    variants          jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(variants) = 'object'),   -- thumb / md / lg
    uploaded_by       uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    deleted_at        timestamptz,
    UNIQUE (bucket, object_key)
);
CREATE INDEX media_assets_folder_idx ON media_assets (folder_id) WHERE deleted_at IS NULL;
CREATE INDEX media_assets_recent_idx ON media_assets (created_at DESC) WHERE deleted_at IS NULL;

CREATE TABLE media_asset_translations (
    media_asset_id uuid NOT NULL REFERENCES media_assets (id) ON DELETE CASCADE,
    locale         text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    alt_text       text,
    title          text,
    caption        text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (media_asset_id, locale)
);

CREATE TABLE galleries (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name       text NOT NULL,             -- internal name, not shown to visitors
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE gallery_items (
    gallery_id uuid NOT NULL REFERENCES galleries (id) ON DELETE CASCADE,
    media_id   uuid NOT NULL REFERENCES media_assets (id) ON DELETE CASCADE,
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (gallery_id, media_id)
);
CREATE INDEX gallery_items_media_idx ON gallery_items (media_id);
