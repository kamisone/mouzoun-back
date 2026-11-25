# Farm shop

Mouzoun sells the products of its farm (dates, olive oil, olives, spices, crafts, gift boxes)
online and at the camp. The model follows mature commerce platforms (Shopify, Medusa, Saleor):
**products → variants → stock per location**, **cart → checkout → order**, with payments,
fulfilments, returns and refunds as their own records, and every money and stock change traceable.

- Migration: `migrations/0027_shop.{up,down}.sql` (the spec called it 0014, already taken).
- Rust: `src/services/shop/*` (rules), `src/modules/shop/*` (HTTP), `src/db/shop.rs` (enums).
- Nuxt: `front/app/pages/shop/*` (public), `front/app/pages/admin/shop/*` (back office).
- Stripe: [stripe.md](stripe.md) (account shared with another site: tags and webhook filter).
- Legal launch (legal entity edited in the admin, legal pages in the builder, placeholder rates, owner to-do): [legal-launch.md](legal-launch.md).

## Catalogue: product → variant → stock

| record | what it is |
|---|---|
| `products` (`kind = 'shop_product'`) | the product page: translations, slugs, SEO, categories (`scope = 'shop'`), cover, gallery, reviews, related products, page-builder carousels. Always `booking_mode = 'not_bookable'` (enforced by the API) |
| `shop_products` (1:1) | food and shop details: type, shipping profile, origin field and crop, perishable / returnable, lot tracking, allergens (14 EU codes), nutrition, season, max per order; `shop_product_translations` for ingredients, storage, producer… |
| `variation_attributes` / `variation_values` | the variation library (Admin › Boutique › Variations, as vitecamio's variant attributes): reusable options (weight, packaging, colour) with translated names and labels, display type (buttons, swatches, dropdown), swatch colour or picture, price adjustment, active flag, optional category. Seeded: Poids (250 g / 500 g / 1 kg), Conditionnement (boîte / bocal / sachet) |
| `product_options` / `product_option_values` | up to 3 options per product, translated: linked to the library (`attribute_id`, `variation_value_id`; names, labels, colours and adjustments copied from it and refreshed on every library save) or product-specific. Display type, default value, per-product swatch picture |
| `product_variants` | what is sold, priced, stocked and shipped: SKU, EAN, price (EUR cents, VAT included), compare-at (> price), cost (admin only), net quantity + unit (unit price "€24.00 / kg"), shipping weight, flags. Exactly one default per product. Archived, never deleted while orders reference them |
| `inventory_levels` | per (variant, location): `on_hand`, `reserved`, `backorder_limit`. **`CHECK (reserved <= on_hand + backorder_limit)`**: the database itself refuses to oversell |
| `inventory_lots` | harvest batches (lot code, harvest, best-before) for `track_lots` products. Their quantities always sum to `on_hand`; stock leaves **FEFO** |
| `inventory_movements` | the ledger (append-only, a trigger refuses UPDATE / DELETE) |
| `inventory_reservations` | stock held for unpaid orders, with an expiry |

Variants are generated from the option matrix (admin "Generate variants"): existing
combinations are kept, archived ones come back, combinations no longer in the matrix are archived.
Two active variants never share a combination, and an active variant has a value for every option.
A new variant costs the base price + the price adjustments of its values; "recompute prices"
also resets the existing active ones (the Omnibus check still applies). The default variant is
the first combination with the options' default values. Library codes never change; a library
variation or value used by a product cannot be deleted (409 `variation_in_use`), only deactivated
(no longer offered to new products). Not taken from vitecamio: computed prices at read time
(prices stay stored per variant, for the price history), per-variant slugs, quantity tiers.

## Categories

Managed in **Admin › Boutique › Catégories** (`/admin/shop/categories`, form at `/admin/shop/categories/{id}`;
migration `0031_shop_categories`; API `modules/shop/admin/categories.rs`, rules `services/shop/categories.rs`).
Viewing needs `shop.read`, every change `shop.catalog`. Shipping, promotions, the variation library and the
product editor only *pick* categories (`MzCategoryPicker`); nothing edits them elsewhere. The old
`/admin/shop/shipping?tab=categories` link redirects to the new page.

- **Tree rules**: no cycle, parent in the same scope, at most `shop.category_max_depth` levels (default 3).
  Checked by the API before writing and by the deferred constraint trigger `categories_tree_check` at commit, so
  `PUT /admin/shop/categories/tree` (one call per drag-and-drop or keyboard move, rows `{id, parent_id,
  sort_order}`) is validated on the final tree. The call carries the tree `version` (an md5 of ids, parents and
  orders) and gets **409 `tree_changed`** if someone else changed the tree since it was read.
- **Visibility**: `is_active = false` hides the category *and its subtree* from the shop (sidebar, facets, the
  `/shop/categories` tree, listings through it); its page answers `{redirect}` → **301** to the nearest visible
  parent, or the shop home. `show_in_menu = false` only removes it from the sidebar and menus: its page works
  (seasonal or campaign categories). SQL helper: `shop_visible_categories()`.
- **Slugs and redirects**: slugs are generated from the name (same rule as product slugs, `utils/slug.ts` on the
  front), deduplicated with `-2`, `-3`, and locked in the form once the category was active once
  (`first_activated_at`). Changing a slug (or deleting the category) inserts a 301 in `redirects` from the old
  path to the new one per language (`/boutique/…`, `/en/shop/…`, `/ar/boutique/…`), rewrites older redirects
  that ended on the old path (no chains) and drops any redirect that started on the new path (no loops).
  The category page endpoint `GET /shop/categories/{slug}` resolves these redirects; nothing else reads
  `redirects` yet.
- **Delete**: `GET /{id}/impact` lists products (and those that would be left without a category),
  subcategories, promotions, shipping rules (none use categories today), variation library entries and menu
  links. Delete is refused (409 `category_in_use`) while an *active* promotion targets the category. `DELETE
  /{id}` (body `products_to`, `subcategories_to`, `menu_links: replace|remove`, `confirm_name` when products or
  subcategories are affected) runs in one transaction: moves or unlinks products (a moved primary link stays
  primary on the replacement; products always keep exactly one primary category while they have any), moves
  subcategories (default: to the parent), re-points variation entries and menu links to the replacement (or
  removes them), creates the 301s, writes one audit entry with the impact. Deactivating is offered first.
- **Product order**: `product_categories.sort_order` and `is_pinned`. With `default_sort = manual` the category
  lists pinned products first, then the manual order (products of subcategories after its own); other defaults
  (`best_selling`, `newest`, `price_asc`, `price_desc`, `name`) sort automatically, pinned first. A sort chosen
  by the visitor overrides both. Parent categories list their subcategories' products too. The product editor
  only adds or removes links (orders and pins are kept); its first category becomes primary when none is.
- **Caching**: the public tree (`catalog:shop:categories:{locale}`), category pages
  (`catalog:shop:category:{locale}:{slug}`) and product lists live under `catalog:shop:`; every admin write
  (create, edit, tree move, product order, delete) clears `catalog:shop:` and `menus:` after commit and emits
  `content.changed` for the pages that show categories.
- **Public page**: cover (`media_views`, eager, ratio reserved), description (three lines on phones), subcategory
  chips, breadcrumbs (also on product pages, through the primary category) with `BreadcrumbList`,
  `CollectionPage` JSON-LD, canonical without filters (`page` kept), `noindex` while it has no published product
  (`indexable`), and a "Prévenez-moi" newsletter form when empty (the `notify-me` endpoint now also works
  while the shop is open).

## Inventory ledger rules

1. `on_hand` changes **only** through `inventory::apply_movement`, which writes **exactly one**
   movement in the same transaction (reason `receipt`, `sale`, `return_restock`, `adjustment`,
   `count`, `damage`, `expired`, `transfer_in`, `transfer_out`, `correction`). The sum of a level's
   movements is its `on_hand` (tested).
2. A cart reserves nothing. The checkout reserves (`reserved += q`) after locking the levels in a
   stable order (variant, location); the CHECK is the last guard (two buyers of the last jar:
   one wins, the other gets `out_of_stock`).
3. Stock leaves (`on_hand -= q`, `reserved -= q`, one `sale` movement per lot, FEFO) when the order
   is paid online, or when it is handed over / paid for offline methods.
4. Location: the pickup point for pickup orders; else the first active location that fulfils online
   orders and can serve the whole order; else split across locations (recorded per reservation).
5. `count` sets an absolute quantity and writes the difference; `transfer` writes two movements with
   one `transfer_id` (the total is unchanged); returns restock into the lot they left from.
6. Backorders: `on_hand` may go below zero by at most `backorder_limit` (deviation: the spec had
   `on_hand >= 0`, which would make a backordered sale impossible to record). Lot-tracked
   products cannot be sold on backorder.
7. Every availability-status change emits `stock.changed` (cache), `stock.low` (staff email) or
   `stock.back_in_stock` ("email me when it is back").

> The shop is operated by the French company (`mouzoun_fr`). The French rules (seller entity,
> VAT by destination, terms of sale, withdrawal, invoices, cookies, compliance checklist) are in
> [france.md](france.md); e-invoicing in [einvoicing.md](einvoicing.md); personal data in
> [gdpr.md](gdpr.md). Where this file and france.md differ, france.md is current.

## Pricing

`services::shop::pricing::price_cart` is one pure function used by the cart, the checkout, draft
orders and order edits. The browser sends only variant ids and quantities.

- Cents everywhere; tax per line after discounts, rounded half up once:
  inclusive `tax = gross − round(gross / (1 + rate))`, exclusive `tax = round(net × rate)`.
- VAT rate per line from its tax class × the destination country × the date (`tax_rates`);
  a gift box splits its price between classes (`bundle_vat_split`); shipping and fees follow the
  goods' rates by value. `totals.vat_breakdown` gives base / VAT / gross per rate (see france.md).
- Order-level discounts are allocated to lines proportionally, the remainder cents to the largest
  lines: lines always sum to the totals (property-tested with `proptest`).
- Promotions with `scope` shop or all: window, minimum amount / quantity, max uses (orders and
  bookings together), per customer, first order / staying guests, products and categories,
  free shipping, percent on shipping; the best non-stackable offer, then every stackable one.
- Shipping: lines that ship are grouped by shipping profile; each group takes the customer's
  chosen rate of its profile (else the cheapest) in the destination's zone (most specific zone
  first: postal patterns `40*`, regions). A group without a rate makes its lines
  `not_shippable_to_country`. Pickup is free; tent delivery costs `shop.stay_delivery_fee_minor`.
