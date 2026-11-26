# Catalogue editors (stays, activities, packages, extras)

The back office of what guests book: **Catalogue › Hébergements / Activités / Formules / Extras /
Ressources** and **Tarifs › Saisons / Plans tarifaires / Catégories de voyageurs / Conditions
d'annulation**. API: `/v1/admin/catalog/...` (`src/modules/catalog`), screens:
`front/app/components/admin/catalog`, pages under `front/app/pages/admin/catalog`.

Nothing here computes availability or prices. Calendar rows and departures come from
`services::inventory` (the same functions the daily job runs); the price simulator and the
package availability check call the booking funnel's `services::booking::trip::quote`; the "à
partir de" price and the package items' sample prices use the funnel's per-item pricing
(`trip::pricing::base_lines` / the engine).

## The editor

One editor (`MzCatalogEditor`), configured per kind. Two columns: content on the start side, status
and organisation on the end side (status, booking mode, featured, place, categories of scope
`product` with the first one primary, tags, amenities, internal SKU). A sticky save bar appears with
unsaved changes; leaving the page asks first.

| Tab | Stays | Activities | Packages | Extras | Saved by |
|---|---|---|---|---|---|
| Général (fr required, en, ar; "manquant" markers; lists: highlights, included, not included, to bring) | ✓ | ✓ | ✓ | ✓ | save bar |
| Détails (kind fields) | ✓ | ✓ | ✓ | ✓ | save bar |
| Médias (cover, video, gallery; alt text per language on the media asset) | ✓ | ✓ | ✓ | ✓ | save bar (alt: its own button) |

**Media on the public page.** The video is MP4 (H.264) or WebM, 100 MB max (`.mov` refused: export
as MP4). The cover is the card image (listings, home page) and the video's
poster; the gallery images are the product page's gallery. The video (a public library file of
kind `video`, stays, activities and packages) is the gallery's first, large slide
(`ProductPage.video`, `MzVideoPlayer` shared with the page-builder video section): only the cover is
shown until the visitor taps play, then the file loads and plays with its controls; never autoplay.
No video: the gallery is unchanged. Extras have no page, so their video is not shown.
| Unités | ✓ | | | | at once |
| Calendrier (+ closures) | ✓ | | | | at once |
| Contenu (items) + "Vérifier une date" | | | ✓ | | save bar |
| Programme (itinerary) | | ✓ | ✓ | | save bar |
| Horaires (+ 14-departure preview) | | fixed departures | | | its button |
| Départs | | fixed departures | | | at once |
| Ressources (requirements) | | ✓ | | | its button |
| Tarifs (rules, taxes, simulator) | ✓ | ✓ | fixed price | ✓ | save bar |
| SEO (slug, title 60, description 155, Google preview, share image) | ✓ | ✓ | ✓ | ✓ | save bar |
| Liens (related products, proposed extras, mandatory toggle) | ✓ | ✓ | ✓ | related only | save bar |

**One save, one transaction.** `PATCH /catalog/{kind}/{id}` writes the product, its translations,
organisation and kind fields, plus the tab being edited (`itinerary`, `items`, `prices`, `tax_ids`,
`schedules`, `resource_requirements`, each under its permission). The body carries the
`updated_at` the editor loaded: when someone saved since, the API answers **409 `stale`** and the
editor offers to reload.

**Slugs.** Made from the name until typed (same rule as the shop: lowercase, French accents removed,
Arabic letters kept). Unique per language (`translations.ar.slug: slug_taken`). Every save goes
through `seo::redirects::after_change` in the same transaction: a changed slug of a live page gets
a 301 per changed language; the sitemap file of the kind is rebuilt on its next request.

