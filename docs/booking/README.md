# Booking system

One booking ("trip") can hold stays, activities, extras and packages, with **one reference**
(`MZ-<yy>-<number>`, restarting every year), **one total**, **one payment** and **one
confirmation**. Everything is behind the `booking.enabled` setting: while it is `false`, the
website shows "Opening soon" and `POST /v1/checkout` answers `503 booking_closed`.

```
search / product page ─► basket (Redis, cookie) ─► checkout ─► hold ─► payment ─► webhook ─► confirmed
        │                     re-priced on read      │ one transaction:          (Stripe / fake)
        │                                            │ lock rows, re-check,
   quote (signed) ──────────────────────────────────►│ re-price, compare with
                                                     │ the signed quote, write
```

Code: `src/services/booking/trip` (pricing and creation of a trip), `src/services/booking/state.rs`
(state machine), `manage.rs` (cancel, refunds, staff decisions), `register.rs` (guest register),
`src/services/payments` (providers, webhooks, refunds), `src/modules/booking` (public API),
`src/modules/booking_admin` (back office), `src/jobs/booking_ops.rs` (hourly jobs). Front:
`front/app/pages/{stays,activities,packages,trip,checkout,booking}`, `front/app/components/booking`,
`front/app/pages/admin/bookings`.

## The funnel

| Step | Page / endpoint | Notes |
|---|---|---|
| Search | `/stays?checkin=…&checkout=…&adults=2&children=1&child_ages=7` · `POST /v1/availability/search` | Search state in the URL; last search in localStorage. Each stay with its rate plans side by side, totals for the stay (mandatory fees and taxes included), nearest free dates when unavailable. |
| Date picker | `GET /v1/availability/calendar?product_id&from&to` | Per day: available, remaining, CTA/CTD, min stay, "from" price (setting `booking.calendar_show_prices`). Cached 60 s under `availability:`. |
| Activity slots | `GET /v1/availability/slots?product_id&from&to` | Seats left, guide language, `open` / `full` / `too_late` / `closed`. |
| Product pages | `/stays/{slug}` · `/activities/{slug}` · `/packages/{slug}` · `GET /v1/catalog/products/{slug}` | Sticky booking widget (mobile bottom bar), schema.org, hreflang through localized slugs. |
| Quote | `POST /v1/quote` | Returns the quote and a **signed quote token** (HMAC, 60 min, bound to the exact request). |
| Basket | `GET/PUT/DELETE /v1/cart`, `POST /v1/cart/items`, `PATCH/DELETE /v1/cart/items/{id}` | Redis, `__Host-mz_cart` HttpOnly cookie, 7 days. Never reserves anything. Every read re-prices and returns `notices` (price changed, unavailable). Mandatory extras and the pitch-selection fee are added by the engine, cannot be removed. |
| Upsell | `/trip` ("Complete your trip") | Extras of the booked stays, activities with departures during the stay. One screen, skippable. |
| Checkout | `/checkout` · `POST /v1/checkout` | `Idempotency-Key` header (24 h, Redis), rate limit, Turnstile when configured, terms required, marketing opt-in unchecked. Creates the booking and **holds** the inventory. |
| Payment | `/booking/{ref}/pay` | Stripe Payment Element (Stripe.js loaded only here) or the fake provider. Countdown of the hold. |
| Confirmation / manage | `/booking/{ref}?t=…` · `GET /v1/bookings/{ref}` | Signed access token (`t`, same as the email links), magic links (`m`), polling of `/status` after payment. Pay balance, arrival details, pre-check-in, cancel with the refund shown first, `.ics`, receipt (print). |
| Find my booking | `/booking/find` · `POST /v1/bookings/find` | Always the same answer; a single-use link (60 min) goes to the booking's email. Rate limited per IP and per reference. |

### Price rules

- First price shown for a stay = total for the dates and party, mandatory extras and fees
  included; exclusive taxes (tourist tax) are shown on the same line.
