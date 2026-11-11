-- 0005_catalog: one products table for everything sellable, detail tables per kind
-- (class-table inheritance: the composite FK (product_id, kind) makes a detail row
-- impossible for a product of another kind).

CREATE TABLE products (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind           text NOT NULL CHECK (kind IN ('accommodation', 'activity', 'extra', 'package')),
    sku            citext UNIQUE,
    status         text NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'published', 'archived')),
    booking_mode   text NOT NULL DEFAULT 'instant' CHECK (booking_mode IN ('instant', 'on_request', 'not_bookable')),
    is_featured    boolean NOT NULL DEFAULT false,
    place_id       uuid REFERENCES places (id) ON DELETE SET NULL,
    cover_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    gallery_id     uuid REFERENCES galleries (id) ON DELETE SET NULL,
    video_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    sort_order     integer NOT NULL DEFAULT 0,
    published_at   timestamptz,
    created_by     uuid REFERENCES users (id) ON DELETE SET NULL,
    updated_by     uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    deleted_at     timestamptz,
    UNIQUE (id, kind)
);
CREATE INDEX products_kind_status_idx ON products (kind, status) WHERE deleted_at IS NULL;

CREATE TABLE product_translations (
    product_id        uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    locale            text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name              text NOT NULL,
    slug              text NOT NULL CHECK (slug ~ '^[^\s/?#]+$'),
    subtitle          text,
    short_description text,
    description       text,
    highlights        text[] NOT NULL DEFAULT '{}',
    included          text[] NOT NULL DEFAULT '{}',
    not_included      text[] NOT NULL DEFAULT '{}',
    what_to_bring     text[] NOT NULL DEFAULT '{}',
    requirements      text,
    important_info    text,
    seo_title         text,
    seo_description   text,
    og_media_id       uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    search_vector     tsvector GENERATED ALWAYS AS (
                          setweight(to_tsvector('simple'::regconfig, coalesce(name, '')), 'A') ||
                          setweight(to_tsvector('simple'::regconfig,
                              coalesce(subtitle, '') || ' ' || coalesce(short_description, '')), 'B') ||
                          setweight(to_tsvector('simple'::regconfig, coalesce(description, '')), 'C')
                      ) STORED,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, locale),
    UNIQUE (locale, slug)
);
CREATE INDEX product_translations_search_idx ON product_translations USING gin (search_vector);
CREATE INDEX product_translations_name_trgm_idx ON product_translations USING gin (name gin_trgm_ops);

CREATE TABLE product_categories (
    product_id  uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    category_id uuid NOT NULL REFERENCES categories (id) ON DELETE CASCADE,
    is_primary  boolean NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, category_id)
);
CREATE UNIQUE INDEX product_categories_single_primary ON product_categories (product_id) WHERE is_primary;
CREATE INDEX product_categories_category_idx ON product_categories (category_id);

CREATE TABLE product_tags (
    product_id uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    tag_id     uuid NOT NULL REFERENCES tags (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, tag_id)
);
CREATE INDEX product_tags_tag_idx ON product_tags (tag_id);

CREATE TABLE product_amenities (
    product_id uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    amenity_id uuid NOT NULL REFERENCES amenities (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, amenity_id)
);

CREATE TABLE related_products (
    product_id         uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    related_product_id uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    sort_order         integer NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, related_product_id),
    CHECK (product_id <> related_product_id)
);

-- ---- Detail tables (class-table inheritance) ----------------------------------------------

CREATE TABLE accommodation_types (
    product_id          uuid PRIMARY KEY,
    kind                text NOT NULL DEFAULT 'accommodation' CHECK (kind = 'accommodation'),
    accommodation_kind  text NOT NULL CHECK (accommodation_kind IN ('tent_pitch', 'campervan_pitch', 'caravan_pitch',
                            'furnished_tent', 'nomad_tent', 'bivouac', 'bungalow', 'room', 'dormitory_bed', 'other')),
    inventory_mode      text NOT NULL CHECK (inventory_mode IN ('unit', 'pool')),
    max_guests          integer NOT NULL CHECK (max_guests > 0),
    max_adults          integer CHECK (max_adults > 0),
    max_children        integer CHECK (max_children >= 0),
    standard_occupancy  integer NOT NULL DEFAULT 2 CHECK (standard_occupancy > 0),
    bed_configuration   jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(bed_configuration) = 'array'),
    surface_m2          numeric(7, 2) CHECK (surface_m2 > 0),
    min_nights          integer NOT NULL DEFAULT 1 CHECK (min_nights >= 1),
    max_nights          integer,
    check_in_from       time,
    check_in_until      time,
    check_out_until     time,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (product_id, kind) REFERENCES products (id, kind) ON DELETE CASCADE,
    CHECK (max_nights IS NULL OR max_nights >= min_nights),
    CHECK (standard_occupancy <= max_guests)
);