- Cash on delivery: allowed when every chosen rate allows it; the highest COD fee is added.
- Issues: `out_of_stock`, `quantity_limited`, `not_shippable_to_country`,
  `promotion_not_applicable` (not blocking), `below_min_order`, `product_unavailable`,
  `address_required`, `fulfillment_required`, `payment_method_unavailable`, `empty_cart`,
  `tax_unavailable` (no rate for a class in the destination: blocking).

## Order state machine

An order has three independent statuses, recomputed from its records (`orders::recompute`) in the
same transaction as every change, with the order row locked `FOR UPDATE`.

```text
status:              draft ──finalize──► pending ──paid / confirmed / transfer received──► open ──fulfilled + paid──► completed
                       │                    │                                              │
                       └────────────────────┴──────────────── cancel ─────────────────────┴──► cancelled
                     (pay at pickup goes straight to open; card payments wait in pending)

payment_status:      unpaid · pending · partially_paid · paid · partially_refunded · refunded · voided
fulfillment_status:  unfulfilled · partially_fulfilled · fulfilled · partially_returned · returned
fulfilment:          pending → packed → shipped → in_transit → delivered
                             ↘ ready_for_pickup → picked_up (6-digit code)        any → failed / cancelled
return:              requested → approved → received → closed      (requested/approved → rejected)
```

