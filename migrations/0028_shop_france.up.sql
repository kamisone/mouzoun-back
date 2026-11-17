-- 0028_shop_france: the shop is operated by a French company (seller of record, Stripe FR,
-- French/EU VAT). Changes on top of 0027_shop (applied already, so not edited):
--   legal entities and per-entity gap-free invoice numbering, immutable invoices with VAT breakdown;
--   VAT by tax class × destination country × date; price history (30-day lowest price rule);
--   consumer contract (accepted CGV revision, withdrawal exemptions and reasons);
--   relay-point delivery; customs fields (non-EU stock, disabled by default); Stripe fees/payouts;
--   French defaults for settings. See docs/shop/france.md.

-- ================================================================================================
-- Legal entities
-- ================================================================================================
CREATE TABLE legal_entities (
    id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    code                citext NOT NULL UNIQUE,
    legal_name          text NOT NULL,
    legal_form          text NOT NULL,
    share_capital_minor bigint CHECK (share_capital_minor >= 0),
    currency            char(3) NOT NULL DEFAULT 'EUR' REFERENCES currencies (code) ON UPDATE CASCADE,
    address             jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(address) = 'object'),
    country_code        char(2) NOT NULL CHECK (country_code ~ '^[A-Z]{2}$'),
    -- FR: siren, siret, rcs_city, vat_number, ape_code — MA: ice, if, rc, patente, cnss
    registration        jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(registration) = 'object'),
    email               citext,
    phone               text,
    invoice_prefix      text NOT NULL DEFAULT '' CHECK (invoice_prefix ~ '^[A-Z0-9-]{0,8}$'),
    publication_director text,
    host                jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(host) = 'object'),
    is_active           boolean NOT NULL DEFAULT true,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER legal_entities_set_updated_at BEFORE UPDATE ON legal_entities FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE legal_entity_translations (
    legal_entity_id uuid NOT NULL REFERENCES legal_entities (id) ON DELETE CASCADE,
    locale          text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    invoice_footer  text NOT NULL DEFAULT '',
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (legal_entity_id, locale)
);
CREATE TRIGGER legal_entity_translations_set_updated_at BEFORE UPDATE ON legal_entity_translations FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- Placeholders: clearly fake identifiers, to fill from the back office before opening.
INSERT INTO legal_entities (code, legal_name, legal_form, share_capital_minor, address, country_code, registration, email, invoice_prefix, host) VALUES
    ('mouzoun_fr', 'MOUZOUN SAS (à compléter)', 'SAS', 100000,
     '{"line1": "À COMPLÉTER", "postal_code": "75000", "city": "Paris", "country_code": "FR"}', 'FR',
     '{"siren": "", "siret": "", "rcs_city": "", "vat_number": "", "ape_code": ""}', NULL, 'F',
     '{"name": "Google Cloud France SARL", "address": "8 rue de Londres, 75009 Paris, France"}'),
    ('mouzoun_ma', 'Mouzoun (Maroc, à compléter)', 'SARL', NULL,
     '{"line1": "M''ssici", "city": "Alnif", "country_code": "MA"}', 'MA',
     '{"ice": "", "if": "", "rc": "", "patente": "", "cnss": ""}', NULL, '', '{}');