- Every total has a "Price details" popover built from the engine's `price_breakdown`.
- Promotions are applied **at trip level**: automatic offers (no code) + the typed code;
  every applicable stackable promotion together, or the best non-stackable one, whichever saves
  more; a code that loses against a better offer is reported as `not_combinable`. Limits
  (`max_redemptions`, `max_per_customer`) are enforced under a row lock at checkout.
- **Rounding**: engine lines are `unit amount × quantity` (integer cents); percentages
  (promotion, percent tax, deposit) are rounded once, half away from zero, per item (deposit:
  once on the total); a fixed-amount promotion is split across the eligible items in proportion
  to their subtotals with the largest-remainder method, so the parts add up exactly; item total
  = subtotal − discount + exclusive taxes; booking totals are sums of item totals.
- The browser never sends prices the server trusts: checkout recomputes and compares with the
  signed quote → `409 price_changed` (with the new quote) or `409 quote_expired`.
- Snapshots: items keep unit prices, breakdown, product snapshot (name, cancellation rules,
  start time); the booking keeps `cancellation_policy_snapshot`. Later edits never change
  existing bookings.
- Omnibus: the hourly job records the lowest nightly price per product and date
  (`price_observations`, 35 days). A struck-through price must use the lowest of the last 30
  days (the front does not show struck-through prices yet: promotions are shown as named lines).

### Payment options

From the rate plan's `payment_mode` and deposit:

| payment_mode | options |
|---|---|
| `pay_on_site` | pay at the camp |
| `optional_online` | full now · deposit now (if the plan has one) · pay at the camp |
| `online_required` | full now · deposit now (if the plan has one) |

The deposit option is offered only when the balance due date (`arrival − booking.balance_due_days`)
is still ahead. Balance: on the saved card (consent at checkout, `booking.balance_auto_charge`)
charged off-session by the hourly job on the due date, or a payment link emailed
`booking.balance_reminder_days` before. A declined balance charge creates a staff task and emails
the guest and the team; **a booking is never cancelled automatically for a failed balance**.

## Booking state machine

```
draft ──► pending ──────────► awaiting_payment ──► confirmed ──► checked_in ──► completed
  │          │  │                   │   │              │  │
  │          │  └──► confirmed      │   └──► expired   │  └──► no_show
  │          ├──► cancelled         └──► cancelled     └──► cancelled
  │          └──► expired
  ├──► awaiting_payment
  └──► confirmed
```

`services::booking::state::allowed` is the only table; `transition` locks the row, refuses
anything else (`409 invalid_transition`), stamps `confirmed_at` / `checked_in_at` /
`checked_out_at`, and writes `booking_status_history` + `audit_logs`. Checkout creates bookings
directly in their first status: `pending` (a request: any on-request product or activity),
`awaiting_payment` (something to pay now) or `confirmed` (pay at the camp, nothing due now).
Items can be cancelled one by one (`POST /v1/admin/bookings/{id}/items/cancel`) without
cancelling the booking; the totals are recomputed.

Holds: `awaiting_payment` → `booking.hold_minutes` (15); `pending` (request) →
`booking.request_answer_hours` (24); a request accepted by staff waits for payment
`booking.payment_link_hours` (48). The existing hold-expiry job releases everything and moves
the booking to `expired`. During 3-D Secure (`payment_intent.requires_action`) the hold is
extended by 10 minutes.

**Payment arriving after expiry** (choice): the inventory may have been sold again, so the
payment is **refunded automatically in full**, the guest gets the refund email, and staff get a
`payment_after_expiry` task. The booking stays `expired` (re-confirming would break the state
machine and risk overbooking).

## Payments and webhooks

Providers implement `services::payments::gateway::PaymentGateway` (create payment, refund,
off-session charge, verify webhook). `stripe` (PaymentIntents over the REST API), `fake`
(development and tests). PayPal and CMI are further implementations of the same trait.

1. `payments::start` writes a `payments` row (`pending`, unique `idempotency_key`) then creates
   the provider intent with that key; a reload resumes the same attempt.
