-- 0027_shop: online shop for the farm products (dates, olive oil, spices, crafts, gift boxes).
--
-- products (kind = 'shop_product') → shop_products (1:1 details) → product_variants (what is
-- sold, priced, stocked and shipped) → inventory_levels per stock_location (on_hand changes only
-- through inventory_movements, an append-only ledger) → carts → orders (immutable snapshots)
-- with payments, fulfillments, returns and refunds as their own records.
--
-- See docs/shop/README.md. Numbered 0027 (the spec said 0014: taken by the email templates).

-- ================================================================================================
-- Changes to existing tables
-- ================================================================================================

ALTER TABLE products DROP CONSTRAINT products_kind_check;
ALTER TABLE products ADD CONSTRAINT products_kind_check
    CHECK (kind IN ('accommodation', 'activity', 'extra', 'package', 'shop_product'));

-- booking_items.product_kind keeps its own CHECK (it was a copy of products.kind): shop products
-- are sold through orders, never through bookings.

ALTER TABLE categories DROP CONSTRAINT categories_scope_check;
ALTER TABLE categories ADD CONSTRAINT categories_scope_check
    CHECK (scope IN ('product', 'post', 'faq', 'place', 'shop'));

-- ---- Shipping profiles first (shop_products references them) ----------------------------------
CREATE TABLE shipping_profiles (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       citext NOT NULL UNIQUE,
    name       text NOT NULL,
    is_default boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX shipping_profiles_single_default ON shipping_profiles (is_default) WHERE is_default;

-- ================================================================================================
-- Catalog
-- ================================================================================================

CREATE TABLE shop_products (
    product_id          uuid PRIMARY KEY,
    kind                text NOT NULL DEFAULT 'shop_product' CHECK (kind = 'shop_product'),
    product_type        text NOT NULL DEFAULT 'food' CHECK (product_type IN ('food', 'drink', 'cosmetic', 'craft',
                            'gift_box', 'other')),
    shipping_profile_id uuid NOT NULL REFERENCES shipping_profiles (id) ON DELETE RESTRICT,
    origin_place_id     uuid REFERENCES places (id) ON DELETE SET NULL,
    farm_crop_id        uuid REFERENCES farm_crops (id) ON DELETE SET NULL,
    is_perishable       boolean NOT NULL DEFAULT false,
    is_returnable       boolean NOT NULL DEFAULT true,
    track_lots          boolean NOT NULL DEFAULT false,
    shelf_life_days     integer CHECK (shelf_life_days > 0),
    allergens           text[] NOT NULL DEFAULT '{}' CHECK (allergens <@ ARRAY['gluten', 'crustaceans', 'eggs',
                            'fish', 'peanuts', 'soybeans', 'milk', 'nuts', 'celery', 'mustard', 'sesame',
                            'sulphites', 'lupin', 'molluscs']::text[]),
    nutrition           jsonb CHECK (nutrition IS NULL OR jsonb_typeof(nutrition) = 'object'),
    option_count        smallint NOT NULL DEFAULT 0 CHECK (option_count BETWEEN 0 AND 3),
    max_per_order       integer CHECK (max_per_order > 0),
    available_period    tstzrange CHECK (available_period IS NULL OR NOT isempty(available_period)),
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (product_id, kind) REFERENCES products (id, kind) ON DELETE CASCADE
);
CREATE INDEX shop_products_profile_idx ON shop_products (shipping_profile_id);

CREATE TABLE shop_product_translations (
    product_id           uuid NOT NULL REFERENCES shop_products (product_id) ON DELETE CASCADE,
    locale               text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    ingredients          text,
    storage_instructions text,
    usage_tips           text,
    origin_story         text,
    producer_name        text,
    legal_notice         text,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, locale)
);

CREATE TABLE product_options (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id uuid NOT NULL REFERENCES shop_products (product_id) ON DELETE CASCADE,
    code       citext NOT NULL CHECK (code ~ '^[a-z][a-z0-9_]*$'),
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (product_id, code),
    UNIQUE (id, product_id)
);

