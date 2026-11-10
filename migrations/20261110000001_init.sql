CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TYPE user_role AS ENUM ('admin', 'staff', 'customer');

CREATE TABLE users (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email         TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    full_name     TEXT NOT NULL,
    role          user_role NOT NULL DEFAULT 'customer',
    is_active     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE activities (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug             TEXT NOT NULL UNIQUE,
    title_fr         TEXT NOT NULL,
    title_en         TEXT NOT NULL,
    description_fr   TEXT NOT NULL DEFAULT '',
    description_en   TEXT NOT NULL DEFAULT '',
    price_mad        INTEGER NOT NULL CHECK (price_mad >= 0),
    duration_minutes INTEGER NOT NULL CHECK (duration_minutes >= 0),
    cover_image_url  TEXT,
    is_published     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX activities_published_idx ON activities (is_published, created_at DESC);