2. The browser confirms with the provider (card data never reaches Mouzoun: PCI SAQ A).
3. `POST /v1/payments/webhooks/{provider}`: signature checked (Stripe: `t=…,v1=…`, 5 min
   tolerance), event **stored first** in `payment_webhook_events` (unique provider + event id),
   then processed in one transaction: payment `succeeded`, `amount_paid_minor` recomputed,
   booking `awaiting_payment → confirmed`, outbox `booking.confirmed` + `payment.succeeded`.
   A duplicate delivery is answered 200 and ignored; a processing error answers 500 so the
   provider retries, and the stored event is reprocessed.
4. The return page polls `GET /v1/bookings/{ref}/status` for ~30 s, then says the email will
   follow.

Holds: an unpaid booking keeps its inventory for `booking.hold_minutes`. When the hold expires
the inventory is released and the open PaymentIntent is **cancelled at Stripe** (job `booking
holds`, every minute), so the guest can no longer pay an expired booking. If money still arrives
(a race with the cancellation), it is refunded in full automatically and staff get a task.

Disputes (`charge.dispute.created`) create a `dispute` staff task with the amount, the reason and
the evidence deadline; the answer is given in the Stripe dashboard.

Refunds go through the original provider (`refunds` row with its own idempotency key), in full
or in part;
offline payments (cash, transfer, card terminal) create a `refund_offline` staff task.
Cancellation refund = what was paid − the policy fees, never below zero; fees per item =
`100 − refund %` of the item total, the refund percentage coming from the **snapshot** rules and
the item's start time (check-in time in camp time, or the activity departure).

## Outbox events and emails

| event | guest email | staff email |
|---|---|---|
| `booking.created` (request, or staff-made booking) | `booking_received` | `staff_new_booking` |
| `booking.created` (website, paying online) | — (the guest is on the payment page) | — |
| `booking.confirmed` | `booking_confirmation` | |
| `payment.succeeded` | `payment_receipt` | `staff_new_booking` (website deposit / full payment) |
| `booking.request_accepted` | `booking_payment_link` | |
| `booking.declined` | `booking_declined` | |
| `booking.cancelled` / `booking.expired` | `booking_cancelled` | `staff_booking_cancelled` |
| `booking.modified` | `booking_modified` | |
| `refund.succeeded` | `refund_receipt` | |
| `booking.balance_due` (scheduled) | `balance_reminder` | |
| `booking.balance_failed` | `balance_failed` | `staff_balance_failed` |
| `booking.precheckin_due` (scheduled) | `precheckin_reminder` | |
| `booking.pre_arrival_due` (scheduled, with directions) | `pre_arrival` | |
| `booking.review_due` (scheduled) | `review_request` | |
| `booking.access_requested` | `booking_magic_link` | |
| `cart.abandoned` (marketing consent, once per customer) | `cart_abandoned` | |
| `payment.failed`, `booking.change_requested` | — (guest sees it on the page / staff task) | |

`skip_guest_emails` (phone / agency bookings) and anonymised customers get no guest email.
Templates: migrations 0014 and 0016 (fr/en/ar), edited in the admin later.

## Jobs

| job | where | what |
|---|---|---|
| hold expiry | `jobs/booking_holds.rs`, every minute | expires pending / awaiting-payment bookings past their hold |
| booking operations | `jobs/booking_ops.rs`, hourly | saved-card balance charges, no-show proposals (staff task, never automatic), automatic check-out the day after departure, ID-number purge after `booking.id_retention_days`, price observations |
| email scheduler | `worker/scheduler.rs`, every 5 min | pre-arrival, review request, balance reminder, pre-check-in reminder (dedupe key per booking) |
| outbox dispatcher | `worker` | emails and cache invalidation (`availability:` on every booking event) |

The queue is the existing Postgres outbox (durable, retried with backoff, deduplicated), not a
separate Redis queue.

## Settings (`settings` table)