CREATE TABLE product_option_translations (
    option_id  uuid NOT NULL REFERENCES product_options (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (option_id, locale)
);

CREATE TABLE product_option_values (
    id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    option_id       uuid NOT NULL REFERENCES product_options (id) ON DELETE CASCADE,
    code            citext NOT NULL CHECK (code ~ '^[a-z0-9][a-z0-9_]*$'),
    sort_order      integer NOT NULL DEFAULT 0,
    swatch_media_id uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (option_id, code),
    UNIQUE (id, option_id)
);

CREATE TABLE product_option_value_translations (
    option_value_id uuid NOT NULL REFERENCES product_option_values (id) ON DELETE CASCADE,
    locale          text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    label           text NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (option_value_id, locale)
);

CREATE TABLE product_variants (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id             uuid NOT NULL REFERENCES shop_products (product_id) ON DELETE CASCADE,
    sku                    citext NOT NULL UNIQUE,
    barcode                text CHECK (barcode ~ '^[0-9]{8,14}$'),
    status                 text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived')),
    is_default             boolean NOT NULL DEFAULT false,
    sort_order             integer NOT NULL DEFAULT 0,
    price_minor            bigint NOT NULL CHECK (price_minor >= 0),
    compare_at_price_minor bigint,
    cost_minor             bigint CHECK (cost_minor >= 0),
    currency               char(3) NOT NULL DEFAULT 'EUR' REFERENCES currencies (code) ON UPDATE CASCADE,
    net_quantity           numeric(10, 3) CHECK (net_quantity > 0),
    net_unit               text CHECK (net_unit IN ('g', 'kg', 'ml', 'l', 'piece')),
    weight_grams           integer NOT NULL CHECK (weight_grams > 0),
    length_mm              integer CHECK (length_mm > 0),
    width_mm               integer CHECK (width_mm > 0),
    height_mm              integer CHECK (height_mm > 0),
    requires_shipping      boolean NOT NULL DEFAULT true,
    is_taxable             boolean NOT NULL DEFAULT true,
    track_inventory        boolean NOT NULL DEFAULT true,
    low_stock_threshold    integer CHECK (low_stock_threshold >= 0),
    cover_media_id         uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    deleted_at             timestamptz,
    CHECK (compare_at_price_minor IS NULL OR compare_at_price_minor > price_minor),
    CHECK ((net_quantity IS NULL) = (net_unit IS NULL))
);
CREATE INDEX product_variants_product_idx ON product_variants (product_id, sort_order) WHERE deleted_at IS NULL;
CREATE UNIQUE INDEX product_variants_single_default ON product_variants (product_id)
    WHERE is_default AND deleted_at IS NULL;

CREATE TABLE product_variant_media (
    variant_id uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    media_id   uuid NOT NULL REFERENCES media_assets (id) ON DELETE CASCADE,
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (variant_id, media_id)
);

-- (option_id, option_value_id) must belong together: composite FK on (id, option_id).
CREATE TABLE product_variant_option_values (
    variant_id      uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    option_id       uuid NOT NULL REFERENCES product_options (id) ON DELETE CASCADE,
    option_value_id uuid NOT NULL,
    created_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (variant_id, option_id),
    FOREIGN KEY (option_value_id, option_id) REFERENCES product_option_values (id, option_id) ON DELETE CASCADE
);
CREATE INDEX product_variant_option_values_value_idx ON product_variant_option_values (option_value_id);

-- ================================================================================================
-- Inventory
-- ================================================================================================

CREATE TABLE stock_locations (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                   citext NOT NULL UNIQUE,
    place_id               uuid REFERENCES places (id) ON DELETE SET NULL,
    address                jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(address) = 'object'),
    is_active              boolean NOT NULL DEFAULT true,
    fulfills_online_orders boolean NOT NULL DEFAULT true,
    is_pickup_point        boolean NOT NULL DEFAULT false,
    sort_order             integer NOT NULL DEFAULT 0,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE stock_location_translations (
    location_id         uuid NOT NULL REFERENCES stock_locations (id) ON DELETE CASCADE,
    locale              text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name                text NOT NULL,
    pickup_instructions text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (location_id, locale)
);

-- The database refuses to oversell: reserved can never exceed what is on hand plus the
-- allowed backorder. on_hand may go below zero only by the backorder limit (deviation: the spec
-- had on_hand >= 0, which would make a backorder impossible to consume).
CREATE TABLE inventory_levels (
    variant_id      uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    location_id     uuid NOT NULL REFERENCES stock_locations (id) ON DELETE RESTRICT,
    on_hand         integer NOT NULL DEFAULT 0,
    reserved        integer NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    backorder_limit integer NOT NULL DEFAULT 0 CHECK (backorder_limit >= 0),
    reorder_point   integer CHECK (reorder_point >= 0),
    bin_location    text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (variant_id, location_id),
    CONSTRAINT inventory_levels_on_hand_floor CHECK (on_hand + backorder_limit >= 0),
    CONSTRAINT inventory_levels_no_oversell CHECK (reserved <= on_hand + backorder_limit)
);
CREATE INDEX inventory_levels_location_idx ON inventory_levels (location_id);

CREATE TABLE inventory_lots (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    variant_id   uuid NOT NULL,
    location_id  uuid NOT NULL,
    lot_code     citext NOT NULL,
    produced_on  date,
    harvested_on date,
    best_before  date,
    quantity     integer NOT NULL DEFAULT 0 CHECK (quantity >= 0),
    notes        text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (variant_id, location_id, lot_code),
    FOREIGN KEY (variant_id, location_id) REFERENCES inventory_levels (variant_id, location_id) ON DELETE CASCADE
);
CREATE INDEX inventory_lots_best_before_idx ON inventory_lots (best_before) WHERE quantity > 0;

-- ================================================================================================
-- Customers and carts
-- ================================================================================================

CREATE TABLE customer_addresses (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    customer_id    uuid NOT NULL REFERENCES customers (id) ON DELETE CASCADE,
    label          text,
    first_name     text NOT NULL,
    last_name      text NOT NULL,
    company        text,
    line1          text NOT NULL,
    line2          text,
    city           text NOT NULL,
    region         text,
    postal_code    text,
    country_code   char(2) NOT NULL CHECK (country_code ~ '^[A-Z]{2}$'),
    phone          text NOT NULL,
    delivery_notes text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    deleted_at     timestamptz
);
CREATE INDEX customer_addresses_customer_idx ON customer_addresses (customer_id) WHERE deleted_at IS NULL;

ALTER TABLE customers
    ADD COLUMN default_shipping_address_id uuid REFERENCES customer_addresses (id) ON DELETE SET NULL,
    ADD COLUMN default_billing_address_id  uuid REFERENCES customer_addresses (id) ON DELETE SET NULL;

-- ================================================================================================
-- Shipping
-- ================================================================================================

CREATE TABLE shipping_zones (
    id                   uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                 citext NOT NULL UNIQUE,
    country_codes        char(2)[] NOT NULL CHECK (cardinality(country_codes) > 0),
    region_codes         text[],
    postal_code_patterns text[],
    is_export            boolean NOT NULL DEFAULT false,
    is_active            boolean NOT NULL DEFAULT true,
    sort_order           integer NOT NULL DEFAULT 0,
    created_at           timestamptz NOT NULL DEFAULT now(),
    updated_at           timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX shipping_zones_countries_idx ON shipping_zones USING gin (country_codes);

CREATE TABLE shipping_zone_translations (
    zone_id    uuid NOT NULL REFERENCES shipping_zones (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (zone_id, locale)
);

CREATE TABLE shipping_rates (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    zone_id                 uuid NOT NULL REFERENCES shipping_zones (id) ON DELETE CASCADE,
    profile_id              uuid NOT NULL REFERENCES shipping_profiles (id) ON DELETE CASCADE,
    code                    citext NOT NULL,
    kind                    text NOT NULL CHECK (kind IN ('flat', 'weight_based', 'price_based', 'free')),
    carrier                 text,
    amount_minor            bigint CHECK (amount_minor >= 0),
    per_kg_minor            bigint CHECK (per_kg_minor >= 0),
    min_weight_grams        integer CHECK (min_weight_grams >= 0),
    max_weight_grams        integer CHECK (max_weight_grams > 0),
    min_subtotal_minor      bigint CHECK (min_subtotal_minor >= 0),
    max_subtotal_minor      bigint CHECK (max_subtotal_minor >= 0),
    free_over_minor         bigint CHECK (free_over_minor >= 0),
    delivery_days_min       integer CHECK (delivery_days_min >= 0),
    delivery_days_max       integer CHECK (delivery_days_max >= 0),
    allows_cash_on_delivery boolean NOT NULL DEFAULT false,
    cod_fee_minor           bigint CHECK (cod_fee_minor >= 0),
    is_active               boolean NOT NULL DEFAULT true,
    sort_order              integer NOT NULL DEFAULT 0,
    currency                char(3) NOT NULL DEFAULT 'EUR' REFERENCES currencies (code) ON UPDATE CASCADE,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    UNIQUE (zone_id, profile_id, code),
    CHECK (kind = 'free' OR amount_minor IS NOT NULL),
    CHECK (kind <> 'weight_based' OR per_kg_minor IS NOT NULL),
    CHECK (max_weight_grams IS NULL OR min_weight_grams IS NULL OR max_weight_grams >= min_weight_grams),
    CHECK (max_subtotal_minor IS NULL OR min_subtotal_minor IS NULL OR max_subtotal_minor >= min_subtotal_minor),
    CHECK (delivery_days_max IS NULL OR delivery_days_min IS NULL OR delivery_days_max >= delivery_days_min)
);
CREATE INDEX shipping_rates_zone_profile_idx ON shipping_rates (zone_id, profile_id) WHERE is_active;

CREATE TABLE shipping_rate_translations (
    rate_id     uuid NOT NULL REFERENCES shipping_rates (id) ON DELETE CASCADE,
    locale      text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name        text NOT NULL,
    description text,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (rate_id, locale)
);

-- Carts live in Postgres (cross-device, abandoned-cart emails). Adding to the cart reserves
-- nothing: stock is reserved when the order is created at checkout.
CREATE TABLE carts (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    token_hash              bytea NOT NULL UNIQUE,
    customer_id             uuid REFERENCES customers (id) ON DELETE SET NULL,
    email                   citext,
    locale                  text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    currency                char(3) NOT NULL DEFAULT 'EUR' REFERENCES currencies (code) ON UPDATE CASCADE,
    status                  text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'converted', 'abandoned', 'expired')),
    fulfillment_method      text CHECK (fulfillment_method IN ('shipping', 'pickup', 'stay_delivery')),
    shipping_address        jsonb CHECK (shipping_address IS NULL OR jsonb_typeof(shipping_address) = 'object'),
    billing_address         jsonb CHECK (billing_address IS NULL OR jsonb_typeof(billing_address) = 'object'),
    -- Deviation: one chosen rate per shipping profile (the spec had a single shipping_rate_id,
    -- which cannot express "standard + liquids" carts). Profiles without a choice get the cheapest.
    shipping_rate_ids       uuid[] NOT NULL DEFAULT '{}',
    pickup_location_id      uuid REFERENCES stock_locations (id) ON DELETE SET NULL,
    booking_id              uuid REFERENCES bookings (id) ON DELETE SET NULL,
    promotion_codes         citext[] NOT NULL DEFAULT '{}',
    converted_order_id      uuid,
    abandoned_email_sent_at timestamptz,
    last_activity_at        timestamptz NOT NULL DEFAULT now(),
    expires_at              timestamptz NOT NULL,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX carts_activity_idx ON carts (last_activity_at) WHERE status = 'active';
CREATE INDEX carts_customer_idx ON carts (customer_id) WHERE customer_id IS NOT NULL;

CREATE TABLE cart_items (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    cart_id    uuid NOT NULL REFERENCES carts (id) ON DELETE CASCADE,
    variant_id uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    quantity   integer NOT NULL CHECK (quantity BETWEEN 1 AND 999),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (cart_id, variant_id)
);

-- ================================================================================================
-- Orders
-- ================================================================================================

-- Order numbers MZS-<yy>-<000123> may have gaps; invoices stay gap-free (invoice_counters).
CREATE SEQUENCE order_number_seq;
CREATE SEQUENCE order_return_number_seq;

CREATE TABLE orders (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    number                  text NOT NULL UNIQUE CHECK (number ~ '^MZS-[0-9]{2}-[0-9]{6,}$'),
    customer_id             uuid NOT NULL REFERENCES customers (id) ON DELETE RESTRICT,
    cart_id                 uuid REFERENCES carts (id) ON DELETE SET NULL,
    booking_id              uuid REFERENCES bookings (id) ON DELETE SET NULL,
    channel                 text NOT NULL DEFAULT 'website' CHECK (channel IN ('website', 'admin', 'camp_pos', 'phone', 'whatsapp')),
    status                  text NOT NULL DEFAULT 'pending' CHECK (status IN ('draft', 'pending', 'open', 'completed', 'cancelled')),
    payment_status          text NOT NULL DEFAULT 'unpaid' CHECK (payment_status IN ('unpaid', 'pending', 'authorized',
                                'partially_paid', 'paid', 'partially_refunded', 'refunded', 'voided')),
    fulfillment_status      text NOT NULL DEFAULT 'unfulfilled' CHECK (fulfillment_status IN ('unfulfilled',
                                'partially_fulfilled', 'fulfilled', 'partially_returned', 'returned')),
    payment_method          text CHECK (payment_method IN ('stripe', 'cash_on_delivery', 'pay_at_pickup', 'bank_transfer')),
    requires_confirmation   boolean NOT NULL DEFAULT false,
    confirmed_at            timestamptz,
    confirmed_by            uuid REFERENCES users (id) ON DELETE SET NULL,
    email                   citext NOT NULL,
    phone                   text,
    locale                  text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    currency                char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    fulfillment_method      text NOT NULL CHECK (fulfillment_method IN ('shipping', 'pickup', 'stay_delivery')),
    shipping_address        jsonb CHECK (shipping_address IS NULL OR jsonb_typeof(shipping_address) = 'object'),
    billing_address         jsonb CHECK (billing_address IS NULL OR jsonb_typeof(billing_address) = 'object'),
    pickup_location_id      uuid REFERENCES stock_locations (id) ON DELETE SET NULL,
    shipping_lines          jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(shipping_lines) = 'array'),
    promotion_codes         citext[] NOT NULL DEFAULT '{}',
    subtotal_minor          bigint NOT NULL DEFAULT 0 CHECK (subtotal_minor >= 0),
    discount_minor          bigint NOT NULL DEFAULT 0 CHECK (discount_minor >= 0),
    shipping_minor          bigint NOT NULL DEFAULT 0 CHECK (shipping_minor >= 0),
    shipping_discount_minor bigint NOT NULL DEFAULT 0 CHECK (shipping_discount_minor >= 0),
    tax_minor               bigint NOT NULL DEFAULT 0 CHECK (tax_minor >= 0),
    cod_fee_minor           bigint NOT NULL DEFAULT 0 CHECK (cod_fee_minor >= 0),
    total_minor             bigint NOT NULL DEFAULT 0 CHECK (total_minor >= 0),
    taxes_included          boolean NOT NULL DEFAULT true,
    amount_paid_minor       bigint NOT NULL DEFAULT 0 CHECK (amount_paid_minor >= 0),
    amount_refunded_minor   bigint NOT NULL DEFAULT 0 CHECK (amount_refunded_minor >= 0),
    customer_note           text,
    internal_notes          text,
    tags                    text[] NOT NULL DEFAULT '{}',
    risk_level              text NOT NULL DEFAULT 'normal' CHECK (risk_level IN ('normal', 'review', 'high')),
    -- Checkout replay protection that survives a Redis outage (same key → same order).
    checkout_key            text UNIQUE,
    terms_accepted_at       timestamptz,
    placed_at               timestamptz,
    paid_at                 timestamptz,
    completed_at            timestamptz,
    cancelled_at            timestamptz,
    cancel_reason           text CHECK (cancel_reason IN ('customer', 'out_of_stock', 'fraud', 'payment_failed',
                                'unconfirmed', 'other')),
    cancelled_by            text CHECK (cancelled_by IN ('customer', 'staff', 'system')),
    created_by              uuid REFERENCES users (id) ON DELETE SET NULL,
    ip_address              inet,
    user_agent              text,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT orders_total_check CHECK (total_minor = subtotal_minor - discount_minor + shipping_minor
        - shipping_discount_minor + cod_fee_minor + CASE WHEN taxes_included THEN 0 ELSE tax_minor END),
    CONSTRAINT orders_refund_check CHECK (amount_refunded_minor <= amount_paid_minor),
    CHECK (fulfillment_method <> 'pickup' OR status = 'draft' OR pickup_location_id IS NOT NULL),
    CHECK ((status = 'cancelled') = (cancelled_at IS NOT NULL))
);
CREATE INDEX orders_customer_idx ON orders (customer_id);
CREATE INDEX orders_status_placed_idx ON orders (status, placed_at DESC);
CREATE INDEX orders_payment_status_idx ON orders (payment_status);
CREATE INDEX orders_fulfillment_status_idx ON orders (fulfillment_status);
CREATE INDEX orders_booking_idx ON orders (booking_id) WHERE booking_id IS NOT NULL;
CREATE INDEX orders_number_trgm_idx ON orders USING gin ((number::text) gin_trgm_ops);
CREATE INDEX orders_email_trgm_idx ON orders USING gin ((email::text) gin_trgm_ops);

ALTER TABLE carts ADD CONSTRAINT carts_converted_order_fkey
    FOREIGN KEY (converted_order_id) REFERENCES orders (id) ON DELETE SET NULL;

CREATE TABLE order_items (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id               uuid NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    variant_id             uuid NOT NULL REFERENCES product_variants (id) ON DELETE RESTRICT,
    product_id             uuid NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    sku                    text NOT NULL,
    quantity               integer NOT NULL CHECK (quantity > 0),
    unit_price_minor       bigint NOT NULL CHECK (unit_price_minor >= 0),
    compare_at_price_minor bigint,
    subtotal_minor         bigint NOT NULL CHECK (subtotal_minor >= 0),
    discount_minor         bigint NOT NULL DEFAULT 0 CHECK (discount_minor >= 0),
    tax_minor              bigint NOT NULL DEFAULT 0 CHECK (tax_minor >= 0),
    total_minor            bigint NOT NULL CHECK (total_minor >= 0),
    tax_lines              jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(tax_lines) = 'array'),
    discount_allocations   jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(discount_allocations) = 'array'),
    product_snapshot       jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(product_snapshot) = 'object'),
    fulfilled_quantity     integer NOT NULL DEFAULT 0 CHECK (fulfilled_quantity >= 0),
    returned_quantity      integer NOT NULL DEFAULT 0 CHECK (returned_quantity >= 0),
    refunded_quantity      integer NOT NULL DEFAULT 0 CHECK (refunded_quantity >= 0),
    sort_order             integer NOT NULL DEFAULT 0,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT order_items_quantities_check CHECK (fulfilled_quantity <= quantity
        AND returned_quantity <= fulfilled_quantity AND refunded_quantity <= quantity),
    CHECK (discount_minor <= subtotal_minor)
);
CREATE INDEX order_items_order_idx ON order_items (order_id, sort_order);
CREATE INDEX order_items_variant_idx ON order_items (variant_id);
CREATE INDEX order_items_product_idx ON order_items (product_id);