CREATE TABLE activities (
    product_id           uuid PRIMARY KEY,
    kind                 text NOT NULL DEFAULT 'activity' CHECK (kind = 'activity'),
    location_type        text NOT NULL CHECK (location_type IN ('on_site', 'off_site')),
    scheduling_mode      text NOT NULL CHECK (scheduling_mode IN ('fixed_slots', 'on_request', 'anytime')),
    duration_minutes     integer CHECK (duration_minutes > 0),
    difficulty           text CHECK (difficulty IN ('easy', 'moderate', 'challenging', 'extreme')),
    min_participants     integer NOT NULL DEFAULT 1 CHECK (min_participants >= 1),
    max_participants     integer,
    min_age              smallint CHECK (min_age >= 0),
    is_guided            boolean NOT NULL DEFAULT false,
    guide_languages      text[] NOT NULL DEFAULT '{}',
    meeting_place_id     uuid REFERENCES places (id) ON DELETE SET NULL,
    booking_cutoff_hours integer NOT NULL DEFAULT 12 CHECK (booking_cutoff_hours >= 0),
    pickup_available     boolean NOT NULL DEFAULT false,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (product_id, kind) REFERENCES products (id, kind) ON DELETE CASCADE,
    CHECK (max_participants IS NULL OR max_participants >= min_participants)
);

CREATE TABLE extras (
    product_id     uuid PRIMARY KEY,
    kind           text NOT NULL DEFAULT 'extra' CHECK (kind = 'extra'),
    applies_to     text NOT NULL CHECK (applies_to IN ('booking', 'accommodation', 'activity', 'any')),
    max_quantity   integer CHECK (max_quantity > 0),
    requires_date  boolean NOT NULL DEFAULT false,
    stock_quantity integer CHECK (stock_quantity >= 0),   -- NULL = unlimited
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (product_id, kind) REFERENCES products (id, kind) ON DELETE CASCADE
);

CREATE TABLE packages (
    product_id       uuid PRIMARY KEY,
    kind             text NOT NULL DEFAULT 'package' CHECK (kind = 'package'),
    duration_days    integer CHECK (duration_days > 0),
    duration_nights  integer CHECK (duration_nights >= 0),
    pricing_mode     text NOT NULL CHECK (pricing_mode IN ('fixed', 'sum_of_items', 'sum_minus_discount')),
    discount_percent numeric(5, 2) NOT NULL DEFAULT 0 CHECK (discount_percent BETWEEN 0 AND 100),
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (product_id, kind) REFERENCES products (id, kind) ON DELETE CASCADE
);

CREATE TABLE itinerary_steps (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id       uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    day_number       integer NOT NULL CHECK (day_number >= 1),
    sort_order       integer NOT NULL DEFAULT 0,
    place_id         uuid REFERENCES places (id) ON DELETE SET NULL,
    duration_minutes integer CHECK (duration_minutes > 0),
    media_id         uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX itinerary_steps_product_idx ON itinerary_steps (product_id, day_number, sort_order);

CREATE TABLE itinerary_step_translations (
    itinerary_step_id uuid NOT NULL REFERENCES itinerary_steps (id) ON DELETE CASCADE,
    locale            text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    title             text NOT NULL,
    description       text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (itinerary_step_id, locale)
);

CREATE TABLE product_extras (
    product_id       uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    extra_product_id uuid NOT NULL REFERENCES extras (product_id) ON DELETE CASCADE,
    is_mandatory     boolean NOT NULL DEFAULT false,
    sort_order       integer NOT NULL DEFAULT 0,
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, extra_product_id),
    CHECK (product_id <> extra_product_id)
);

CREATE TABLE package_items (
    id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    package_product_id uuid NOT NULL REFERENCES packages (product_id) ON DELETE CASCADE,
    item_product_id    uuid NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    quantity           integer NOT NULL DEFAULT 1 CHECK (quantity > 0),
    day_number         integer CHECK (day_number >= 1),
    is_optional        boolean NOT NULL DEFAULT false,
    sort_order         integer NOT NULL DEFAULT 0,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CHECK (package_product_id <> item_product_id)
);
CREATE INDEX package_items_package_idx ON package_items (package_product_id, sort_order);
CREATE INDEX package_items_item_idx ON package_items (item_product_id);