| key | default | |
|---|---|---|
| `booking.enabled` | false | public funnel open |
| `booking.hold_minutes` | 15 | hold while paying |
| `booking.request_answer_hours` | 24 | answer delay of a request (hold) |
| `booking.payment_link_hours` | 48 | hold after a request is accepted |
| `booking.balance_due_days` | 14 | balance due before arrival |
| `booking.balance_auto_charge` | false | offer to charge the balance on the saved card |
| `booking.balance_reminder_days` | 3 | payment-link email before the due date |
| `booking.balance_grace_days` | 3 | staff delay after a failed balance charge |
| `booking.standalone_activities` | true | day visitors may book activities without a stay |
| `booking.calendar_show_prices` | true | prices in the date picker |
| `booking.urgency_threshold` | 2 | "Only N left" at or below this |
| `booking.self_service_changes` | false | guests change dates online (phase 3) |
| `booking.guest_cancellation` | true | guests cancel online |
| `booking.request_card_authorization` | false | card authorisation on requests (not implemented yet) |
| `booking.precheckin_reminder_days` | 5 | pre-check-in reminder |
| `booking.magic_link_minutes` | 60 | validity of "find my booking" links |
| `booking.pitch_selection` | true | guests may pick a specific pitch |
| `booking.id_retention_days` | 365 | ID numbers kept after departure |
| `legal.terms_version` | 2026-11 | stored on each booking with the acceptance time |
| `notifications.staff_email` | "" | staff notifications |

## Environment

| variable | |
|---|---|
| `STRIPE_SECRET_KEY`, `STRIPE_PUBLISHABLE_KEY`, `STRIPE_WEBHOOK_SECRET` | Stripe (all three, or none) |
| `PAYMENTS_FAKE=true` | fake provider: development and tests only, never on a public server |
| `TURNSTILE_SECRET_KEY` + `TURNSTILE_SITE_KEY` (back, both or none) | bot check on checkout, sign-in and find-my-booking; the site reads the site key from `/v1/booking/config` |
| `GUEST_DATA_KEY` | base64 32-byte AES-256-GCM key for ID numbers (`openssl rand -base64 32`). Without it, ID numbers are refused. **Losing it makes stored numbers unreadable.** |
| `TRUST_PROXY` | default true: client IP from the last `X-Forwarded-For` entry (our nginx) |

Each value is one GitHub secret, like the others.

## Back office

`/admin` (Today), `/admin/bookings` (filters, saved views in the browser, CSV), `/admin/bookings/{id}`,
`/admin/bookings/new`. Endpoints and permissions are listed in
`src/modules/booking_admin/mod.rs`. New permissions (migration 0015): `bookings.cancel`,
`bookings.override_price`, `payments.refund`, `payments.record_offline`, `rates.write`,
`guests.read_identity` (viewing numbers is audited), `reports.read`, `activities.manifest`.
Amounts are typed in euros (`"45.50"`) and converted to cents exactly at the API boundary.

## Testing

```bash
# back: unit + integration (real Postgres and Redis from docker compose)
cargo test                       # booking_trip, booking_api, booking_admin, email, ...

# front: unit, mocked e2e
npm test && npm run build && npm run test:e2e

# front: full stack (real API with PAYMENTS_FAKE=true; the setup project opens booking itself)
# Use a database and Redis of their own: an API already running on your dev database would
# consume the outbox (sign-in codes, emails) before the suite reads them in Mailpit.
createdb mouzoun_e2e   # once, then: DATABASE_URL=…/mouzoun_e2e cargo sqlx migrate run && cargo run --bin seed_dev
DATABASE_URL=…/mouzoun_e2e cargo run --bin seed_dev -- staff e2e-staff@mouzoun.test 'A-long-password-1' super_admin
DATABASE_URL=…/mouzoun_e2e REDIS_URL=…/1 APP_PORT=18090 PAYMENTS_FAKE=true COOKIE_SECURE=false \
  EMAIL_PROVIDER=smtp SMTP_HOST=127.0.0.1 SMTP_PORT=1025 SMTP_TLS=none \
  CORS_ORIGINS=http://127.0.0.1:3210 PUBLIC_SITE_URL=http://127.0.0.1:3210 \
  WEBAUTHN_ORIGIN=http://127.0.0.1:3210 WEBAUTHN_RP_ID=127.0.0.1 cargo run --bin mouzoun-api
npm run build
E2E_STAFF_EMAIL=e2e-staff@mouzoun.test E2E_STAFF_PASSWORD='A-long-password-1' \
  npx playwright test -c playwright.fullstack.config.ts      # + STRIPE_E2E=1 with Stripe test keys
```