CREATE TABLE order_events (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    order_id      uuid NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    kind          text NOT NULL CHECK (kind ~ '^[a-z_]+$'),
    from_status   text,
    to_status     text,
    message       text,
    data          jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(data) = 'object'),
    actor_type    text NOT NULL CHECK (actor_type IN ('customer', 'staff', 'system', 'webhook')),
    actor_user_id uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX order_events_order_idx ON order_events (order_id, created_at);

CREATE TABLE fulfillments (
    id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id         uuid NOT NULL REFERENCES orders (id) ON DELETE RESTRICT,
    location_id      uuid NOT NULL REFERENCES stock_locations (id) ON DELETE RESTRICT,
    method           text NOT NULL CHECK (method IN ('shipping', 'pickup', 'stay_delivery')),
    status           text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'packed', 'ready_for_pickup', 'shipped',
                         'in_transit', 'delivered', 'picked_up', 'failed', 'cancelled')),
    carrier          text,
    tracking_number  text,
    tracking_url     text CHECK (tracking_url ~ '^https?://'),
    pickup_code_hash bytea,
    packed_at        timestamptz,
    shipped_at       timestamptz,
    delivered_at     timestamptz,
    created_by       uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX fulfillments_order_idx ON fulfillments (order_id);
CREATE INDEX fulfillments_status_idx ON fulfillments (status) WHERE status NOT IN ('delivered', 'picked_up', 'cancelled');

