# The shop operated by the French company

The online shop is sold by a French company (`legal_entities.code = mouzoun_fr`, setting
`shop.legal_entity`). Bookings stay with the Moroccan entity (`bookings.legal_entity =
mouzoun_ma`, to decide). Migration `0028_shop_france`. Everything below must be **validated by
a lawyer and an accountant before opening**: the code implements the mechanics, not the advice.

## Seller and entities

- `legal_entities`: legal name, form, capital, address, country, registration (`siren`,
  `siret`, `rcs_city`, `vat_number`, `ape_code` for FR; `ice`, `if`, `rc`, `patente`, `cnss`
  for MA), email, phone, invoice prefix (`F` for France), publication director, host;
  `legal_entity_translations.invoice_footer`. Edited in **Admin › Boutique › Conformité France**
  (with a specimen invoice PDF). Seeded identity is a placeholder ("à compléter").
- `orders`, `invoices`, `payments` carry `legal_entity_id` (backfill: shop → FR, bookings → MA;
  defaults `shop_entity_id()` / `booking_entity_id()`).
- Stock: `stock_locations.country_code` and `legal_entity_id`. The shop sells only the stock of
  its entity's locations (`Entrepôt France`); the camp store (MA, `mouzoun_ma`) is not used.

## Prices and VAT

- Prices are set in EUR incl. VAT. Tax classes: `food_reduced` (dates, oil, olives, spices),
  `standard` (crafts), `zero`. `tax_rates` per class × country × period (exclusion constraint:
  no overlapping active periods). FR 5.5 % / 20 % seeded **unconfirmed**; the other EU countries
  are inactive placeholders.
- Destination: same country → home rates; EU with `tax.oss_enabled` → the customer's country
  rates, without OSS → French VAT (under the €10,000 EU distance-sales threshold: the admin page
  warns at 80 % and when exceeded); outside the EU → `zero` (exports; zones must be enabled).
- `tax.keep_gross_price_constant` (default true): same price incl. VAT everywhere; false: the
  net price is constant and the gross follows the destination rate.
- Gift boxes: `shop_products.bundle_vat_split` `[{tax_class_code, share_percent}]` (sum 100).
- Shipping and COD fees follow the goods: their VAT is split by the goods' value per rate.
- `totals.vat_breakdown` / `orders.vat_breakdown` / `invoices.vat_breakdown`:
  `{rate, base_minor, vat_minor, gross_minor}` per rate, summing exactly to the totals.
- **Omnibus / L112-1-1**: `variant_price_history` (trigger on every price change) and
  `variant_reference_price()`: the lowest price of the 30 days before the current price. The
  API refuses a compare-at price above it (or without any earlier price); the product page shows
  the reference; the variant grid shows the 30-day low.

## Delivery

- France métropolitaine zone (postal patterns `01*`…`95*` without `20*`: Corsica and overseas
  excluded), Colissimo at home and Mondial Relay in relay points (`shipping_rates.delivery_type`),
  free over 60 €. EU zone inactive.
- `relay_point` fulfilment: `RelayPointProvider` trait, `FakeRelayPoints` (deterministic,
  no carrier contract yet), `GET /v1/shop/relay-points?postal_code=`; the chosen point is
  stored on the cart, the order and the fulfilment and decides the zone and VAT.
- Pickup at the camp and delivery to the tent are refused for a French seller.
- French addresses: 5-digit postal code required, phone stored in E.164.
- Customs fields (stock outside the EU, disabled): `product_variants.hs_code`,
  `product_variant_translations.customs_description`, `shipping_zones.requires_customs`,
  settings `customs.ioss_enabled` / `customs.ioss_number`.

## Payment

- Stripe only (bank transfer can be enabled). Cash on delivery and pay at pickup are locked for
  a French entity unless a super admin sets `payments.allow_cash_fr` (NF525 cash register).
- Stripe Checkout: no `payment_method_types` (dashboard methods, SCA by Stripe),
  `billing_address_collection=required`, `customer_email`, `statement_descriptor_suffix`, no
  Stripe Tax; each line name shows its VAT rate.
