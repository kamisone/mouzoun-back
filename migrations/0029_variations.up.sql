-- 0029_variations: a shared library of variations (modelled on vitecamio's variant attributes).
--
-- variation_attributes ("Poids", "Conditionnement", "Couleur"...) own a vocabulary of
-- variation_values with a display label per locale, an optional colour swatch and an optional
-- price adjustment. A product links an attribute and picks some of its values: they are copied
-- into product_options / product_option_values (attribute_id / variation_value_id keep the link,
-- and library edits are propagated), so carts, orders and stock keep reading the same tables.
-- Options without a link stay product-specific, as before.

CREATE TABLE variation_attributes (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code         citext NOT NULL UNIQUE CHECK (code ~ '^[a-z][a-z0-9_]*$'),
    -- Back-office only: tells apart two attributes with the same public name.
    admin_label  text UNIQUE,
    display_type text NOT NULL DEFAULT 'button' CHECK (display_type IN ('button', 'swatch', 'dropdown')),
    -- Optional scope: offered first on products of this category.
    category_id  uuid REFERENCES categories (id) ON DELETE SET NULL,
    sort_order   integer NOT NULL DEFAULT 0,
    is_active    boolean NOT NULL DEFAULT true,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE variation_attribute_translations (
    attribute_id uuid NOT NULL REFERENCES variation_attributes (id) ON DELETE CASCADE,
    locale       text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name         text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (attribute_id, locale)
);

CREATE TABLE variation_values (
    id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    attribute_id           uuid NOT NULL REFERENCES variation_attributes (id) ON DELETE CASCADE,
    code                   citext NOT NULL CHECK (code ~ '^[a-z0-9][a-z0-9_]*$'),
    swatch_color           text CHECK (swatch_color ~ '^#[0-9a-fA-F]{6}$'),
    swatch_media_id        uuid REFERENCES media_assets (id) ON DELETE SET NULL,
    -- Added to the base price when variants are generated (negative = cheaper).
    price_adjustment_minor bigint,
    sort_order             integer NOT NULL DEFAULT 0,
    is_active              boolean NOT NULL DEFAULT true,
    created_at             timestamptz NOT NULL DEFAULT now(),
    updated_at             timestamptz NOT NULL DEFAULT now(),
    UNIQUE (attribute_id, code)
);
CREATE INDEX variation_values_attribute_idx ON variation_values (attribute_id, sort_order);

CREATE TABLE variation_value_translations (
    value_id   uuid NOT NULL REFERENCES variation_values (id) ON DELETE CASCADE,
    locale     text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    label      text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (value_id, locale)
);

-- Library deletions are refused by the API while products use them (RESTRICT is the safety net).
ALTER TABLE product_options
    ADD COLUMN attribute_id     uuid REFERENCES variation_attributes (id) ON DELETE RESTRICT,
    ADD COLUMN display_type     text NOT NULL DEFAULT 'button' CHECK (display_type IN ('button', 'swatch', 'dropdown')),
    ADD COLUMN default_value_id uuid;
ALTER TABLE product_options ADD UNIQUE (product_id, attribute_id);

ALTER TABLE product_option_values
    ADD COLUMN variation_value_id     uuid REFERENCES variation_values (id) ON DELETE RESTRICT,
    ADD COLUMN swatch_color           text CHECK (swatch_color ~ '^#[0-9a-fA-F]{6}$'),
    ADD COLUMN price_adjustment_minor bigint;
CREATE INDEX product_option_values_variation_idx ON product_option_values (variation_value_id) WHERE variation_value_id IS NOT NULL;

-- The default value belongs to the option (composite FK on product_option_values (id, option_id)).
ALTER TABLE product_options
    ADD CONSTRAINT product_options_default_value_fk FOREIGN KEY (default_value_id, id)
        REFERENCES product_option_values (id, option_id) ON DELETE SET NULL (default_value_id);

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['variation_attributes', 'variation_attribute_translations', 'variation_values',
        'variation_value_translations']
    LOOP
        EXECUTE format('CREATE TRIGGER %I BEFORE UPDATE ON %I FOR EACH ROW EXECUTE FUNCTION set_updated_at()',
                       t || '_set_updated_at', t);
    END LOOP;
END $$;

-- Starter library for the farm shop (labels editable in Admin › Boutique › Variations).
WITH a AS (
    INSERT INTO variation_attributes (code, display_type, sort_order) VALUES
        ('weight', 'button', 0), ('packaging', 'button', 1)
    RETURNING id, code
), at AS (
    INSERT INTO variation_attribute_translations (attribute_id, locale, name)
    SELECT a.id, v.locale, v.name FROM a JOIN (VALUES
        ('weight', 'fr', 'Poids'), ('weight', 'en', 'Weight'), ('weight', 'ar', 'الوزن'),
        ('packaging', 'fr', 'Conditionnement'), ('packaging', 'en', 'Packaging'), ('packaging', 'ar', 'التعبئة')
    ) AS v (code, locale, name) ON v.code = a.code::text
    WHERE EXISTS (SELECT 1 FROM locales l WHERE l.code = v.locale)
), vv AS (
    INSERT INTO variation_values (attribute_id, code, sort_order)
    SELECT a.id, v.code, v.sort FROM a JOIN (VALUES
        ('weight', '250g', 0), ('weight', '500g', 1), ('weight', '1kg', 2),
        ('packaging', 'box', 0), ('packaging', 'jar', 1), ('packaging', 'bag', 2)
    ) AS v (attr, code, sort) ON v.attr = a.code::text
    RETURNING id, attribute_id, code
)
INSERT INTO variation_value_translations (value_id, locale, label)
SELECT vv.id, v.locale, v.label FROM vv JOIN a ON a.id = vv.attribute_id JOIN (VALUES
    ('weight', '250g', 'fr', '250 g'), ('weight', '250g', 'en', '250 g'), ('weight', '250g', 'ar', '250 غ'),
    ('weight', '500g', 'fr', '500 g'), ('weight', '500g', 'en', '500 g'), ('weight', '500g', 'ar', '500 غ'),
    ('weight', '1kg', 'fr', '1 kg'), ('weight', '1kg', 'en', '1 kg'), ('weight', '1kg', 'ar', '1 كغ'),
    ('packaging', 'box', 'fr', 'Boîte'), ('packaging', 'box', 'en', 'Box'), ('packaging', 'box', 'ar', 'علبة'),
    ('packaging', 'jar', 'fr', 'Bocal'), ('packaging', 'jar', 'en', 'Jar'), ('packaging', 'jar', 'ar', 'برطمان'),
    ('packaging', 'bag', 'fr', 'Sachet'), ('packaging', 'bag', 'en', 'Bag'), ('packaging', 'bag', 'ar', 'كيس')
) AS v (attr, code, locale, label) ON v.attr = a.code::text AND v.code = vv.code::text
WHERE EXISTS (SELECT 1 FROM locales l WHERE l.code = v.locale);