**Duplicate** copies translations (slugs `-copie` in fr, `-copy` in en, `-<id suffix>` in other
languages, the shop's rule; then `-2`, `-3` when taken; names "(copie)", "(copy)", "(نسخة)"), cover,
video, a new gallery with the same images, categories, tags, amenities, related products, proposed
extras, price rules, taxes, schedules (**inactive**), itinerary, package items and resource
requirements, as a **draft**. Never units, departures, calendar rows or bookings (a stay copy gets
its own calendar rows, capacity 0 until it has units).

**List.** Thumbnail, name (admin language, French otherwise), status, booking mode, "à partir de"
(lowest price of the next 30 days, cached 10 minutes under `catalog:`), key numbers (stays: units or
places, max guests; activities: duration, next departure, places left this week; packages: nights,
items), missing languages, last change. Search and filters (status, category, place, booking
mode, sort) live in the URL. Rows are dragged (or moved with the arrows) to set `sort_order`, the
order of the public listings. Bulk publish, unpublish, archive (each product in its own
transaction: one refusal does not stop the others).

**Every write** records an audit row and enqueues `product.updated` (product page, listings and
"from" prices under `catalog:`, availability) and `seo.changed` for the kind's sitemap file. Shared
pricing settings enqueue `product.updated` too (every price may change).

## Stays: units or pool

- **Unit mode**: numbered pitches or tents (`accommodation_units`: code, name, zone place, status
  Actif / Maintenance / Retiré, coordinates, notes). Inline add, bulk create ("Créer P-01 à P-12"),
  reorder. A night's capacity = active units not closed that night (`closures` of scope `unit`).
  `services::inventory::sync_unit_capacity` recomputes every future night in the same transaction
  whenever a unit or a unit closure changes; the booking flow can call it too.
- **Pool mode**: one number, `accommodation_types.pool_capacity` (migration 0036; NULL keeps the old
  behaviour, capacity = active units). `set_pool_capacity` writes it on every future night, or on
  a range with weekdays from the calendar.
- **Refusals, never moves.** A change that would leave any future night below its `reserved` count
  is refused (409 `below_reserved`) with the nights and the bookings concerned (reference, dates,
  link). Retiring or closing a unit allocated to a future booking: 409 `unit_booked`. Switching
  unit ↔ pool with future bookings: 409 `mode_locked`. Deleting a unit that was ever allocated:
  409 `unit_has_history` (retire it). Bookings are never moved or cancelled here.
- **Calendar.** Month grid (capacity, reserved, available, stop sell, minimum stay override, CTA,
  CTD, season colour, closed). Click a first night then a last one; restrict to weekdays; apply stop
  sell, minimum nights (or clear it), CTA, CTD and, in pool mode, capacity. Product closures (period,
  reason, public notice) stop sales (the booking flow checks them); unit closures lower capacity.
- **Rows.** `generate_inventory_calendar` keeps rows 18 months ahead (`CALENDAR_HORIZON_MONTHS`,
  daily job `jobs::inventory`, and at once when a stay is created or duplicated). Existing rows are
  never overwritten by the generator.

## Activities: schedules and departures

- `scheduling_mode`: **Départs fixes** (departures generated from schedules), **Sur demande**
  (guests suggest a day; forces `booking_mode = on_request`), **À tout moment** (any day, no
  departure limit).
- **Schedules** (weekdays bitmask Mon=1 … Sun=64, start time in camp time, capacity, validity,
  active). The preview shows the next 14 departures of the schedules being edited
  (`POST .../schedules/preview`, same planner as the generator).
- **Generation.** `services::inventory::plan_slots` turns schedules into departures with the real
  tz database (`camp_time::to_utc`, Africa/Casablanca: the Ramadan clock changes and the permanent
  UTC+0 from 20 September 2026 are never a fixed offset); `ends_at = starts_at + duration`.
  `sync_activity_slots` (schedule save) upserts on `(product_id, starts_at)`, updates or deletes
  only **future departures with no booking**, and reports the booked ones it kept
  (`kept_booked`); departures of a removed schedule follow the same rule. The daily job inserts the
  missing ones up to `booking.max_days_ahead` (365). Product closures close the departures they
  cover; deleting the closure reopens the closed ones of its period.
- **Departures**: capacity (never below the bookings: 409 `below_reserved`), close / reopen, cancel
  (only without bookings: 409 `slot_booked` with the bookings, cancel them from the booking screen),
  guide and language, one-off departures.
- **Resources**: "1 quad pour 2 participants" (`product_resource_requirements`); the tab shows the
  active resources of each kind and the resulting maximum per departure, and flags schedules above
  it. Catalogue › Ressources lists the resources (code, kind, name, seats, linked user for guides).

## Packages

- `pricing_mode`: **Prix fixe** (own price rules), **Somme des éléments**, **Somme moins remise**
  (`discount_percent` required, 0–100). In the sum modes the Pricing tab has no rules: the
  simulator shows the price item by item from the engine.
- **Items** (stays, activities, extras): quantity, day (1 … `duration_days`), optional, order.
  Checked by the API: no package or shop product inside (`package_in_package`,
  `not_bookable_kind`), at most one stay (`one_stay_only`) whose quantity = `duration_nights`
  (`nights_mismatch`), days within the duration (`range`). Each item shows its status and its price
  for one adult on a sample date.
- An item used by a **published** package cannot be archived or unpublished (409 `in_use`, the
  packages listed); its editor shows "Utilisé dans N formules publiées".
- **"Vérifier une date"** asks `trip::quote` (back-office mode) for the package, a start date and a
  party (`adult:2,child:1`) and shows which item blocks. No availability is stored for packages.

## Price rules and the simulator

Rules (`prices`): rate plan (all or one), season (base or one), guest category (all or one),
pricing unit, amount (euros typed, **integer cents** in the API), extra guest amount (stays),
min/max quantity, minimum nights, weekdays bitmask, validity (end excluded), priority, active.

Allowed units per kind (checked by the API, `not_allowed_for_kind`):

| Kind | Units |
|---|---|
| Stays | `per_unit_per_night`, `per_person_per_night` |
| Activities | `per_person`, `per_unit`, `per_booking`, `per_hour` |
| Packages (fixed price) | `per_person`, `per_unit`, `per_booking` |
| Extras | `per_person`, `per_unit`, `per_booking`, `per_hour`, `per_day`; + the nightly ones when `requires_date` |

**Matching** (the engine, `services::pricing::engine`): a rule applies to a night or date when its
rate plan is NULL or the requested one, its season is NULL or the date's season (the highest-priority
season whose period contains the date), the weekday is in its mask, the validity contains the date,
the minimum nights and the quantity range are met. Among the applicable rules the **most specific**
wins (one level each for rate plan, season, guest category, validity, weekday restriction, minimum
nights), then the highest **priority**, then the lowest id. Examples, a khaïma:

