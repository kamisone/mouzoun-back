-- 0001_foundation: extensions, the shared updated_at trigger function, locales, currencies, FX.

CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS btree_gist;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE EXTENSION IF NOT EXISTS unaccent;

-- Attached to every table with an updated_at column (see 0011_updated_at_triggers).
CREATE FUNCTION set_updated_at() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TABLE locales (
    code        text PRIMARY KEY CHECK (code ~ '^[a-z]{2,3}(-[A-Za-z0-9]{2,8})?$'),
    name        text NOT NULL,
    native_name text NOT NULL,
    is_rtl      boolean NOT NULL DEFAULT false,
    is_default  boolean NOT NULL DEFAULT false,
    is_active   boolean NOT NULL DEFAULT true,
    sort_order  integer NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX locales_single_default ON locales (is_default) WHERE is_default;

CREATE TABLE currencies (
    code        char(3) PRIMARY KEY CHECK (code ~ '^[A-Z]{3}$'),
    name        text NOT NULL,
    symbol      text NOT NULL,
    minor_units smallint NOT NULL DEFAULT 2 CHECK (minor_units BETWEEN 0 AND 4),
    is_default  boolean NOT NULL DEFAULT false,
    is_active   boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX currencies_single_default ON currencies (is_default) WHERE is_default;

CREATE TABLE exchange_rates (
    base_currency  char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    quote_currency char(3) NOT NULL REFERENCES currencies (code) ON UPDATE CASCADE,
    rate           numeric(18, 8) NOT NULL CHECK (rate > 0),
    valid_on       date NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (base_currency, quote_currency, valid_on),
    CHECK (base_currency <> quote_currency)
);