-- The shop's entity (setting shop.legal_entity), for SQL predicates on stock locations.
CREATE FUNCTION shop_entity_id() RETURNS uuid
LANGUAGE sql STABLE AS $$
    SELECT id FROM legal_entities
    WHERE code = (SELECT value #>> '{}' FROM settings WHERE key = 'shop.legal_entity')::citext
$$;

-- Booking payments and invoices belong to the entity of the setting bookings.legal_entity.
CREATE FUNCTION booking_entity_id() RETURNS uuid
LANGUAGE sql STABLE AS $$
    SELECT id FROM legal_entities
    WHERE code = COALESCE((SELECT value #>> '{}' FROM settings WHERE key = 'bookings.legal_entity'), 'mouzoun_ma')::citext
$$;

-- ---- Per-entity, per-year gap-free invoice numbering (replaces invoice_counters) -----------------
CREATE TABLE invoice_entity_counters (
    legal_entity_id uuid NOT NULL REFERENCES legal_entities (id) ON DELETE RESTRICT,
    year            smallint NOT NULL CHECK (year BETWEEN 2000 AND 2999),
    last_number     integer NOT NULL DEFAULT 0 CHECK (last_number >= 0),
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (legal_entity_id, year)
);
CREATE TRIGGER invoice_entity_counters_set_updated_at BEFORE UPDATE ON invoice_entity_counters FOR EACH ROW EXECUTE FUNCTION set_updated_at();

-- ================================================================================================
-- Entities on sales, invoices, payments
-- ================================================================================================
ALTER TABLE orders ADD COLUMN legal_entity_id uuid REFERENCES legal_entities (id) ON DELETE RESTRICT;
UPDATE orders SET legal_entity_id = (SELECT id FROM legal_entities WHERE code = 'mouzoun_fr');
ALTER TABLE orders ALTER COLUMN legal_entity_id SET NOT NULL;
ALTER TABLE orders ALTER COLUMN legal_entity_id SET DEFAULT shop_entity_id();

ALTER TABLE payments ADD COLUMN legal_entity_id uuid REFERENCES legal_entities (id) ON DELETE RESTRICT;
UPDATE payments SET legal_entity_id = (SELECT id FROM legal_entities
                                       WHERE code = CASE WHEN order_id IS NOT NULL THEN 'mouzoun_fr' ELSE 'mouzoun_ma' END);
ALTER TABLE payments ALTER COLUMN legal_entity_id SET NOT NULL;
-- Shop payments always name their entity; booking payments default to the booking entity.
ALTER TABLE payments ALTER COLUMN legal_entity_id SET DEFAULT booking_entity_id();
ALTER TABLE payments
    ADD COLUMN fee_minor  bigint CHECK (fee_minor >= 0),
    ADD COLUMN net_minor  bigint,
    ADD COLUMN payout_id  text,
    ADD COLUMN settled_at timestamptz;
CREATE INDEX payments_unsettled_idx ON payments (created_at) WHERE provider = 'stripe' AND status IN ('succeeded', 'partially_refunded', 'refunded') AND payout_id IS NULL;

ALTER TABLE invoices
    ADD COLUMN legal_entity_id           uuid REFERENCES legal_entities (id) ON DELETE RESTRICT,
    ADD COLUMN seller_snapshot           jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(seller_snapshot) = 'object'),
    ADD COLUMN vat_breakdown             jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(vat_breakdown) = 'array'),
    ADD COLUMN credit_note_for_invoice_id uuid REFERENCES invoices (id) ON DELETE RESTRICT,
    ADD COLUMN delivery_date             date,
    ADD COLUMN format_version            integer NOT NULL DEFAULT 1,
    ADD COLUMN einvoice_status           text,
    ADD COLUMN pdf_sha256                text CHECK (pdf_sha256 ~ '^[0-9a-f]{64}$'),
    -- Private object in the bucket (invoices/<entity>/<number>.pdf), sealed with pdf_sha256.
    ADD COLUMN pdf_object_key            text;
UPDATE invoices SET legal_entity_id = (SELECT id FROM legal_entities
                                       WHERE code = CASE WHEN order_id IS NOT NULL THEN 'mouzoun_fr' ELSE 'mouzoun_ma' END);
ALTER TABLE invoices ALTER COLUMN legal_entity_id SET NOT NULL;
-- A credit note points to the invoice it corrects (required for orders by the API).
ALTER TABLE invoices ADD CONSTRAINT invoices_credit_note_link CHECK (credit_note_for_invoice_id IS NULL OR kind = 'credit_note');
CREATE INDEX invoices_entity_idx ON invoices (legal_entity_id, issued_at);

-- Counters from the former shared sequence (it numbered booking invoices: Moroccan entity).
INSERT INTO invoice_entity_counters (legal_entity_id, year, last_number)
SELECT (SELECT id FROM legal_entities WHERE code = 'mouzoun_ma'), year, last_number FROM invoice_counters;
DROP TABLE invoice_counters;

-- Issued invoices are immutable: corrections are a credit note plus a new invoice. Only the PDF
-- reference, its hash and the e-invoicing status can be filled in afterwards.
CREATE FUNCTION invoices_immutable() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'invoices are kept (accounting records): issue a credit note' USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF (NEW.number, NEW.kind, NEW.lines, NEW.subtotal_minor, NEW.tax_minor, NEW.total_minor, NEW.currency, NEW.billing_details,
        NEW.seller_snapshot, NEW.vat_breakdown, NEW.legal_entity_id, NEW.order_id, NEW.booking_id, NEW.issued_at,
        NEW.credit_note_for_invoice_id, NEW.delivery_date)
       IS DISTINCT FROM
       (OLD.number, OLD.kind, OLD.lines, OLD.subtotal_minor, OLD.tax_minor, OLD.total_minor, OLD.currency, OLD.billing_details,
        OLD.seller_snapshot, OLD.vat_breakdown, OLD.legal_entity_id, OLD.order_id, OLD.booking_id, OLD.issued_at,
        OLD.credit_note_for_invoice_id, OLD.delivery_date) THEN
        RAISE EXCEPTION 'an issued invoice cannot be changed: issue a credit note' USING ERRCODE = 'insufficient_privilege';
    END IF;
    IF OLD.pdf_sha256 IS NOT NULL AND (NEW.pdf_sha256, NEW.pdf_object_key) IS DISTINCT FROM (OLD.pdf_sha256, OLD.pdf_object_key) THEN
        RAISE EXCEPTION 'the invoice PDF is already sealed' USING ERRCODE = 'insufficient_privilege';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER invoices_immutable BEFORE UPDATE OR DELETE ON invoices FOR EACH ROW EXECUTE FUNCTION invoices_immutable();

-- ================================================================================================
-- VAT by destination
-- ================================================================================================
CREATE TABLE tax_classes (
    code       citext PRIMARY KEY CHECK (code ~ '^[a-z][a-z0-9_]*$'),
    sort_order integer NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER tax_classes_set_updated_at BEFORE UPDATE ON tax_classes FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE tax_class_translations (
    tax_class_code citext NOT NULL REFERENCES tax_classes (code) ON UPDATE CASCADE ON DELETE CASCADE,
    locale         text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    name           text NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tax_class_code, locale)
);
CREATE TRIGGER tax_class_translations_set_updated_at BEFORE UPDATE ON tax_class_translations FOR EACH ROW EXECUTE FUNCTION set_updated_at();

CREATE TABLE tax_rates (
    id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tax_class_code citext NOT NULL REFERENCES tax_classes (code) ON UPDATE CASCADE ON DELETE RESTRICT,
    country_code   char(2) NOT NULL CHECK (country_code ~ '^[A-Z]{2}$'),
    rate_percent   numeric(5, 2) NOT NULL CHECK (rate_percent BETWEEN 0 AND 100),
    valid_period   daterange NOT NULL CHECK (NOT isempty(valid_period)),
    is_active      boolean NOT NULL DEFAULT true,
    -- Seeded placeholders stay false until the accountant confirms them (compliance checklist).
    is_confirmed   boolean NOT NULL DEFAULT false,
    note           text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    EXCLUDE USING gist ((tax_class_code::text) WITH =, country_code WITH =, valid_period WITH &&) WHERE (is_active)
);
CREATE TRIGGER tax_rates_set_updated_at BEFORE UPDATE ON tax_rates FOR EACH ROW EXECUTE FUNCTION set_updated_at();

INSERT INTO tax_classes (code, sort_order) VALUES ('food_reduced', 1), ('standard', 2), ('zero', 3);
INSERT INTO tax_class_translations (tax_class_code, locale, name) VALUES
    ('food_reduced', 'fr', 'Alimentaire (taux réduit)'), ('food_reduced', 'en', 'Food (reduced rate)'), ('food_reduced', 'ar', 'مواد غذائية (معدل مخفض)'),
    ('standard', 'fr', 'Taux normal'), ('standard', 'en', 'Standard rate'), ('standard', 'ar', 'المعدل العادي'),
    ('zero', 'fr', 'Taux zéro'), ('zero', 'en', 'Zero rate'), ('zero', 'ar', 'معدل صفر');

-- France (active, to confirm by the accountant), other EU countries inactive placeholders.
INSERT INTO tax_rates (tax_class_code, country_code, rate_percent, valid_period, is_active, is_confirmed, note) VALUES
    ('food_reduced', 'FR', 5.5, '[2014-01-01,)', true, false, 'À confirmer par l''expert-comptable'),
    ('standard', 'FR', 20, '[2014-01-01,)', true, false, 'À confirmer par l''expert-comptable'),
    ('zero', 'FR', 0, '[2014-01-01,)', true, false, 'Exports hors UE');
INSERT INTO tax_rates (tax_class_code, country_code, rate_percent, valid_period, is_active, is_confirmed, note)
SELECT c, cc, r, '[2024-01-01,)', false, false, 'Placeholder, to confirm (OSS)'
FROM (VALUES
    ('DE', 7, 19), ('BE', 6, 21), ('ES', 10, 21), ('IT', 10, 22), ('NL', 9, 21), ('PT', 6, 23), ('AT', 10, 20),
    ('IE', 0, 23), ('LU', 3, 17), ('DK', 25, 25), ('SE', 12, 25), ('FI', 14, 25.5), ('PL', 5, 23), ('CZ', 12, 21),
    ('GR', 13, 24), ('HU', 18, 27), ('RO', 11, 21), ('SK', 19, 23), ('SI', 9.5, 22), ('HR', 5, 25), ('BG', 20, 20),
    ('EE', 9, 24), ('LV', 12, 21), ('LT', 9, 21), ('CY', 5, 19), ('MT', 0, 18)
) AS v (cc, food, std)
CROSS JOIN LATERAL (VALUES ('food_reduced', v.food::numeric), ('standard', v.std::numeric)) AS x (c, r);

ALTER TABLE shop_products
    ADD COLUMN tax_class_code   citext NOT NULL DEFAULT 'food_reduced' REFERENCES tax_classes (code) ON UPDATE CASCADE ON DELETE RESTRICT,
    -- Mixed-rate bundles: [{tax_class_code, share_percent}] summing to 100 (checked by the API).
    ADD COLUMN bundle_vat_split jsonb CHECK (bundle_vat_split IS NULL OR jsonb_typeof(bundle_vat_split) = 'array'),
    ADD COLUMN withdrawal_exemption text NOT NULL DEFAULT 'none'
        CHECK (withdrawal_exemption IN ('none', 'perishable', 'unsealed_hygiene', 'personalised'));
UPDATE shop_products SET tax_class_code = CASE WHEN product_type IN ('food', 'drink', 'gift_box') THEN 'food_reduced' ELSE 'standard' END,
                         withdrawal_exemption = CASE WHEN is_perishable THEN 'perishable'
                                                     WHEN product_type IN ('food', 'drink', 'cosmetic', 'gift_box') THEN 'unsealed_hygiene'
                                                     WHEN NOT is_returnable THEN 'personalised' ELSE 'none' END;
ALTER TABLE shop_products DROP COLUMN is_returnable;

-- ================================================================================================
-- Price history (lowest price of the last 30 days before a reduction)
-- ================================================================================================
CREATE TABLE variant_price_history (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    variant_id  uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    price_minor bigint NOT NULL CHECK (price_minor >= 0),
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz,
    changed_by  uuid REFERENCES users (id) ON DELETE SET NULL,
    CHECK (valid_to IS NULL OR valid_to >= valid_from)
);
CREATE INDEX variant_price_history_idx ON variant_price_history (variant_id, valid_from DESC);
CREATE UNIQUE INDEX variant_price_history_open_uniq ON variant_price_history (variant_id) WHERE valid_to IS NULL;

-- Every price change closes the open interval and opens a new one. The acting user comes from
-- the transaction setting `mouzoun.user_id` when the API sets it.
CREATE FUNCTION variant_price_history_track() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE actor uuid := NULLIF(current_setting('mouzoun.user_id', true), '')::uuid;
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.price_minor = OLD.price_minor THEN
        RETURN NEW;
    END IF;
    UPDATE variant_price_history SET valid_to = now() WHERE variant_id = NEW.id AND valid_to IS NULL;
    INSERT INTO variant_price_history (variant_id, price_minor, valid_from, changed_by) VALUES (NEW.id, NEW.price_minor, now(), actor);
    RETURN NEW;
END;
$$;
CREATE TRIGGER product_variants_price_history AFTER INSERT OR UPDATE OF price_minor ON product_variants
    FOR EACH ROW EXECUTE FUNCTION variant_price_history_track();
INSERT INTO variant_price_history (variant_id, price_minor, valid_from)
SELECT id, price_minor, created_at FROM product_variants;

-- Reference price of a reduction: the lowest price applied during the 30 days before the current
-- price took effect (French Consumer Code L112-1-1 / Omnibus). NULL without earlier price.
CREATE FUNCTION variant_reference_price(variant uuid) RETURNS bigint
LANGUAGE sql STABLE AS $$
    SELECT min(h.price_minor)
    FROM variant_price_history h,
         (SELECT valid_from FROM variant_price_history WHERE variant_id = variant AND valid_to IS NULL) cur
    WHERE h.variant_id = variant AND h.valid_from < cur.valid_from
      AND COALESCE(h.valid_to, 'infinity') > cur.valid_from - interval '30 days'
$$;

-- ================================================================================================
-- Variants: food information, origin, customs
-- ================================================================================================
ALTER TABLE product_variants
    ADD COLUMN country_of_origin char(2) CHECK (country_of_origin ~ '^[A-Z]{2}$'),
    ADD COLUMN hs_code           text CHECK (hs_code ~ '^[0-9]{6,10}$');
UPDATE product_variants SET country_of_origin = 'MA';

CREATE TABLE product_variant_translations (
    variant_id          uuid NOT NULL REFERENCES product_variants (id) ON DELETE CASCADE,
    locale              text NOT NULL REFERENCES locales (code) ON UPDATE CASCADE,
    customs_description text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    updated_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (variant_id, locale)
);
CREATE TRIGGER product_variant_translations_set_updated_at BEFORE UPDATE ON product_variant_translations FOR EACH ROW EXECUTE FUNCTION set_updated_at();

ALTER TABLE stock_locations
    ADD COLUMN country_code    char(2) NOT NULL DEFAULT 'MA' CHECK (country_code ~ '^[A-Z]{2}$'),
    ADD COLUMN legal_entity_id uuid REFERENCES legal_entities (id) ON DELETE RESTRICT;
ALTER TABLE stock_locations ALTER COLUMN country_code DROP DEFAULT;
-- Existing locations are the Moroccan camp: owned by the Moroccan entity, not used by the French shop.
UPDATE stock_locations SET legal_entity_id = (SELECT id FROM legal_entities WHERE code = 'mouzoun_ma');

ALTER TABLE shipping_zones ADD COLUMN requires_customs boolean NOT NULL DEFAULT false;
ALTER TABLE shipping_rates ADD COLUMN delivery_type text NOT NULL DEFAULT 'home' CHECK (delivery_type IN ('home', 'relay_point'));

-- ================================================================================================
-- Consumer contract, relay points, returns
-- ================================================================================================
ALTER TABLE orders
    ADD COLUMN terms_page_revision_id     uuid REFERENCES page_revisions (id) ON DELETE RESTRICT,
    ADD COLUMN withdrawal_deadline        date,
    ADD COLUMN marketing_consent_snapshot boolean NOT NULL DEFAULT false,
    -- Per VAT rate {rate, base_minor, vat_minor, gross_minor} at checkout (goods, shipping, fees).
    ADD COLUMN vat_breakdown              jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(vat_breakdown) = 'array'),
    ADD COLUMN relay_point                jsonb CHECK (relay_point IS NULL OR jsonb_typeof(relay_point) = 'object'),
    -- Deviation: website orders must carry the accepted CGV revision; staff draft/phone orders may not.
    ADD CONSTRAINT orders_terms_accepted CHECK (channel <> 'website' OR status = 'draft'
        OR (terms_page_revision_id IS NOT NULL AND terms_accepted_at IS NOT NULL)) NOT VALID;
