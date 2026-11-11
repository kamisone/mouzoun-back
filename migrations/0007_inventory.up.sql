-- 0007_inventory: units, per-night inventory, activity schedules and slots, resources, closures.

CREATE TABLE accommodation_units (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id uuid NOT NULL REFERENCES accommodation_types (product_id) ON DELETE CASCADE,
    code       citext NOT NULL,
    name       text,
    place_id   uuid REFERENCES places (id) ON DELETE SET NULL,
    status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'maintenance', 'retired')),
    latitude   numeric(9, 6) CHECK (latitude BETWEEN -90 AND 90),
    longitude  numeric(9, 6) CHECK (longitude BETWEEN -180 AND 180),
    notes      text,
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (product_id, code)
);

CREATE TABLE inventory_calendar (
    product_id          uuid NOT NULL REFERENCES accommodation_types (product_id) ON DELETE CASCADE,
    day                 date NOT NULL,
    capacity            integer NOT NULL CHECK (capacity >= 0),
    reserved            integer NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    stop_sell           boolean NOT NULL DEFAULT false,
    min_nights_override smallint CHECK (min_nights_override >= 1),
    closed_to_arrival   boolean NOT NULL DEFAULT false,
    closed_to_departure boolean NOT NULL DEFAULT false,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, day),
    CHECK (reserved <= capacity)
);

CREATE TABLE activity_schedules (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id   uuid NOT NULL REFERENCES activities (product_id) ON DELETE CASCADE,
    days_of_week smallint NOT NULL DEFAULT 127 CHECK (days_of_week BETWEEN 1 AND 127),   -- Mon=1 … Sun=64
    start_time   time NOT NULL,                                                      -- Africa/Casablanca
    capacity     integer NOT NULL CHECK (capacity > 0),
    valid_period daterange NOT NULL CHECK (NOT isempty(valid_period)),
    is_active    boolean NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX activity_schedules_product_idx ON activity_schedules (product_id) WHERE is_active;

CREATE TABLE activity_slots (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id     uuid NOT NULL REFERENCES activities (product_id) ON DELETE CASCADE,
    schedule_id    uuid REFERENCES activity_schedules (id) ON DELETE SET NULL,
    starts_at      timestamptz NOT NULL,
    ends_at        timestamptz NOT NULL,
    capacity       integer NOT NULL CHECK (capacity >= 0),
    reserved       integer NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    status         text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed', 'full', 'cancelled')),
    guide_user_id  uuid REFERENCES users (id) ON DELETE SET NULL,
    guide_language text REFERENCES locales (code) ON UPDATE CASCADE ON DELETE SET NULL,
    notes          text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    UNIQUE (product_id, starts_at),
    CHECK (ends_at > starts_at),
    CHECK (reserved <= capacity)
);
CREATE INDEX activity_slots_starts_at_idx ON activity_slots (starts_at);

CREATE TABLE resources (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    kind       text NOT NULL CHECK (kind IN ('vehicle', 'equipment', 'animal', 'guide', 'room', 'other')),
    code       citext NOT NULL UNIQUE,        -- e.g. QUAD-01
    name       text NOT NULL,
    seats      integer CHECK (seats > 0),
    user_id    uuid REFERENCES users (id) ON DELETE SET NULL,   -- guides
    status     text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'maintenance', 'retired')),
    notes      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX resources_kind_idx ON resources (kind) WHERE status = 'active';

CREATE TABLE product_resource_requirements (
    product_id             uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    resource_kind          text NOT NULL CHECK (resource_kind IN ('vehicle', 'equipment', 'animal', 'guide', 'room', 'other')),
    units_per_group        integer NOT NULL DEFAULT 1 CHECK (units_per_group > 0),
    participants_per_group integer NOT NULL DEFAULT 1 CHECK (participants_per_group > 0),  -- e.g. 1 quad per 2
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, resource_kind)
);

CREATE TABLE closures (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    scope       text NOT NULL CHECK (scope IN ('site', 'product', 'unit', 'resource')),
    product_id  uuid REFERENCES products (id) ON DELETE CASCADE,
    unit_id     uuid REFERENCES accommodation_units (id) ON DELETE CASCADE,
    resource_id uuid REFERENCES resources (id) ON DELETE CASCADE,
    period      daterange NOT NULL CHECK (NOT isempty(period)),
    reason      text,
    is_public   boolean NOT NULL DEFAULT false,
    created_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (scope = 'site'     AND product_id IS NULL     AND unit_id IS NULL     AND resource_id IS NULL) OR
        (scope = 'product'  AND product_id IS NOT NULL AND unit_id IS NULL     AND resource_id IS NULL) OR
        (scope = 'unit'     AND product_id IS NULL     AND unit_id IS NOT NULL AND resource_id IS NULL) OR
        (scope = 'resource' AND product_id IS NULL     AND unit_id IS NULL     AND resource_id IS NOT NULL)
    )
);
CREATE INDEX closures_period_idx ON closures USING gist (period);
