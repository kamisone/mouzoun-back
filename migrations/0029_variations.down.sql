-- 0029_variations (down). Product options keep their copied names and values; only the links,
-- display types, swatch colours and price adjustments are dropped.

ALTER TABLE product_options DROP CONSTRAINT product_options_default_value_fk;
DROP INDEX product_option_values_variation_idx;
ALTER TABLE product_option_values
    DROP COLUMN variation_value_id,
    DROP COLUMN swatch_color,
    DROP COLUMN price_adjustment_minor;
ALTER TABLE product_options DROP CONSTRAINT product_options_product_id_attribute_id_key;
ALTER TABLE product_options
    DROP COLUMN attribute_id,
    DROP COLUMN display_type,
    DROP COLUMN default_value_id;

DROP TABLE variation_value_translations;
DROP TABLE variation_values;
DROP TABLE variation_attribute_translations;
DROP TABLE variation_attributes;