- `amount_paid_minor` / `amount_refunded_minor` are caches of `payments` / `refunds`.
- Order lines snapshot name, options, SKU, image, price, tax and discount **in the order's
  language**: editing a product never changes a past order.
- `CHECK (total = subtotal − discount + shipping − shipping_discount + cod_fee + tax if not included)`.
- Refunds never exceed the refundable balance (checked under the order lock). Offline refunds are
  final at once; provider refunds stay `pending` until the webhook.
- Invoices and credit notes share the gap-free yearly numbering of booking invoices
  (`invoice_counters`). The invoice is issued when the order becomes paid (`order.paid`).
- Pickup codes are derived (HMAC of the fulfilment id), only their hash is stored; the order page
  shows the code, staff check it at the counter (and can collect the payment in the same step).

## Checkout and webhook sequence

```text
browser            API                                    Postgres                  Stripe
  │ POST /v1/shop/checkout (Idempotency-Key)
  │──────────────────►│ lock cart, re-price, refuse blocking issues
  │                   │ customer (find or create by email), order pending + snapshots,
  │                   │ reserve stock (hold 30 min), payment row, cart converted ─► commit
  │                   │ create Checkout Session FROM THE ORDER (Idempotency-Key) ───────────────►│
  │                   │ amount_total == order total ? else expire + cancel (bug guard)
  │◄── redirect_url ──│
  │ ─────────────── pays on Stripe ──────────────────────────────────────────────────────────►│
  │ success_url (order page, ?paid=1) — changes nothing, polls /orders/{n}/status
  │                   │◄──────────── POST /webhooks/stripe (signed) ─────────────────────────│
  │                   │ verify signature (5 min) → INSERT payment_webhook_events ON CONFLICT DO NOTHING
  │                   │ + outbox payment_webhook.received → 200
  │                   │ worker: lock order, apply once (payment succeeded, consume stock,
  │                   │ order open, order.paid → emails, staff alert, invoice)
  │ status = paid ◄───│
```