ALTER TABLE carts ADD COLUMN relay_point jsonb CHECK (relay_point IS NULL OR jsonb_typeof(relay_point) = 'object');
ALTER TABLE fulfillments ADD COLUMN relay_point jsonb CHECK (relay_point IS NULL OR jsonb_typeof(relay_point) = 'object');

DO $$
DECLARE t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['carts', 'orders'] LOOP
        EXECUTE format('ALTER TABLE %I DROP CONSTRAINT %I', t, t || '_fulfillment_method_check');
        EXECUTE format($f$ALTER TABLE %I ADD CONSTRAINT %I CHECK (fulfillment_method IN ('shipping', 'pickup', 'stay_delivery', 'relay_point'))$f$,
                       t, t || '_fulfillment_method_check');
    END LOOP;
END $$;
ALTER TABLE fulfillments DROP CONSTRAINT fulfillments_method_check;
ALTER TABLE fulfillments ADD CONSTRAINT fulfillments_method_check CHECK (method IN ('shipping', 'pickup', 'stay_delivery', 'relay_point'));

ALTER TABLE order_returns DROP CONSTRAINT order_returns_reason_check;
ALTER TABLE order_returns ADD CONSTRAINT order_returns_reason_check CHECK (reason IN ('damaged', 'wrong_item', 'quality',
    'not_as_described', 'changed_mind', 'other', 'withdrawal', 'non_conformity'));