The suite covers French desktop and Arabic mobile (stay + extra + activity with deposit, pay at
the camp then cancel with refund, account claim), the back office (request accepted, phone
booking in cash) and accessibility checks (axe) on every screen it passes through.

### Stripe test cards and the Stripe CLI

1. Use test keys (`sk_test_…`, `pk_test_…`) in `STRIPE_SECRET_KEY` / `STRIPE_PUBLISHABLE_KEY`.
2. Forward webhooks locally:
   `stripe listen --forward-to http://localhost:8080/v1/payments/webhooks/stripe`
   and put the printed `whsec_…` in `STRIPE_WEBHOOK_SECRET`.
3. Cards: `4242 4242 4242 4242` (success), `4000 0025 0000 3155` (3-D Secure required),
   `4000 0000 0000 9995` (declined, insufficient funds), `4000 0000 0000 0341` (attaches, then
   fails off-session: balance-failure path). Any future expiry, any CVC.
4. Replay an event: `stripe events resend evt_…` — processed once (duplicate deliveries are
   ignored).

### Stripe dashboard (production)

Online payment turns on by itself when the three Stripe secrets are on the server: no code
change, no setting. Until then guests only see "pay at the camp" (prepayment-only rate plans
cannot be booked online) and the back office says "Online payment not configured".

1. **API keys** — Developers → API keys: the publishable key (`pk_live_…`) and a secret key.
   A restricted key (`rk_live_…`) is enough with these permissions: *PaymentIntents* write,
   *SetupIntents* write, *Customers* write, *Payment methods* write, *Refunds* write.
2. **Webhook** — Developers → Webhooks → Add endpoint:
   - URL: `https://mouzoun.com/api/v1/payments/webhooks/stripe`
   - API version: the account's default.
   - Events (exactly these; anything else is answered 200 and ignored):
     | Event | What it does |
     |---|---|
     | `payment_intent.succeeded` | payment recorded, booking confirmed (or refunded if its hold expired) |
     | `payment_intent.payment_failed` | payment failed; for an off-session balance: emails + staff task |
     | `payment_intent.requires_action` | 3-D Secure in progress: the hold is extended by 10 minutes |
     | `payment_intent.canceled` | payment marked cancelled |
     | `refund.created`, `refund.updated`, `refund.failed` | refund confirmed or failed |
     | `charge.dispute.created` | chargeback: staff task with the evidence deadline |
   - Copy the endpoint's **signing secret** (`whsec_…`) into `STRIPE_WEBHOOK_SECRET`.
3. **Payment methods** — Settings → Payment methods: cards (with Apple Pay / Google Pay) on;
   3-D Secure is handled by Stripe ("automatic payment methods" on every PaymentIntent).
4. **Statement descriptor** — Settings → Business → Public details: `MOUZOUN` so guests
   recognise the charge.
5. Check: the go-live page (back office) shows "Stripe live" and, after the first test event
   (Webhooks → your endpoint → *Send test event* → `payment_intent.succeeded`), the date of the
   last webhook received. `GET /api/health/ready` reports `"payments": {"online": true}`.

Every write to Stripe carries an `Idempotency-Key` (payment attempt, refund, off-session charge,
cancellation, customer, setup intent), so retries never charge or refund twice. Webhooks are
verified (signature + 5-minute tolerance, constant-time) and stored before processing (one row
per Stripe event id): redeliveries are no-ops.