CREATE TABLE fulfillment_items (
    fulfillment_id uuid NOT NULL REFERENCES fulfillments (id) ON DELETE CASCADE,
    order_item_id  uuid NOT NULL REFERENCES order_items (id) ON DELETE RESTRICT,
    quantity       integer NOT NULL CHECK (quantity > 0),
    -- Lots picked (FEFO), [{lot_id, lot_code, best_before, quantity}].
    lots           jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(lots) = 'array'),
    created_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (fulfillment_id, order_item_id)
);

CREATE TABLE order_returns (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id      uuid NOT NULL REFERENCES orders (id) ON DELETE RESTRICT,
    number        text NOT NULL UNIQUE CHECK (number ~ '^MZR-[0-9]{2}-[0-9]{6,}$'),
    status        text NOT NULL DEFAULT 'requested' CHECK (status IN ('requested', 'approved', 'rejected', 'received', 'closed')),
    reason        text NOT NULL CHECK (reason IN ('damaged', 'wrong_item', 'quality', 'not_as_described', 'changed_mind', 'other')),
    customer_note text,
    staff_note    text,
    requested_at  timestamptz NOT NULL DEFAULT now(),
    received_at   timestamptz,
    created_by    uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX order_returns_order_idx ON order_returns (order_id);
CREATE INDEX order_returns_open_idx ON order_returns (status) WHERE status IN ('requested', 'approved');

CREATE TABLE return_items (
    return_id     uuid NOT NULL REFERENCES order_returns (id) ON DELETE CASCADE,
    order_item_id uuid NOT NULL REFERENCES order_items (id) ON DELETE RESTRICT,
    quantity      integer NOT NULL CHECK (quantity > 0),
    restock       boolean NOT NULL DEFAULT false,
    condition     text NOT NULL DEFAULT 'sellable' CHECK (condition IN ('sellable', 'damaged', 'expired')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (return_id, order_item_id)
);

-- ---- Ledger and reservations (reference orders, fulfillments and returns) -----------------------

-- Append-only: every change to inventory_levels.on_hand writes exactly one row here, in the same
-- transaction. UPDATE and DELETE are refused by a trigger.
CREATE TABLE inventory_movements (
    id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    variant_id      uuid NOT NULL REFERENCES product_variants (id) ON DELETE RESTRICT,
    location_id     uuid NOT NULL REFERENCES stock_locations (id) ON DELETE RESTRICT,
    lot_id          uuid REFERENCES inventory_lots (id) ON DELETE RESTRICT,
    delta           integer NOT NULL CHECK (delta <> 0),
    on_hand_after   integer NOT NULL,
    reason          text NOT NULL CHECK (reason IN ('receipt', 'sale', 'return_restock', 'adjustment', 'count',
                        'damage', 'expired', 'transfer_in', 'transfer_out', 'correction')),
    order_id        uuid REFERENCES orders (id) ON DELETE RESTRICT,
    fulfillment_id  uuid REFERENCES fulfillments (id) ON DELETE RESTRICT,
    return_id       uuid REFERENCES order_returns (id) ON DELETE RESTRICT,
    transfer_id     uuid,
    unit_cost_minor bigint CHECK (unit_cost_minor >= 0),
    note            text,
    created_by      uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX inventory_movements_level_idx ON inventory_movements (variant_id, location_id, created_at DESC);
CREATE INDEX inventory_movements_order_idx ON inventory_movements (order_id) WHERE order_id IS NOT NULL;
CREATE INDEX inventory_movements_transfer_idx ON inventory_movements (transfer_id) WHERE transfer_id IS NOT NULL;

CREATE FUNCTION inventory_movements_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'inventory_movements is append-only (% refused)', TG_OP
        USING ERRCODE = 'insufficient_privilege';
END;
$$;
CREATE TRIGGER inventory_movements_append_only BEFORE UPDATE OR DELETE ON inventory_movements
    FOR EACH ROW EXECUTE FUNCTION inventory_movements_append_only();

CREATE TABLE inventory_reservations (
    id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id      uuid NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    order_item_id uuid NOT NULL REFERENCES order_items (id) ON DELETE CASCADE,
    variant_id    uuid NOT NULL REFERENCES product_variants (id) ON DELETE RESTRICT,
    location_id   uuid NOT NULL REFERENCES stock_locations (id) ON DELETE RESTRICT,
    quantity      integer NOT NULL CHECK (quantity > 0),
    expires_at    timestamptz,                 -- NULL: held until staff confirm or cancel (COD)
    released_at   timestamptz,
    consumed_at   timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (released_at IS NULL OR consumed_at IS NULL)
);
CREATE INDEX inventory_reservations_order_idx ON inventory_reservations (order_id);
CREATE INDEX inventory_reservations_expiry_idx ON inventory_reservations (expires_at)
    WHERE released_at IS NULL AND consumed_at IS NULL;

CREATE TABLE stock_notifications (
    id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    variant_id  uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    email       citext NOT NULL,
    locale      text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    customer_id uuid REFERENCES customers (id) ON DELETE SET NULL,
    notified_at timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX stock_notifications_pending_uniq ON stock_notifications (variant_id, email) WHERE notified_at IS NULL;

-- ================================================================================================
-- Payments, refunds, invoices, promotions, reviews: shared with bookings
-- ================================================================================================

ALTER TABLE payments
    ALTER COLUMN booking_id DROP NOT NULL,
    ADD COLUMN order_id uuid REFERENCES orders (id) ON DELETE RESTRICT,
    ADD COLUMN provider_session_id text,
    ADD CONSTRAINT payments_owner_check CHECK (num_nonnulls(booking_id, order_id) = 1),
    ADD CONSTRAINT payments_provider_session_uniq UNIQUE (provider, provider_session_id);
CREATE INDEX payments_order_idx ON payments (order_id) WHERE order_id IS NOT NULL;
ALTER TABLE payments DROP CONSTRAINT payments_provider_check;
ALTER TABLE payments ADD CONSTRAINT payments_provider_check CHECK (provider IN ('cmi', 'stripe', 'paypal', 'cash',
    'bank_transfer', 'card_terminal', 'other', 'cash_on_delivery', 'pay_at_pickup'));

ALTER TABLE refunds
    ADD COLUMN return_id uuid REFERENCES order_returns (id) ON DELETE SET NULL,
    ADD COLUMN lines     jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(lines) = 'array'),
    ADD COLUMN restock   boolean NOT NULL DEFAULT false;

-- Same gap-free yearly numbering as booking invoices (one legal sequence for the business).
ALTER TABLE invoices
    ALTER COLUMN booking_id DROP NOT NULL,
    ADD COLUMN order_id uuid REFERENCES orders (id) ON DELETE RESTRICT,
    ADD CONSTRAINT invoices_owner_check CHECK (num_nonnulls(booking_id, order_id) = 1);
CREATE INDEX invoices_order_idx ON invoices (order_id) WHERE order_id IS NOT NULL;

ALTER TABLE promotions
    ADD COLUMN scope                text NOT NULL DEFAULT 'booking' CHECK (scope IN ('booking', 'shop', 'all')),
    ADD COLUMN applies_to_shipping  boolean NOT NULL DEFAULT false,
    ADD COLUMN min_quantity         integer CHECK (min_quantity > 0),
    ADD COLUMN customer_eligibility text NOT NULL DEFAULT 'all' CHECK (customer_eligibility IN ('all', 'first_order',
        'staying_guests'));
ALTER TABLE promotions DROP CONSTRAINT promotions_discount_type_check;
ALTER TABLE promotions ADD CONSTRAINT promotions_discount_type_check
    CHECK (discount_type IN ('percent', 'fixed_amount', 'free_nights', 'free_shipping'));

CREATE TABLE promotion_categories (
    promotion_id uuid NOT NULL REFERENCES promotions (id) ON DELETE CASCADE,
    category_id  uuid NOT NULL REFERENCES categories (id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (promotion_id, category_id)
);

ALTER TABLE promotion_redemptions
    ALTER COLUMN booking_id DROP NOT NULL,
    ADD COLUMN order_id uuid REFERENCES orders (id) ON DELETE CASCADE,
    ADD CONSTRAINT promotion_redemptions_owner_check CHECK (num_nonnulls(booking_id, order_id) = 1),
    ADD CONSTRAINT promotion_redemptions_order_uniq UNIQUE (promotion_id, order_id);

ALTER TABLE reviews ADD COLUMN order_id uuid REFERENCES orders (id) ON DELETE SET NULL;

-- ---- Navigation: a "shop" link (the shop home, or one of its categories) -----------------------
DO $$
DECLARE c record;
BEGIN
    FOR c IN SELECT conname FROM pg_constraint
             WHERE conrelid = 'nav_menu_items'::regclass AND contype = 'c'
               AND (pg_get_constraintdef(oid) LIKE '%num_nonnulls%' OR pg_get_constraintdef(oid) LIKE '%CASE link_type%'
                    OR conname = 'nav_menu_items_link_type_check')
    LOOP
        EXECUTE format('ALTER TABLE nav_menu_items DROP CONSTRAINT %I', c.conname);
    END LOOP;
END $$;
ALTER TABLE nav_menu_items
    ADD CONSTRAINT nav_menu_items_link_type_check
        CHECK (link_type IN ('page', 'product', 'category', 'place', 'url', 'route', 'shop')),
    ADD CONSTRAINT nav_menu_items_target_count
        CHECK (num_nonnulls(page_id, product_id, category_id, place_id, url, route) = CASE link_type
                   WHEN 'shop' THEN num_nonnulls(category_id) ELSE 1 END),
    ADD CONSTRAINT nav_menu_items_target_check
        CHECK (CASE link_type
                   WHEN 'page'     THEN page_id IS NOT NULL
                   WHEN 'product'  THEN product_id IS NOT NULL
                   WHEN 'category' THEN category_id IS NOT NULL
                   WHEN 'place'    THEN place_id IS NOT NULL
                   WHEN 'url'      THEN url IS NOT NULL
                   WHEN 'route'    THEN route IS NOT NULL
                   WHEN 'shop'     THEN true
               END);

-- ================================================================================================
-- updated_at triggers
-- ================================================================================================
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['shipping_profiles', 'shop_products', 'shop_product_translations', 'product_options',
        'product_option_translations', 'product_option_values', 'product_option_value_translations',
        'product_variants', 'stock_locations', 'stock_location_translations', 'inventory_levels', 'inventory_lots',
        'customer_addresses', 'shipping_zones', 'shipping_zone_translations', 'shipping_rates',
        'shipping_rate_translations', 'carts', 'cart_items', 'orders', 'order_items', 'fulfillments',
        'order_returns']
    LOOP
        EXECUTE format('CREATE TRIGGER %I BEFORE UPDATE ON %I FOR EACH ROW EXECUTE FUNCTION set_updated_at()',
                       t || '_set_updated_at', t);
    END LOOP;
END $$;

-- ================================================================================================
-- Permissions, settings
-- ================================================================================================

INSERT INTO permissions (code, module, description) VALUES
    ('shop.read',     'shop', 'View the shop: products, stock, orders and reports'),
    ('shop.catalog',  'shop', 'Edit shop products, variants, options, categories and media'),
    ('shop.pricing',  'shop', 'Edit shop prices, promotions, shipping and taxes'),
    ('shop.stock',    'shop', 'Receive, adjust, count and transfer stock; manage lots'),
    ('shop.orders',   'shop', 'View, edit, confirm, fulfil and cancel orders'),
    ('shop.refunds',  'shop', 'Handle returns and refunds'),
    ('shop.settings', 'shop', 'Edit shop settings and payment methods');

INSERT INTO role_permissions (role_id, permission_code)
SELECT r.id, p.code
FROM roles r
JOIN permissions p ON p.module = 'shop'
WHERE r.code IN ('super_admin', 'manager')
   OR (r.code = 'reception' AND p.code IN ('shop.read', 'shop.orders'))
   OR (r.code = 'editor' AND p.code IN ('shop.read', 'shop.catalog'))
ON CONFLICT DO NOTHING;

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('shop.status', 'shop', 'string', '"catalog_only"', true, 'closed | catalog_only (products visible, buying disabled) | open'),
    ('shop.currency', 'shop', 'string', '"EUR"', true, 'Currency shop prices are set and charged in'),
    ('shop.prices_include_tax', 'shop', 'boolean', 'true', true, 'Prices shown to customers include TVA'),
    ('shop.guest_checkout', 'shop', 'boolean', 'true', true, 'Customers can order without an account'),
    ('shop.checkout_hold_minutes', 'shop', 'number', '30', false, 'How long stock is held for an unpaid online payment (Stripe minimum: 30)'),
    ('shop.cart_ttl_days', 'shop', 'number', '30', false, 'Days an inactive cart is kept'),
    ('shop.low_stock_threshold', 'shop', 'number', '5', false, 'Default "low stock" level when a variant has none'),
    ('shop.min_order_minor', 'shop', 'number', '0', true, 'Minimum order subtotal, in cents'),
    ('shop.max_quantity_per_line', 'shop', 'number', '20', true, 'Maximum quantity of one variant in an order'),
    ('shop.ships_to_countries', 'shop', 'json', '["MA"]', true, 'ISO country codes offered in the address form'),
    ('shop.carriers', 'shop', 'json', '["amana", "ctm", "dhl"]', false, 'Carriers offered on shipping rates and fulfilments'),
    ('shop.withdrawal_days', 'shop', 'number', '7', true, 'Consumer withdrawal period for distance sales, in days (to confirm with a lawyer)'),
    ('shop.stay_delivery_enabled', 'shop', 'boolean', 'true', true, 'Guests can have orders delivered to their tent/pitch'),
    ('shop.stay_delivery_fee_minor', 'shop', 'number', '0', true, 'Fee for delivery to the tent/pitch, in cents'),
    ('shop.shipping_tax_id', 'shop', 'json', 'null', false, 'Tax applied to shipping (taxes.id), null = none'),
    ('shop.export_zero_rated', 'shop', 'boolean', 'false', false, 'Export zones (is_export) are charged 0 % TVA (to confirm with an accountant)'),
    ('shop.abandoned_cart_hours', 'shop', 'number', '4', false, 'Hours of inactivity before the abandoned-cart email'),
    ('shop.abandoned_cart_requires_consent', 'shop', 'boolean', 'true', false, 'Abandoned-cart emails only to customers who accepted marketing'),
    ('shop.notify_emails', 'shop', 'json', '[]', false, 'Staff addresses alerted about new orders, low stock and payment issues'),
    ('shop.route_prefix', 'shop', 'localized_text', '{"fr": "boutique", "en": "shop", "ar": "boutique"}', true, 'URL segment of the shop per language'),
    ('shop.legal', 'shop', 'json', '{"seller_name": "", "address": "", "ice": "", "if": "", "rc": "", "patente": "", "cnss": ""}', false,
        'Seller identifiers printed on invoices (ICE, IF, RC, patente, CNSS): to fill and confirm with the accountant'),
    ('payments.stripe.enabled', 'payments', 'boolean', 'false', false, 'Offer card payment (Stripe Checkout) in the shop; needs the STRIPE_* environment variables'),
    ('payments.stripe.checkout_mode', 'payments', 'string', '"hosted"', false, 'hosted (redirect to Stripe) | embedded'),
    ('payments.stripe.statement_descriptor', 'payments', 'string', '"MOUZOUN"', false, 'Text on the card statement (max 22 characters)'),
    ('payments.cod.enabled', 'payments', 'boolean', 'false', false, 'Offer cash on delivery'),
    ('payments.cod.max_order_minor', 'payments', 'json', 'null', false, 'Largest order total payable cash on delivery, in cents (null = no limit)'),
    ('payments.pay_at_pickup.enabled', 'payments', 'boolean', 'true', false, 'Offer payment at the camp shop for pickup orders'),
    ('payments.bank_transfer.enabled', 'payments', 'boolean', 'false', false, 'Offer bank transfer'),
    ('payments.bank_transfer.details', 'payments', 'localized_text', '{"fr": "", "en": "", "ar": ""}', false, 'Bank details sent to the customer (IBAN, holder, reference to quote)'),
    ('payments.bank_transfer.hold_days', 'payments', 'number', '5', false, 'Days an unpaid bank-transfer order is kept before it is cancelled')
ON CONFLICT (key) DO NOTHING;

-- ================================================================================================
-- Email templates (fr / en / ar). body_text is generated from the HTML when empty.
-- Variables per template are listed below; the order lines use the order_summary.html partial.
-- ================================================================================================

CREATE TEMPORARY TABLE shop_tpl (code text, locale text, subject text, body_html text) ON COMMIT DROP;

INSERT INTO shop_tpl (code, locale, subject, body_html) VALUES
-- ---- order_confirmation ----
('order_confirmation', 'fr', $t$Merci pour votre commande {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Merci, {{ customer.first_name }} !</h1>
<p>Nous avons bien reçu votre commande <strong>{{ order.number }}</strong>.</p>
{% include "order_summary.html" %}
<p>{% if order.fulfillment_method == "pickup" %}Nous vous prévenons dès qu'elle est prête à retirer à la boutique du camp.{% elif order.fulfillment_method == "stay_delivery" %}Nous la livrons à votre tente pendant votre séjour.{% else %}Nous vous envoyons le numéro de suivi dès l'expédition.{% endif %}</p>
{{ button(order.url, "Suivre ma commande") }}
<p>L'équipe {{ site.name }}</p>$t$),
('order_confirmation', 'en', $t$Thank you for your order {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Thank you, {{ customer.first_name }}!</h1>
<p>We have received your order <strong>{{ order.number }}</strong>.</p>
{% include "order_summary.html" %}
<p>{% if order.fulfillment_method == "pickup" %}We will let you know as soon as it is ready to collect at the camp shop.{% elif order.fulfillment_method == "stay_delivery" %}We will deliver it to your tent during your stay.{% else %}We will send you the tracking number as soon as it ships.{% endif %}</p>
{{ button(order.url, "Track my order") }}
<p>The {{ site.name }} team</p>$t$),
('order_confirmation', 'ar', $t$شكرًا على طلبك {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">شكرًا يا {{ customer.first_name }}!</h1>
<p>لقد استلمنا طلبك <strong dir="ltr">{{ order.number }}</strong>.</p>
{% include "order_summary.html" %}
<p>{% if order.fulfillment_method == "pickup" %}سنخبرك فور جاهزيته للاستلام من متجر المخيم.{% elif order.fulfillment_method == "stay_delivery" %}سنوصله إلى خيمتك أثناء إقامتك.{% else %}سنرسل إليك رقم التتبع فور الشحن.{% endif %}</p>
{{ button(order.url, "تتبع طلبي") }}
<p>فريق {{ site.name }}</p>$t$),
-- ---- order_payment_pending (bank transfer) ----
('order_payment_pending', 'fr', $t$Commande {{ order.number }} : en attente de votre paiement$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Votre commande est réservée</h1>
{% if payment_url %}<p>Bonjour {{ customer.first_name }}, voici le lien pour régler votre commande <strong>{{ order.number }}</strong> ({{ money(order.total_minor, order.currency) }}) par carte, en toute sécurité.</p>
{{ button(payment_url, "Payer ma commande") }}{% else %}<p>Bonjour {{ customer.first_name }}, nous gardons vos produits jusqu'au {{ date(order.hold_until) }}. Merci d'effectuer le virement de <strong>{{ money(order.total_minor, order.currency) }}</strong> en indiquant la référence <strong>{{ order.number }}</strong>.</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{{ bank_transfer_details }}</p>{% endif %}
{% include "order_summary.html" %}
{{ button(order.url, "Voir ma commande") }}$t$),
('order_payment_pending', 'en', $t$Order {{ order.number }}: waiting for your payment$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Your order is reserved</h1>
{% if payment_url %}<p>Hello {{ customer.first_name }}, here is the secure link to pay your order <strong>{{ order.number }}</strong> ({{ money(order.total_minor, order.currency) }}) by card.</p>
{{ button(payment_url, "Pay my order") }}{% else %}<p>Hello {{ customer.first_name }}, we keep your products until {{ date(order.hold_until) }}. Please transfer <strong>{{ money(order.total_minor, order.currency) }}</strong> quoting the reference <strong>{{ order.number }}</strong>.</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{{ bank_transfer_details }}</p>{% endif %}
{% include "order_summary.html" %}
{{ button(order.url, "View my order") }}$t$),
('order_payment_pending', 'ar', $t$الطلب {{ order.number }}: في انتظار دفعك$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">طلبك محجوز</h1>
{% if payment_url %}<p>مرحبًا {{ customer.first_name }}، إليك الرابط الآمن لدفع طلبك <strong dir="ltr">{{ order.number }}</strong> ({{ money(order.total_minor, order.currency) }}) بالبطاقة.</p>
{{ button(payment_url, "ادفع طلبي") }}{% else %}<p>مرحبًا {{ customer.first_name }}، نحتفظ بمنتجاتك حتى {{ date(order.hold_until) }}. يُرجى تحويل <strong>{{ money(order.total_minor, order.currency) }}</strong> مع ذكر المرجع <strong dir="ltr">{{ order.number }}</strong>.</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{{ bank_transfer_details }}</p>{% endif %}
{% include "order_summary.html" %}
{{ button(order.url, "عرض طلبي") }}$t$),
-- ---- order_confirmed_cod ----
('order_confirmed_cod', 'fr', $t$Commande {{ order.number }} confirmée : paiement à la livraison$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Commande confirmée</h1>
<p>Bonjour {{ customer.first_name }}, votre commande <strong>{{ order.number }}</strong> est confirmée. Vous réglerez <strong>{{ money(order.total_minor, order.currency) }}</strong> en espèces au livreur.</p>
{% include "order_summary.html" %}
{{ button(order.url, "Suivre ma commande") }}$t$),
('order_confirmed_cod', 'en', $t$Order {{ order.number }} confirmed: cash on delivery$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Order confirmed</h1>
<p>Hello {{ customer.first_name }}, your order <strong>{{ order.number }}</strong> is confirmed. You will pay <strong>{{ money(order.total_minor, order.currency) }}</strong> in cash to the courier.</p>
{% include "order_summary.html" %}
{{ button(order.url, "Track my order") }}$t$),
('order_confirmed_cod', 'ar', $t$تأكيد الطلب {{ order.number }}: الدفع عند الاستلام$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">تم تأكيد الطلب</h1>
<p>مرحبًا {{ customer.first_name }}، تم تأكيد طلبك <strong dir="ltr">{{ order.number }}</strong>. ستدفع <strong>{{ money(order.total_minor, order.currency) }}</strong> نقدًا لموظف التوصيل.</p>
{% include "order_summary.html" %}
{{ button(order.url, "تتبع طلبي") }}$t$),
-- ---- order_shipped ----
('order_shipped', 'fr', $t$Votre commande {{ order.number }} est en route$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">C'est parti !</h1>
<p>Bonjour {{ customer.first_name }}, votre colis a été remis à {{ fulfillment.carrier or "notre transporteur" }}.{% if fulfillment.tracking_number %} Numéro de suivi : <strong dir="ltr">{{ fulfillment.tracking_number }}</strong>.{% endif %}</p>
{% if fulfillment.tracking_url %}{{ button(fulfillment.tracking_url, "Suivre le colis") }}{% else %}{{ button(order.url, "Voir ma commande") }}{% endif %}$t$),
('order_shipped', 'en', $t$Your order {{ order.number }} is on its way$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">On its way!</h1>
<p>Hello {{ customer.first_name }}, your parcel has been handed to {{ fulfillment.carrier or "our carrier" }}.{% if fulfillment.tracking_number %} Tracking number: <strong dir="ltr">{{ fulfillment.tracking_number }}</strong>.{% endif %}</p>
{% if fulfillment.tracking_url %}{{ button(fulfillment.tracking_url, "Track the parcel") }}{% else %}{{ button(order.url, "View my order") }}{% endif %}$t$),
('order_shipped', 'ar', $t$طلبك {{ order.number }} في الطريق$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">طلبك في الطريق!</h1>
<p>مرحبًا {{ customer.first_name }}، سُلّم طردك إلى {{ fulfillment.carrier or "شركة النقل" }}.{% if fulfillment.tracking_number %} رقم التتبع: <strong dir="ltr">{{ fulfillment.tracking_number }}</strong>.{% endif %}</p>
{% if fulfillment.tracking_url %}{{ button(fulfillment.tracking_url, "تتبع الطرد") }}{% else %}{{ button(order.url, "عرض طلبي") }}{% endif %}$t$),
-- ---- order_ready_for_pickup ----
('order_ready_for_pickup', 'fr', $t$Commande {{ order.number }} prête à retirer$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Votre commande vous attend</h1>
<p>Bonjour {{ customer.first_name }}, votre commande est prête à {{ fulfillment.location_name }}.</p>
{% if fulfillment.pickup_instructions %}<p>{{ fulfillment.pickup_instructions }}</p>{% endif %}
<p>Code de retrait à présenter : <strong style="font-size:20px;letter-spacing:4px;" dir="ltr">{{ fulfillment.pickup_code }}</strong></p>
{{ button(order.url, "Voir ma commande") }}$t$),
('order_ready_for_pickup', 'en', $t$Order {{ order.number }} is ready to collect$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Your order is waiting for you</h1>
<p>Hello {{ customer.first_name }}, your order is ready at {{ fulfillment.location_name }}.</p>
{% if fulfillment.pickup_instructions %}<p>{{ fulfillment.pickup_instructions }}</p>{% endif %}
<p>Pickup code to show: <strong style="font-size:20px;letter-spacing:4px;" dir="ltr">{{ fulfillment.pickup_code }}</strong></p>
{{ button(order.url, "View my order") }}$t$),
('order_ready_for_pickup', 'ar', $t$الطلب {{ order.number }} جاهز للاستلام$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">طلبك في انتظارك</h1>
<p>مرحبًا {{ customer.first_name }}، طلبك جاهز في {{ fulfillment.location_name }}.</p>
{% if fulfillment.pickup_instructions %}<p>{{ fulfillment.pickup_instructions }}</p>{% endif %}
<p>رمز الاستلام: <strong style="font-size:20px;letter-spacing:4px;" dir="ltr">{{ fulfillment.pickup_code }}</strong></p>
{{ button(order.url, "عرض طلبي") }}$t$),
-- ---- order_delivered ----
('order_delivered', 'fr', $t$Commande {{ order.number }} livrée$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Bonne dégustation !</h1>
<p>Bonjour {{ customer.first_name }}, votre commande {{ order.number }} a été livrée. Nous espérons que les produits de notre ferme vous plairont.</p>
{{ button(order.url, "Voir ma commande") }}$t$),
('order_delivered', 'en', $t$Order {{ order.number }} delivered$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Enjoy!</h1>
<p>Hello {{ customer.first_name }}, your order {{ order.number }} has been delivered. We hope you enjoy the products of our farm.</p>
{{ button(order.url, "View my order") }}$t$),
('order_delivered', 'ar', $t$تم تسليم الطلب {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">بالهناء والشفاء!</h1>
<p>مرحبًا {{ customer.first_name }}، تم تسليم طلبك <span dir="ltr">{{ order.number }}</span>. نتمنى أن تنال منتجات مزرعتنا إعجابك.</p>
{{ button(order.url, "عرض طلبي") }}$t$),
-- ---- order_cancelled ----
('order_cancelled', 'fr', $t$Commande {{ order.number }} annulée$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Commande annulée</h1>
<p>Bonjour {{ customer.first_name }}, votre commande {{ order.number }} a été annulée.{% if order.amount_paid_minor > 0 %} Le montant réglé vous est remboursé sur votre moyen de paiement d'origine.{% endif %}</p>
<p>Une question ? Répondez simplement à cet e-mail.</p>$t$),
('order_cancelled', 'en', $t$Order {{ order.number }} cancelled$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Order cancelled</h1>
<p>Hello {{ customer.first_name }}, your order {{ order.number }} has been cancelled.{% if order.amount_paid_minor > 0 %} The amount you paid is refunded to your original payment method.{% endif %}</p>
<p>Any questions? Just reply to this email.</p>$t$),
('order_cancelled', 'ar', $t$تم إلغاء الطلب {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">تم إلغاء الطلب</h1>
<p>مرحبًا {{ customer.first_name }}، تم إلغاء طلبك <span dir="ltr">{{ order.number }}</span>.{% if order.amount_paid_minor > 0 %} سيُعاد المبلغ المدفوع إلى وسيلة الدفع الأصلية.{% endif %}</p>
<p>لديك سؤال؟ ما عليك سوى الرد على هذه الرسالة.</p>$t$),
-- ---- refund_issued ----
('refund_issued', 'fr', $t$Remboursement de {{ money(refund.amount_minor, refund.currency) }} pour {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Remboursement effectué</h1>
<p>Bonjour {{ customer.first_name }}, nous vous avons remboursé <strong>{{ money(refund.amount_minor, refund.currency) }}</strong> pour la commande {{ order.number }}. Selon votre banque, il apparaît sous 5 à 10 jours.</p>
{{ button(order.url, "Voir ma commande") }}$t$),
('refund_issued', 'en', $t$Refund of {{ money(refund.amount_minor, refund.currency) }} for {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Refund issued</h1>
<p>Hello {{ customer.first_name }}, we have refunded <strong>{{ money(refund.amount_minor, refund.currency) }}</strong> for order {{ order.number }}. Depending on your bank it shows within 5 to 10 days.</p>
{{ button(order.url, "View my order") }}$t$),
('refund_issued', 'ar', $t$استرداد {{ money(refund.amount_minor, refund.currency) }} للطلب {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">تم الاسترداد</h1>
<p>مرحبًا {{ customer.first_name }}، أعدنا إليك <strong>{{ money(refund.amount_minor, refund.currency) }}</strong> عن الطلب <span dir="ltr">{{ order.number }}</span>. يظهر المبلغ خلال 5 إلى 10 أيام حسب بنكك.</p>
{{ button(order.url, "عرض طلبي") }}$t$),
-- ---- return_update ----
('return_update', 'fr', $t$Retour {{ order_return.number }} : {{ order_return.status_label }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Votre demande de retour</h1>
<p>Bonjour {{ customer.first_name }}, votre retour {{ order_return.number }} (commande {{ order.number }}) est maintenant : <strong>{{ order_return.status_label }}</strong>.</p>
{% if order_return.staff_note %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ order_return.staff_note }}</p>{% endif %}
{{ button(order.url, "Voir ma commande") }}$t$),
('return_update', 'en', $t$Return {{ order_return.number }}: {{ order_return.status_label }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Your return request</h1>
<p>Hello {{ customer.first_name }}, your return {{ order_return.number }} (order {{ order.number }}) is now: <strong>{{ order_return.status_label }}</strong>.</p>
{% if order_return.staff_note %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ order_return.staff_note }}</p>{% endif %}
{{ button(order.url, "View my order") }}$t$),
('return_update', 'ar', $t$الإرجاع {{ order_return.number }}: {{ order_return.status_label }}$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">طلب الإرجاع</h1>
<p>مرحبًا {{ customer.first_name }}، حالة الإرجاع <span dir="ltr">{{ order_return.number }}</span> (الطلب <span dir="ltr">{{ order.number }}</span>) الآن: <strong>{{ order_return.status_label }}</strong>.</p>
{% if order_return.staff_note %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ order_return.staff_note }}</p>{% endif %}
{{ button(order.url, "عرض طلبي") }}$t$),
-- ---- abandoned_cart ----
('abandoned_cart', 'fr', $t$Vous avez oublié quelque chose ?$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Votre panier vous attend</h1>
<p>Les produits de notre ferme sont toujours dans votre panier :</p>
<ul>{% for line in lines %}<li>{{ line.quantity }} × {{ line.name }}</li>{% endfor %}</ul>
{{ button(cart_url, "Reprendre mon panier") }}
<p style="font-size:12px;color:#6b6259;">Vous recevez cet e-mail une seule fois.</p>$t$),
('abandoned_cart', 'en', $t$Did you forget something?$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Your cart is waiting</h1>
<p>Our farm products are still in your cart:</p>
<ul>{% for line in lines %}<li>{{ line.quantity }} × {{ line.name }}</li>{% endfor %}</ul>
{{ button(cart_url, "Back to my cart") }}
<p style="font-size:12px;color:#6b6259;">You receive this email only once.</p>$t$),
('abandoned_cart', 'ar', $t$هل نسيت شيئًا؟$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">سلتك في انتظارك</h1>
<p>منتجات مزرعتنا ما زالت في سلتك:</p>
<ul>{% for line in lines %}<li>{{ line.quantity }} × {{ line.name }}</li>{% endfor %}</ul>
{{ button(cart_url, "العودة إلى سلتي") }}
<p style="font-size:12px;color:#6b6259;">تصلك هذه الرسالة مرة واحدة فقط.</p>$t$),
-- ---- back_in_stock ----
('back_in_stock', 'fr', $t${{ product.name }} est de retour$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">De retour en stock</h1>
<p>{{ product.name }}{% if product.variant %} ({{ product.variant }}){% endif %} est à nouveau disponible. Les quantités sont limitées.</p>
{{ button(product.url, "Voir le produit") }}$t$),
('back_in_stock', 'en', $t${{ product.name }} is back$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Back in stock</h1>
<p>{{ product.name }}{% if product.variant %} ({{ product.variant }}){% endif %} is available again. Quantities are limited.</p>
{{ button(product.url, "See the product") }}$t$),
('back_in_stock', 'ar', $t${{ product.name }} متوفر من جديد$t$, $t$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">عاد إلى المخزون</h1>
<p>{{ product.name }}{% if product.variant %} ({{ product.variant }}){% endif %} متوفر من جديد. الكميات محدودة.</p>
{{ button(product.url, "عرض المنتج") }}$t$),
-- ---- staff (default locale only is used; all three seeded for consistency) ----
('staff_new_order', 'fr', $t$Nouvelle commande {{ order.number }} ({{ money(order.total_minor, order.currency) }})$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">Nouvelle commande {{ order.number }}</h1>
<p>{{ customer.full_name }} · {{ order.email }}{% if order.phone %} · <span dir="ltr">{{ order.phone }}</span>{% endif %}</p>
<p>Paiement : {{ order.payment_method }} · Livraison : {{ order.fulfillment_method }}{% if order.requires_confirmation %} · <strong>à confirmer par téléphone</strong>{% endif %}</p>
{% include "order_summary.html" %}
{{ button(admin_url, "Ouvrir la commande") }}$t$),
('staff_new_order', 'en', $t$New order {{ order.number }} ({{ money(order.total_minor, order.currency) }})$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">New order {{ order.number }}</h1>
<p>{{ customer.full_name }} · {{ order.email }}{% if order.phone %} · <span dir="ltr">{{ order.phone }}</span>{% endif %}</p>
<p>Payment: {{ order.payment_method }} · Fulfilment: {{ order.fulfillment_method }}{% if order.requires_confirmation %} · <strong>confirm by phone</strong>{% endif %}</p>
{% include "order_summary.html" %}
{{ button(admin_url, "Open the order") }}$t$),
('staff_new_order', 'ar', $t$طلب جديد {{ order.number }} ({{ money(order.total_minor, order.currency) }})$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">طلب جديد <span dir="ltr">{{ order.number }}</span></h1>
<p>{{ customer.full_name }} · {{ order.email }}{% if order.phone %} · <span dir="ltr">{{ order.phone }}</span>{% endif %}</p>
<p>الدفع: {{ order.payment_method }} · التسليم: {{ order.fulfillment_method }}{% if order.requires_confirmation %} · <strong>يجب التأكيد هاتفيًا</strong>{% endif %}</p>
{% include "order_summary.html" %}
{{ button(admin_url, "فتح الطلب") }}$t$),
('staff_low_stock', 'fr', $t$Stock bas : {{ stock.sku }}$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">Stock bas</h1>
<p>{{ stock.name }} (<span dir="ltr">{{ stock.sku }}</span>) à {{ stock.location }} : <strong>{{ stock.available }}</strong> disponible(s), seuil {{ stock.threshold }}.</p>
{{ button(admin_url, "Ouvrir l'inventaire") }}$t$),
('staff_low_stock', 'en', $t$Low stock: {{ stock.sku }}$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">Low stock</h1>
<p>{{ stock.name }} (<span dir="ltr">{{ stock.sku }}</span>) at {{ stock.location }}: <strong>{{ stock.available }}</strong> available, threshold {{ stock.threshold }}.</p>
{{ button(admin_url, "Open the inventory") }}$t$),
('staff_low_stock', 'ar', $t$مخزون منخفض: {{ stock.sku }}$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">مخزون منخفض</h1>
<p>{{ stock.name }} (<span dir="ltr">{{ stock.sku }}</span>) في {{ stock.location }}: <strong>{{ stock.available }}</strong> متاح، الحد {{ stock.threshold }}.</p>
{{ button(admin_url, "فتح المخزون") }}$t$),
('staff_payment_issue', 'fr', $t$À vérifier : paiement de la commande {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">Paiement à vérifier</h1>
<p>Commande {{ order.number }} : {{ issue }}</p>
<p>Ne pas expédier avant vérification.</p>
{{ button(admin_url, "Ouvrir la commande") }}$t$),
('staff_payment_issue', 'en', $t$Check the payment of order {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">Payment to check</h1>
<p>Order {{ order.number }}: {{ issue }}</p>
<p>Do not ship before it is checked.</p>
{{ button(admin_url, "Open the order") }}$t$),
('staff_payment_issue', 'ar', $t$تحقق من دفع الطلب {{ order.number }}$t$, $t$<h1 style="margin:0 0 16px 0;font-size:20px;">دفع يحتاج إلى التحقق</h1>
<p>الطلب <span dir="ltr">{{ order.number }}</span>: {{ issue }}</p>
<p>لا تشحن قبل التحقق.</p>
{{ button(admin_url, "فتح الطلب") }}$t$);

-- "{% include ... ignore missing %}" above is a harmless leftover-proof no-op: remove it.
UPDATE shop_tpl SET body_html = replace(body_html, '', '');

INSERT INTO email_templates (code, variables)
SELECT DISTINCT s.code, v.variables
FROM shop_tpl s
JOIN (VALUES
    ('order_confirmation',     ARRAY['site', 'customer', 'order', 'lines']),
    ('order_payment_pending',  ARRAY['site', 'customer', 'order', 'lines', 'bank_transfer_details', 'payment_url']),
    ('order_confirmed_cod',    ARRAY['site', 'customer', 'order', 'lines']),
    ('order_shipped',          ARRAY['site', 'customer', 'order', 'fulfillment']),
    ('order_ready_for_pickup', ARRAY['site', 'customer', 'order', 'fulfillment']),
    ('order_delivered',        ARRAY['site', 'customer', 'order']),
    ('order_cancelled',        ARRAY['site', 'customer', 'order']),
    ('refund_issued',          ARRAY['site', 'customer', 'order', 'refund']),
    ('return_update',          ARRAY['site', 'customer', 'order', 'order_return']),
    ('abandoned_cart',         ARRAY['site', 'lines', 'cart_url']),
    ('back_in_stock',          ARRAY['site', 'product']),
    ('staff_new_order',        ARRAY['site', 'customer', 'order', 'lines', 'admin_url']),
    ('staff_low_stock',        ARRAY['site', 'stock', 'admin_url']),
    ('staff_payment_issue',    ARRAY['site', 'order', 'issue', 'admin_url'])
) AS v (code, variables) ON v.code = s.code
ON CONFLICT (code) DO NOTHING;

INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, s.locale, s.subject, s.body_html, ''
FROM shop_tpl s JOIN email_templates t ON t.code = s.code
ON CONFLICT DO NOTHING;