ALTER TABLE order_returns ADD COLUMN refund_due_by date;
ALTER TABLE return_items ADD COLUMN still_sealed boolean;

-- ================================================================================================
-- Settings
-- ================================================================================================
DELETE FROM settings WHERE key IN ('shop.legal', 'shop.shipping_tax_id', 'shop.export_zero_rated');
UPDATE settings SET value = '14' WHERE key = 'shop.withdrawal_days';
UPDATE settings SET value = '["FR"]' WHERE key = 'shop.ships_to_countries';
UPDATE settings SET value = '["colissimo", "mondial_relay", "chronopost", "dpd"]' WHERE key = 'shop.carriers';
UPDATE settings SET value = 'false' WHERE key IN ('payments.cod.enabled', 'payments.pay_at_pickup.enabled',
    'payments.bank_transfer.enabled', 'shop.stay_delivery_enabled');
UPDATE settings SET value = '{"fr": "boutique", "en": "shop", "ar": "boutique"}' WHERE key = 'shop.route_prefix';

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('shop.legal_entity', 'shop', 'string', '"mouzoun_fr"', false, 'Legal entity selling in the shop (legal_entities.code)'),
    ('bookings.legal_entity', 'booking', 'string', '"mouzoun_ma"', false, 'Legal entity issuing booking invoices (to decide)'),
    ('shop.refund_deadline_days', 'shop', 'number', '14', true, 'Days to refund after a withdrawal request (Consumer Code)'),
    ('shop.legal_guarantee_years', 'shop', 'number', '2', true, 'Legal guarantee of conformity, in years'),
    ('shop.consumer_mediator', 'shop', 'json', '{"name": "", "url": ""}', true, 'Consumer mediator (mandatory information: CGV and footer)'),
    ('shop.checkout_button_label', 'shop', 'localized_text',
     '{"fr": "Commander avec obligation de paiement", "en": "Order with obligation to pay", "ar": "اطلب مع الالتزام بالدفع"}', true,
     'Final checkout button: must state the payment obligation (French text required)'),
    ('shop.eu_countries', 'shop', 'json',
     '["AT","BE","BG","CY","CZ","DE","DK","EE","ES","FI","FR","GR","HR","HU","IE","IT","LT","LU","LV","MT","NL","PL","PT","RO","SE","SI","SK"]', false,
     'EU member states (VAT: OSS, customs)'),
    ('payments.allow_cash_fr', 'payments', 'boolean', 'false', false,
     'Allow cash on delivery / at pickup for a French entity (cash-register rules NF525: super admin only)'),
    ('tax.oss_enabled', 'tax', 'boolean', 'false', false, 'OSS registered: EU destinations charged at their own rate'),
    ('tax.oss_threshold_minor', 'tax', 'number', '1000000', false, 'EU distance-sales threshold (EUR cents per calendar year)'),
    ('tax.keep_gross_price_constant', 'tax', 'boolean', 'true', false, 'Same price incl. VAT in every EU country'),
    ('customs.ioss_enabled', 'customs', 'boolean', 'false', false, 'Import One-Stop Shop (parcels ≤ 150 € shipped from outside the EU)'),
    ('customs.ioss_number', 'customs', 'string', '""', false, 'IOSS number'),
    ('legal.cgv_page_id', 'legal', 'json', 'null', true, 'Page of the terms of sale (CGV)'),
    ('legal.notice_page_id', 'legal', 'json', 'null', true, 'Legal notice page (mentions légales)'),
    ('legal.privacy_page_id', 'legal', 'json', 'null', true, 'Privacy policy page'),
    ('legal.cookies_page_id', 'legal', 'json', 'null', true, 'Cookie policy page'),
    ('legal.withdrawal_page_id', 'legal', 'json', 'null', true, 'Right of withdrawal page with the model form'),
    ('legal.shipping_page_id', 'legal', 'json', 'null', true, 'Delivery and returns page'),
    ('privacy.data_retention', 'privacy', 'json',
     '{"prospects_years": 3, "customers_marketing_years": 3, "accounting_years": 10, "abandoned_carts_days": 30, "consent_months": 6}', false,
     'Retention periods (CNIL): applied by the retention job')
