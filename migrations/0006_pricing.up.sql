-- 0006_pricing: guest categories, cancellation policies, rate plans, seasons, prices, taxes,
-- promotions. All amounts are integers in minor units (cents).

CREATE TABLE guest_categories (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                   citext NOT NULL UNIQUE,
    min_age                smallint CHECK (min_age >= 0),
    max_age                smallint CHECK (max_age >= 0),
    counts_toward_capacity boolean NOT NULL DEFAULT true,
    sort_order             integer NOT NULL DEFAULT 0,
    is_active              boolean NOT NULL DEFAULT true,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CHECK (max_age IS NULL OR min_age IS NULL OR max_age >= min_age)
);

CREATE TABLE guest_category_translations (
    guest_category_id uuid NOT NULL REFERENCES guest_categories (id) ON DELETE CASCADE,
    locale            text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name              text NOT NULL,
    description       text,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (guest_category_id, locale)
);

CREATE TABLE cancellation_policies (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       citext NOT NULL UNIQUE,
    -- e.g. [{"min_hours_before":168,"refund_percent":100},{"min_hours_before":48,"refund_percent":50},
    --       {"min_hours_before":0,"refund_percent":0}]
    rules      jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(rules) = 'array'),
    is_default boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX cancellation_policies_single_default ON cancellation_policies (is_default) WHERE is_default;

CREATE TABLE cancellation_policy_translations (
    cancellation_policy_id uuid NOT NULL REFERENCES cancellation_policies (id) ON DELETE CASCADE,
    locale                 text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name                   text NOT NULL,
    description            text,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (cancellation_policy_id, locale)
);

CREATE TABLE rate_plans (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                   citext NOT NULL UNIQUE,
    cancellation_policy_id uuid REFERENCES cancellation_policies (id) ON DELETE SET NULL,
    meal_plan              text NOT NULL DEFAULT 'none' CHECK (meal_plan IN ('none', 'breakfast', 'half_board', 'full_board')),
    deposit_type           text NOT NULL DEFAULT 'none' CHECK (deposit_type IN ('none', 'percent', 'fixed', 'full')),
    -- One column per unit: never a percentage and an amount in the same column.
    deposit_percent        numeric(5, 2) CHECK (deposit_percent > 0 AND deposit_percent <= 100),
    deposit_amount_minor   bigint CHECK (deposit_amount_minor > 0),
    deposit_currency       char(3) REFERENCES currencies (code) ON UPDATE CASCADE,
    payment_mode           text NOT NULL DEFAULT 'pay_on_site' CHECK (payment_mode IN ('pay_on_site', 'optional_online', 'online_required')),
    is_public              boolean NOT NULL DEFAULT true,
    is_active              boolean NOT NULL DEFAULT true,
    sort_order             integer NOT NULL DEFAULT 0,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    CHECK ((deposit_type = 'percent') = (deposit_percent IS NOT NULL)),
    CHECK ((deposit_type = 'fixed') = (deposit_amount_minor IS NOT NULL)),
    CHECK ((deposit_amount_minor IS NULL) = (deposit_currency IS NULL))
);

CREATE TABLE rate_plan_translations (
    rate_plan_id uuid NOT NULL REFERENCES rate_plans (id) ON DELETE CASCADE,
    locale       text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name         text NOT NULL,
    description  text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (rate_plan_id, locale)
);