- The same key returns the same order (`orders.checkout_key`, plus Redis replay). Rate limits per
  IP and per email.
- Cash on delivery: `requires_confirmation`, stock held without expiry until staff confirm (phone /
  WhatsApp) or cancel. Pay at pickup / bank transfer: held `payments.bank_transfer.hold_days`.
- Events arriving early (a refund before its payment was applied) are retried by the outbox until
  their dependency exists.
- Jobs: every minute expired holds are released and their unpaid orders cancelled (Stripe session
  expired); hourly abandoned-cart emails (once, consent required by default).

## Adding a payment provider (PayPal, CMI)

1. Implement `services::shop::payments::PaymentProvider` (`create_checkout`, `retrieve`, `expire`,
   `refund`, `verify_webhook`, `parse_event` → `ShopOutcome`).
2. Register it in `ShopPayments::from_config` from its environment variables (never the database).
3. Add its name to the `payments.provider` CHECK (new migration) and `PaymentProvider` enum.
4. Route its webhook to `services::shop::webhooks::ingest`; outcomes are applied by the same worker.
5. Add a `payments.<name>.enabled` setting and expose it in `ShopSettings::enabled_methods`.

## Adding a shipping carrier

Carriers are data: add the code to `shop.carriers` (settings), create rates with that carrier
(admin → Shipping). Tracking stays manual (number + link on the fulfilment); label / tracking APIs
are out of scope.

## Settings (`settings` table)

