-- 0004_taxonomy_places: categories, tags, amenities, places, farm crops.

CREATE TABLE categories (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    scope          text NOT NULL CHECK (scope IN ('product', 'post', 'faq', 'place')),
    parent_id      uuid REFERENCES categories (id) ON DELETE CASCADE,
    icon           text,
    cover_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    sort_order     integer NOT NULL DEFAULT 0,
    is_active      boolean NOT NULL DEFAULT true,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    CHECK (parent_id <> id)
);
CREATE INDEX categories_scope_idx ON categories (scope, sort_order);

CREATE TABLE category_translations (
    category_id     uuid NOT NULL REFERENCES categories (id) ON DELETE CASCADE,
    locale          text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name            text NOT NULL,
    slug            text NOT NULL CHECK (slug ~ '^[^\s/?#]+$'),
    description     text,
    seo_title       text,
    seo_description text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (category_id, locale),
    UNIQUE (locale, slug)
);

CREATE TABLE tags (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       citext NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE tag_translations (
    tag_id     uuid NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name       text NOT NULL,
    slug       text NOT NULL CHECK (slug ~ '^[^\s/?#]+$'),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tag_id, locale),
    UNIQUE (locale, slug)
);

CREATE TABLE amenities (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code          citext NOT NULL UNIQUE,
    amenity_group text NOT NULL CHECK (amenity_group IN ('sanitary', 'water_power', 'food_drink', 'comfort',
                      'connectivity', 'family', 'safety', 'accessibility', 'parking_transport', 'other')),
    icon          text,
    is_site_wide  boolean NOT NULL DEFAULT false,
    sort_order    integer NOT NULL DEFAULT 0,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE amenity_translations (
    amenity_id  uuid NOT NULL REFERENCES amenities (id) ON DELETE CASCADE,
    locale      text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name        text NOT NULL,
    description text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (amenity_id, locale)
);

CREATE TABLE places (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind           text NOT NULL CHECK (kind IN ('camp_zone', 'facility', 'farm_field', 'meeting_point', 'village',
                       'town', 'natural_site', 'heritage_site', 'other')),
    parent_id      uuid REFERENCES places (id) ON DELETE SET NULL,
    category_id    uuid REFERENCES categories (id) ON DELETE SET NULL,
    latitude       numeric(9, 6) CHECK (latitude BETWEEN -90 AND 90),
    longitude      numeric(9, 6) CHECK (longitude BETWEEN -180 AND 180),
    distance_km    numeric(8, 2) CHECK (distance_km >= 0),
    cover_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    gallery_id     uuid REFERENCES galleries (id) ON DELETE SET NULL,
    is_published   boolean NOT NULL DEFAULT false,
    sort_order     integer NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    deleted_at     timestamptz,
    CHECK (parent_id <> id)
);
CREATE INDEX places_kind_idx ON places (kind) WHERE deleted_at IS NULL;

CREATE TABLE place_translations (
    place_id          uuid NOT NULL REFERENCES places (id) ON DELETE CASCADE,
    locale            text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name              text NOT NULL,
    slug              text NOT NULL CHECK (slug ~ '^[^\s/?#]+$'),
    short_description text,
    description       text,
    directions        text,
    seo_title         text,
    seo_description   text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (place_id, locale),
    UNIQUE (locale, slug)
);

CREATE TABLE place_amenities (
    place_id   uuid NOT NULL REFERENCES places (id) ON DELETE CASCADE,
    amenity_id uuid NOT NULL REFERENCES amenities (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (place_id, amenity_id)
);

CREATE TABLE farm_crops (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code           citext NOT NULL UNIQUE,
    harvest_months smallint[] NOT NULL DEFAULT '{}'
                   CHECK (harvest_months <@ ARRAY[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]::smallint[]),
    icon           text,
    cover_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    sort_order     integer NOT NULL DEFAULT 0,
    is_published   boolean NOT NULL DEFAULT false,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE farm_crop_translations (
    farm_crop_id uuid NOT NULL REFERENCES farm_crops (id) ON DELETE CASCADE,
    locale       text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name         text NOT NULL,
    description  text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (farm_crop_id, locale)
);

CREATE TABLE place_crops (
    place_id   uuid NOT NULL REFERENCES places (id) ON DELETE CASCADE,
    crop_id    uuid NOT NULL REFERENCES farm_crops (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (place_id, crop_id)
);