ON CONFLICT (key) DO NOTHING;

-- ================================================================================================
-- Email templates: seller identity, withdrawal rights and mediator on the confirmation;
-- withdrawal acknowledgment.
-- ================================================================================================
UPDATE email_templates SET variables = variables || ARRAY['seller', 'withdrawal', 'mediator']
WHERE code = 'order_confirmation';
UPDATE email_template_translations t SET body_html = t.body_html || E'\n{% include "order_legal.html" %}'
FROM email_templates e WHERE e.id = t.email_template_id AND e.code = 'order_confirmation';
UPDATE email_templates SET variables = variables || ARRAY['withdrawal'] WHERE code = 'return_update';

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('withdrawal_acknowledgment', ARRAY['site', 'customer', 'order', 'order_return', 'seller', 'withdrawal'])
    RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Accusé de réception de votre rétractation {{ order_return.number }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Votre rétractation est enregistrée</h1>
<p>Bonjour {{ customer.first_name }}, nous accusons réception de votre demande de rétractation {{ order_return.number }} pour la commande {{ order.number }}, reçue le {{ date(order_return.requested_at) }}.</p>
<p>Renvoyez les produits sous 14 jours à l'adresse indiquée ci-dessous. Nous vous remboursons au plus tard le {{ date(withdrawal.refund_due_by) }}, frais de livraison standard inclus ; le remboursement peut être différé jusqu'à réception des produits.</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ seller.legal_name }}<br>{{ seller.address }}</p>
{{ button(order.url, "Voir ma commande") }}$tpl$),
    ('en', $tpl$We received your withdrawal {{ order_return.number }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">Your withdrawal is registered</h1>
<p>Hello {{ customer.first_name }}, we acknowledge your withdrawal {{ order_return.number }} for order {{ order.number }}, received on {{ date(order_return.requested_at) }}.</p>
<p>Send the products back within 14 days to the address below. We refund you by {{ date(withdrawal.refund_due_by) }} at the latest, standard delivery costs included; the refund may wait until we receive the products.</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ seller.legal_name }}<br>{{ seller.address }}</p>
{{ button(order.url, "View my order") }}$tpl$),
    ('ar', $tpl$استلمنا طلب التراجع {{ order_return.number }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;color:#1d1a17;">تم تسجيل طلب التراجع</h1>
<p>مرحبًا {{ customer.first_name }}، نؤكد استلام طلب التراجع <span dir="ltr">{{ order_return.number }}</span> عن الطلب <span dir="ltr">{{ order.number }}</span> بتاريخ {{ date(order_return.requested_at) }}.</p>
<p>أعد المنتجات خلال 14 يومًا إلى العنوان أدناه. نعيد لك المبلغ في أجل أقصاه {{ date(withdrawal.refund_due_by) }}، بما في ذلك مصاريف التوصيل العادي؛ وقد يُؤجل الاسترداد حتى استلام المنتجات.</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ seller.legal_name }}<br>{{ seller.address }}</p>
{{ button(order.url, "عرض طلبي") }}$tpl$)
) AS v (locale, subject, body_html);