CREATE TABLE seasons (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code       citext NOT NULL UNIQUE,
    color      text CHECK (color ~ '^#[0-9A-Fa-f]{6}$'),
    priority   integer NOT NULL DEFAULT 0,      -- higher wins when periods of two seasons overlap
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE season_translations (
    season_id  uuid NOT NULL REFERENCES seasons (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (season_id, locale)
);

CREATE TABLE season_periods (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    season_id  uuid NOT NULL REFERENCES seasons (id) ON DELETE CASCADE,
    period     daterange NOT NULL CHECK (NOT isempty(period)),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    EXCLUDE USING gist (season_id WITH =, period WITH &&)
);
CREATE INDEX season_periods_period_idx ON season_periods USING gist (period);

CREATE TABLE prices (
    id                       uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    product_id               uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    rate_plan_id             uuid REFERENCES rate_plans (id) ON DELETE CASCADE,          -- NULL = all plans
    season_id                uuid REFERENCES seasons (id) ON DELETE CASCADE,             -- NULL = base price
    guest_category_id        uuid REFERENCES guest_categories (id) ON DELETE CASCADE,    -- NULL = any guest
    pricing_unit             text NOT NULL CHECK (pricing_unit IN ('per_unit_per_night', 'per_person_per_night',
                                 'per_person', 'per_unit', 'per_booking', 'per_hour', 'per_day')),
    amount_minor             bigint NOT NULL CHECK (amount_minor >= 0),
    currency                 char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    extra_guest_amount_minor bigint CHECK (extra_guest_amount_minor >= 0),
    min_quantity             integer NOT NULL DEFAULT 1 CHECK (min_quantity >= 1),
    max_quantity             integer,
    min_nights               integer CHECK (min_nights >= 1),
    days_of_week             smallint NOT NULL DEFAULT 127 CHECK (days_of_week BETWEEN 1 AND 127), -- Mon=1 … Sun=64
    valid_period             daterange,
    priority                 integer NOT NULL DEFAULT 0,
    is_active                boolean NOT NULL DEFAULT true,
    created_at               timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now(),
    CHECK (max_quantity IS NULL OR max_quantity >= min_quantity)
);
CREATE INDEX prices_product_idx ON prices (product_id) WHERE is_active;

CREATE TABLE taxes (
    id                        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                      citext NOT NULL UNIQUE,
    calc_type                 text NOT NULL CHECK (calc_type IN ('percent', 'fixed_per_person_per_night',
                                  'fixed_per_booking', 'fixed_per_unit')),
    rate_percent              numeric(6, 3) CHECK (rate_percent >= 0),
    amount_minor              bigint CHECK (amount_minor >= 0),
    currency                  char(3) REFERENCES currencies (code) ON UPDATE CASCADE,
    is_inclusive              boolean NOT NULL DEFAULT false,
    exempt_guest_category_ids uuid[] NOT NULL DEFAULT '{}',
    valid_period              daterange,
    is_active                 boolean NOT NULL DEFAULT true,
    created_at                timestamptz NOT NULL DEFAULT now(),
    updated_at                timestamptz NOT NULL DEFAULT now(),
    CHECK (
        (calc_type = 'percent' AND rate_percent IS NOT NULL)
        OR (calc_type <> 'percent' AND amount_minor IS NOT NULL AND currency IS NOT NULL)
    )
);

CREATE TABLE tax_translations (
    tax_id     uuid NOT NULL REFERENCES taxes (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name       text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tax_id, locale)
);

CREATE TABLE product_taxes (
    product_id uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    tax_id     uuid NOT NULL REFERENCES taxes (id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (product_id, tax_id)
);

CREATE TABLE promotions (
    id                      uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                    citext UNIQUE,           -- NULL = automatic offer
    discount_type           text NOT NULL CHECK (discount_type IN ('percent', 'fixed_amount', 'free_nights')),
    -- One column per unit, exactly the one matching discount_type is set.
    discount_percent        numeric(5, 2) CHECK (discount_percent > 0 AND discount_percent <= 100),
    discount_amount_minor   bigint CHECK (discount_amount_minor > 0),
    discount_free_nights    integer CHECK (discount_free_nights > 0),
    -- Currency of discount_amount_minor and min_subtotal_minor.
    currency                char(3) REFERENCES currencies (code) ON UPDATE CASCADE,
    min_subtotal_minor      bigint CHECK (min_subtotal_minor >= 0),
    min_nights              integer CHECK (min_nights >= 1),
    min_days_before_arrival integer CHECK (min_days_before_arrival >= 0),
    max_redemptions         integer CHECK (max_redemptions > 0),
    max_per_customer        integer CHECK (max_per_customer > 0),
    booking_window          tstzrange,
    stay_window             daterange,
    applies_to_all          boolean NOT NULL DEFAULT true,
    is_stackable            boolean NOT NULL DEFAULT false,
    is_active               boolean NOT NULL DEFAULT true,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    CHECK ((discount_type = 'percent') = (discount_percent IS NOT NULL)),
    CHECK ((discount_type = 'fixed_amount') = (discount_amount_minor IS NOT NULL)),
    CHECK ((discount_type = 'free_nights') = (discount_free_nights IS NOT NULL)),
    CHECK ((discount_amount_minor IS NULL AND min_subtotal_minor IS NULL) OR currency IS NOT NULL)
);

CREATE TABLE promotion_translations (
    promotion_id uuid NOT NULL REFERENCES promotions (id) ON DELETE CASCADE,
    locale       text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name         text NOT NULL,
    description  text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (promotion_id, locale)
);

CREATE TABLE promotion_products (
    promotion_id uuid NOT NULL REFERENCES promotions (id) ON DELETE CASCADE,
    product_id   uuid NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (promotion_id, product_id)
);