- Daily job: Stripe payouts and balance transactions → `payments.fee_minor`, `net_minor`,
  `payout_id`, `settled_at`.

## Consumer contract (checkout)

- Recap before the button (delivery, payment, total incl. VAT, seller, withdrawal notice).
- Terms checkbox, never pre-ticked; the published CGV revision (`legal.cgv_page_id`) is sent
  back (`terms_revision_id`, 409 `terms_changed` if it changed) and stored in
  `orders.terms_page_revision_id` with `terms_accepted_at` (CHECK on website orders).
- Separate unticked marketing checkbox → `orders.marketing_consent_snapshot`.
- Button: `shop.checkout_button_label` (fr "Commander avec obligation de paiement", required
  wording, checked on save).
- Confirmation email: seller identity, withdrawal rights and deadline rule, legal guarantee,
  mediator (partial `order_legal.html`), with **the accepted CGV as PDF** and **the model
  withdrawal form** attached.

## Right of withdrawal

- 14 days from delivery of the last parcel (`orders.withdrawal_deadline`, set on delivery).
- `shop_products.withdrawal_exemption`: `none`, `perishable` (refused), `personalised`
  (refused), `unsealed_hygiene` (accepted only if the customer states the seal is intact,
  `return_items.still_sealed`). Seed: food = `unsealed_hygiene`, fresh = `perishable`.
- Return reasons add `withdrawal` and `non_conformity` (legal guarantee, 2 years).
- A withdrawal sets `order_returns.refund_due_by` (+14 days, standard shipping included in the
  refund) and sends the `withdrawal_acknowledgment` email; staff see the due list (Conformité
  France, `GET /v1/admin/shop/withdrawals`).

## Invoices

Gap-free numbers per entity and year (`F2027-000001`), seller snapshot, VAT breakdown, delivery
date, format version, immutable once issued (trigger: corrections are credit notes pointing to
the invoice, VAT prorated), PDF rendered by the API, stored privately in the bucket
(`invoices/<entity>/<number>.pdf`) and sealed with its SHA-256. See [einvoicing.md](einvoicing.md).

## French language (Toubon) and food information (INCO)

- A product cannot be published without its French name and description; food, drinks and gift
  boxes also need French ingredients, storage instructions, the food business operator
  (`producer_name`, name and address) and, on every active variant, net quantity and country of
  origin.
- Legal pages cannot be published without the French title and French text in every section.
- Consumer email templates refuse an empty French subject/body.

## Compliance checklist

`GET /v1/admin/shop/compliance`; setting `shop.status = open` is refused (409
`compliance_checklist`) until all pass: seller identity complete, French VAT rates confirmed,
legal pages published, mediator set, button wording, card payment on, published products
complete in French, French shipping rates, a French warehouse, French emails complete.

## Accounting exports (CSV, `;`, UTF-8 BOM)

`/v1/admin/shop/exports/sales-journal.csv` (per day × entity × type × country × rate),
`documents.csv` (every invoice and credit note, with the PDF hash), `stripe.csv` (gross, fees,
net, payout).

## Points to validate before opening

1. Identity of the company (SIREN, SIRET, RCS, VAT number, capital, address, director, host).
2. VAT: 5.5 % for each food product (dates, oil, olives, spice blends), 20 % for crafts, the
   gift-box split, shipping VAT rule; OSS registration and threshold monitoring.
3. CGV, legal notice, privacy and cookie policies, withdrawal page and form (placeholders seeded).
4. Consumer mediator (mandatory) and the legal-guarantee wording.
5. Withdrawal exemptions per product (sealed food = hygiene exception?).
6. Food labelling: operator name/address (importer in France), origin, allergens, best-before.
7. Import of food from Morocco: customs, sanitary controls, who is importer of record.
8. Booking invoices entity (`bookings.legal_entity`) and whether stays sold to French customers
   change anything.
9. Invoice timezone: dates use the camp's timezone (Africa/Casablanca), one hour from Paris.
10. E-invoicing / e-reporting calendar (see einvoicing.md).
