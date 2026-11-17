-- 0028_shop_france (down). Restores the 0027 shape; data that only exists here (entities, price
-- history, tax rates, relay points) is dropped.

DELETE FROM email_template_translations WHERE email_template_id IN (SELECT id FROM email_templates WHERE code = 'withdrawal_acknowledgment');
DELETE FROM email_templates WHERE code = 'withdrawal_acknowledgment';
UPDATE email_template_translations t SET body_html = replace(t.body_html, E'\n{% include "order_legal.html" %}', '')
FROM email_templates e WHERE e.id = t.email_template_id AND e.code = 'order_confirmation';
UPDATE email_templates SET variables = array_remove(array_remove(array_remove(variables, 'seller'), 'withdrawal'), 'mediator')
WHERE code IN ('order_confirmation', 'return_update');

DELETE FROM settings WHERE key IN ('shop.legal_entity', 'bookings.legal_entity', 'shop.refund_deadline_days', 'shop.legal_guarantee_years',
    'shop.consumer_mediator', 'shop.checkout_button_label', 'shop.eu_countries', 'payments.allow_cash_fr', 'tax.oss_enabled',
    'tax.oss_threshold_minor', 'tax.keep_gross_price_constant', 'customs.ioss_enabled', 'customs.ioss_number', 'legal.cgv_page_id',
    'legal.notice_page_id', 'legal.privacy_page_id', 'legal.cookies_page_id', 'legal.withdrawal_page_id', 'legal.shipping_page_id',
    'privacy.data_retention');
UPDATE settings SET value = '7' WHERE key = 'shop.withdrawal_days';
UPDATE settings SET value = '["MA"]' WHERE key = 'shop.ships_to_countries';
UPDATE settings SET value = '["amana", "ctm", "dhl"]' WHERE key = 'shop.carriers';
UPDATE settings SET value = 'true' WHERE key IN ('payments.pay_at_pickup.enabled', 'shop.stay_delivery_enabled');
INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('shop.shipping_tax_id', 'shop', 'json', 'null', false, 'Tax applied to shipping (taxes.id), null = none'),
    ('shop.export_zero_rated', 'shop', 'boolean', 'false', false, 'Export zones (is_export) are charged 0 % TVA (to confirm with an accountant)'),
    ('shop.legal', 'shop', 'json', '{"seller_name": "", "address": "", "ice": "", "if": "", "rc": "", "patente": "", "cnss": ""}', false,
        'Seller identifiers printed on invoices (ICE, IF, RC, patente, CNSS): to fill and confirm with the accountant')
ON CONFLICT (key) DO NOTHING;

ALTER TABLE return_items DROP COLUMN still_sealed;
ALTER TABLE order_returns DROP COLUMN refund_due_by;
UPDATE order_returns SET reason = 'other' WHERE reason IN ('withdrawal', 'non_conformity');
ALTER TABLE order_returns DROP CONSTRAINT order_returns_reason_check;
ALTER TABLE order_returns ADD CONSTRAINT order_returns_reason_check CHECK (reason IN ('damaged', 'wrong_item', 'quality',
    'not_as_described', 'changed_mind', 'other'));

UPDATE carts SET fulfillment_method = 'shipping' WHERE fulfillment_method = 'relay_point';
UPDATE orders SET fulfillment_method = 'shipping' WHERE fulfillment_method = 'relay_point';
UPDATE fulfillments SET method = 'shipping' WHERE method = 'relay_point';
DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['carts', 'orders'] LOOP
        EXECUTE format('ALTER TABLE %I DROP CONSTRAINT %I', t, t || '_fulfillment_method_check');
        EXECUTE format($f$ALTER TABLE %I ADD CONSTRAINT %I CHECK (fulfillment_method IN ('shipping', 'pickup', 'stay_delivery'))$f$,
                       t, t || '_fulfillment_method_check');
    END LOOP;
END $$;
ALTER TABLE fulfillments DROP CONSTRAINT fulfillments_method_check;
ALTER TABLE fulfillments ADD CONSTRAINT fulfillments_method_check CHECK (method IN ('shipping', 'pickup', 'stay_delivery'));
ALTER TABLE fulfillments DROP COLUMN relay_point;
ALTER TABLE carts DROP COLUMN relay_point;
ALTER TABLE orders
    DROP CONSTRAINT orders_terms_accepted,
    DROP COLUMN terms_page_revision_id,
    DROP COLUMN withdrawal_deadline,
    DROP COLUMN marketing_consent_snapshot,
    DROP COLUMN vat_breakdown,
    DROP COLUMN relay_point;

ALTER TABLE shipping_rates DROP COLUMN delivery_type;
ALTER TABLE shipping_zones DROP COLUMN requires_customs;
ALTER TABLE stock_locations DROP COLUMN legal_entity_id, DROP COLUMN country_code;
DROP TABLE product_variant_translations;
ALTER TABLE product_variants DROP COLUMN country_of_origin, DROP COLUMN hs_code;

DROP FUNCTION variant_reference_price(uuid);
DROP TRIGGER product_variants_price_history ON product_variants;
DROP FUNCTION variant_price_history_track();
DROP TABLE variant_price_history;

ALTER TABLE shop_products ADD COLUMN is_returnable boolean NOT NULL DEFAULT true;
UPDATE shop_products SET is_returnable = withdrawal_exemption = 'none';
ALTER TABLE shop_products DROP COLUMN tax_class_code, DROP COLUMN bundle_vat_split, DROP COLUMN withdrawal_exemption;
DROP TABLE tax_rates;
DROP TABLE tax_class_translations;
DROP TABLE tax_classes;

DROP TRIGGER invoices_immutable ON invoices;
DROP FUNCTION invoices_immutable();
CREATE TABLE invoice_counters (
    year        smallint PRIMARY KEY CHECK (year BETWEEN 2000 AND 2999),
    last_number integer NOT NULL DEFAULT 0 CHECK (last_number >= 0),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER invoice_counters_set_updated_at BEFORE UPDATE ON invoice_counters FOR EACH ROW EXECUTE FUNCTION set_updated_at();
INSERT INTO invoice_counters (year, last_number)
SELECT year, max(last_number) FROM invoice_entity_counters GROUP BY year;
DROP TABLE invoice_entity_counters;

DROP INDEX invoices_entity_idx;
ALTER TABLE invoices
    DROP CONSTRAINT invoices_credit_note_link,
    DROP COLUMN legal_entity_id, DROP COLUMN seller_snapshot, DROP COLUMN vat_breakdown, DROP COLUMN credit_note_for_invoice_id,
    DROP COLUMN delivery_date, DROP COLUMN format_version, DROP COLUMN einvoice_status, DROP COLUMN pdf_sha256, DROP COLUMN pdf_object_key;
DROP INDEX payments_unsettled_idx;
ALTER TABLE payments ALTER COLUMN legal_entity_id DROP DEFAULT;
ALTER TABLE payments DROP COLUMN legal_entity_id, DROP COLUMN fee_minor, DROP COLUMN net_minor, DROP COLUMN payout_id, DROP COLUMN settled_at;
ALTER TABLE orders ALTER COLUMN legal_entity_id DROP DEFAULT;
ALTER TABLE orders DROP COLUMN legal_entity_id;

DROP FUNCTION shop_entity_id();
DROP FUNCTION booking_entity_id();
DROP TABLE legal_entity_translations;
DROP TABLE legal_entities;