| Rule | Specificity | Tuesday 9 June 2026 (low season) | Saturday 13 June 2026 (low season) | Saturday 20 Dec 2025 (peak) |
|---|---|---|---|---|
| 45 € every night | 0 | **applies, wins** | applies | applies |
| 55 € Fri + Sat nights | 1 (weekdays) | — (Tuesday) | **wins** (1 > 0) | applies (1) |
| 60 € high season | 1 (season) | — | — | — (the night's season is peak: priority 20 > 10) |
| 70 € peak season | 1 (season) | — | — | applies (1): ties with 55 € → the higher **priority** wins, then the lower id |

Giving the peak rule priority 1 makes it win on peak weekends; without it the grid flags the two
rules as ambiguous.

When two active rules can apply to the same case with the same specificity and priority, the
grid shows both in orange: "Deux règles s'appliquent au même cas ; la priorité décide"
(`engine::ambiguous_pairs`, the engine's own fields).

**Simulator** (`POST .../price-quote`): dates (or a departure), guests per category, rate plan,
promo code → `trip::quote` in back-office mode, the function the funnel uses: lines with the rule
used, discount, taxes, total, deposit, cancellation steps. A test compares it with the public
funnel's quote for the same input.

**Taxes**: the product's taxes checklist; for stays the tourist tax (fixed per person per night) is
suggested.

**Percentages** cross the API in one format: answers are decimal strings (`"12.5"`), requests may
send a string or a JSON number, read from its text without float arithmetic
(`web::percent`). This also fixed the shop's gift box VAT split (`share_percent`), refused when
sent as a number and then ignored at checkout once stored as a string.

## Publication, delete and archive

Status: Brouillon → Publié → Archivé, and back. Publishing sets `published_at` the first time and
runs the checklist shown in the status column:

- **Blocking**: French name, slug and short description; cover image with French alt text; for
  bookable products at least one active price rule (packages in a sum mode: items, each priced) and
  a cancellation policy reachable through a rate plan the prices allow; stays: an active unit or
  places; activities with fixed departures: an active schedule and an upcoming open departure;
  packages: at least one item, every non-optional item published.
- **Warnings** (never block): English or Arabic missing, no gallery, gallery images without alt
  text, no SEO description, no category.

A published product that passed its blocking checks keeps passing them on save (a save that would
break one answers 422 `publish_blocked`); an older product that never passed can still be edited.

Unpublishing and archiving never touch bookings. The public page follows the route registry:
unpublished → inactive redirect to the listing, deleted → the redirect becomes active. **Delete**
is a soft delete (`deleted_at`) allowed only for products never booked (`booking_items`,
`RESTRICT`); otherwise 409 `booked` and the editor offers "Archiver". A product with prices,
schedules or units asks for its French name (`?confirm=`). A product used by a published package
or as a mandatory extra cannot be archived or unpublished until it is removed there.

## Permissions

| | super_admin / manager | editor | reception | guide |
|---|---|---|---|---|
| See the editors (`products.read`) | ✓ | ✓ | ✓ | ✓ (granted by 0036) |
| Content, media, SEO, organisation, status, itinerary, items (`products.write`) | ✓ | ✓ | read | — |
| Price rules, taxes, Tarifs screens (`pricing.write`) | ✓ | — | — | — |
| Units, calendar, closures, schedules, departures, resources (`inventory.write`) | ✓ | — | read | read |

Tabs are read-only without the write permission; actions a user cannot use are hidden. The menu
shows Catalogue with `products.read` (Ressources with `inventory.read`) and Tarifs with
`pricing.read`. No permission code was added; migration 0036 gives the `guide` role `products.read`
so guides can open the departures and the calendar (read-only). Pickers (place, category, tag, amenity, product) use
`GET /v1/admin/lookup/{kind}` (`content.read`, which editors and managers have).

## Dev seed

`seed_dev` creates: stays "Khaïma nomade" (unit mode, K-01 … K-06, K-06 in maintenance, weekend
surcharge Fri + Sat nights), "Emplacement tente" (pool, 20 places), "Emplacement van" (unit mode,
V-01 … V-08); seasons low / high / peak with periods; rate plans Flexible and Non remboursable;
child at half price; activities "Balade en quad" (09:00 and 16:00 daily, 1 quad per 2, 4 quads),
"Visite du village de M'ssici" (10:00, guided, 4-step itinerary), "Visite de la ferme et des
troupeaux" (anytime), "Nuit sous les étoiles" (on request); extras breakfast, dinner, firewood,
transfer from Rissani, lunch; packages "Week-end dans le désert" (2 nights khaïma + quad + village,
sum minus 10 %), "Journée découverte" (fixed price: farm + village + lunch) and the draft "Bivouac
des nomades" (French only). Placeholder covers and galleries with alt text in fr/en/ar.

## API

`/v1/admin/catalog/...` (kind = `stays`, `activities`, `packages`, `extras`):

- `GET/POST /{kind}`, `GET/PATCH/DELETE /{kind}/{id}`, `POST /{kind}/{id}/duplicate`,
  `POST /{kind}/{id}/publish|unpublish|archive`, `GET /{kind}/{id}/publish-checklist`,
  `PUT /{kind}/order`, `POST /{kind}/bulk`, `GET/PUT /{kind}/{id}/itinerary`.
- Stays: `GET/POST /stays/{id}/units`, `POST .../units/bulk`, `PUT .../units/order`,
  `PATCH/DELETE .../units/{unit_id}`, `PUT .../inventory` (mode, pool capacity),
  `GET/PATCH .../calendar`, `GET/POST .../closures`, `DELETE .../closures/{closure_id}` (closures
  also for activities).
- Activities: `GET/PUT .../schedules`, `GET/POST .../schedules/preview`, `GET/POST .../slots`,
  `PATCH /catalog/slots/{slot_id}`, `GET/PUT .../resource-requirements`, `GET /catalog/guides`.
- Packages: `GET/PUT .../items` (`?date=` sample prices), `GET .../availability?start&party`.
- Pricing: `GET/PUT .../prices`, `POST .../price-quote`, `PUT .../taxes`; CRUD of
  `/catalog/pricing/seasons|rate-plans|guest-categories|cancellation-policies` and
  `/catalog/resources`; `GET /catalog/references` (lists for the editors' selects).

Errors use the admin format: `invalid_fields` with paths (`translations.ar.slug`,
`prices[2].amount_minor`, `items[1].quantity`), and stable codes for refusals (`stale`,
`publish_blocked`, `below_reserved`, `unit_booked`, `mode_locked`, `slot_booked`, `in_use`,
`booked`, `confirm_name`).