| key | default | meaning |
|---|---|---|
| `shop.status` | `catalog_only` | `closed` (404), `catalog_only` (products visible, cart/checkout 404, "opens soon" form), `open` |
| `shop.currency`, `shop.prices_include_tax` | EUR, true | prices are set and charged in EUR, VAT included |
| `shop.guest_checkout` | true | no account needed |
| `shop.checkout_hold_minutes` | 30 | stock held for a card payment (Stripe minimum 30) |
| `shop.cart_ttl_days` | 30 | inactive carts expire |
| `shop.low_stock_threshold` | 5 | default "only n left" level |
| `shop.min_order_minor`, `shop.max_quantity_per_line` | 0, 20 | limits |
| `shop.ships_to_countries`, `shop.carriers` | ["FR"], [colissimo, mondial_relay, chronopost, dpd] | address form, carriers list |
| `shop.withdrawal_days` | 14 | right of withdrawal after delivery (Consumer Code L221-18) |
| `shop.stay_delivery_enabled`, `shop.stay_delivery_fee_minor` | false, 0 | delivery to the tent (never for the French entity) |
| `shop.abandoned_cart_hours`, `shop.abandoned_cart_requires_consent` | 4, true | recovery email |
| `shop.notify_emails` | [] | staff alerts (else `notifications.staff_email`) |
| `shop.route_prefix` | boutique / shop | URL segment in email links (the Nuxt pages use `/boutique`, `/en/shop`, `/ar/boutique`: change both together) |
| `shop.legal_entity`, `bookings.legal_entity` | mouzoun_fr, mouzoun_ma | seller of the shop / of booking invoices (`legal_entities`) |
| French operation (`shop.refund_deadline_days`, `shop.legal_guarantee_years`, `shop.consumer_mediator`, `shop.checkout_button_label`, `shop.eu_countries`, `tax.*`, `customs.*`, `legal.*_page_id`, `payments.allow_cash_fr`, `privacy.data_retention`) | | see france.md |
| `payments.stripe.enabled`, `.checkout_mode`, `.statement_descriptor` | false, hosted, MOUZOUN | card payment |
| `payments.cod.enabled`, `.max_order_minor` | false, null | cash on delivery (locked for a French seller) |
| `payments.pay_at_pickup.enabled` | false | pay at the camp (locked for a French seller) |
| `payments.bank_transfer.enabled`, `.details`, `.hold_days` | false, —, 5 | bank transfer |

Permissions: `shop.read`, `shop.catalog`, `shop.pricing`, `shop.stock`, `shop.orders`,
`shop.refunds`, `shop.settings` (manager and super admin: all; reception: read + orders; editor:
read + catalogue). Orders hold customer data: reading them needs `shop.orders` or `shop.refunds`.

## Deviations from the specification

- Migration `0027_shop` (0014 was taken).
- `inventory_levels.on_hand` may go below zero by the backorder limit (see rule 6).
- `carts.shipping_rate_ids uuid[]` (one chosen rate per shipping profile) instead of a single
  `shipping_rate_id`, which cannot express a cart with several profiles.
- `orders.payment_method`, `orders.checkout_key`, `orders.promotion_codes`,
  `orders.terms_accepted_at`, `fulfillment_items.lots`, `stock_locations.sort_order`, settings
  `shop.export_zero_rated`, `shop.abandoned_cart_requires_consent`, `shop.route_prefix` added.
- Invoices and packing slips are printable HTML pages (print / save as PDF) instead of PDF files.
- CSV import updates existing variants by SKU (dry run first); new products are created in the
  editor.
- Shop URL prefixes are set in the Nuxt pages' i18n paths; the setting feeds the email links.
- The Stripe tests use a local mock server instead of wiremock; the provider trait methods are
  `create_checkout / retrieve / expire / refund / verify_webhook / parse_event`.
- Saved order views are kept per browser.
- One Stripe webhook endpoint for bookings and the shop (`/api/v1/webhooks/stripe`): shop events are
  recognised and queued, booking events keep their existing path.

## Legal points to confirm before going live

French operation: see the validation list at the end of [france.md](france.md). Still valid:

- Food labelling: ingredients, allergens, net quantity, best-before, lot number, origin, producer.
- Shipping food and olive oil abroad (customs, phytosanitary rules, carrier restrictions).
