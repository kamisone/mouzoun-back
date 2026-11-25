# Stripe for the shop

The shop takes card payments (and Apple Pay / Google Pay) through **Stripe Checkout Sessions**:
the customer pays on Stripe's page (or the embedded form), card data never reaches our servers
(PCI SAQ A). Bookings keep their PaymentIntents flow; both share the same Stripe account,
keys and webhook endpoint.

**The Stripe account is also shared with another site (vitecamio).** Mouzoun tags everything it
creates and ignores the other site's events: see [Shared account](#shared-account).

## Keys (environment only)

| variable | value |
|---|---|
| `STRIPE_SECRET_KEY` | a **restricted key** (`rk_live_…` / `rk_test_…`) with write access to Checkout Sessions, Coupons, Refunds, PaymentIntents, Customers, SetupIntents (bookings) and read access to Charges |
| `STRIPE_PUBLISHABLE_KEY` | `pk_live_…` / `pk_test_…` (embedded mode and bookings) |
| `STRIPE_WEBHOOK_SECRET` | `whsec_…` of **Mouzoun's own** endpoint below (never the other site's). Empty: payments can be created but every webhook is refused, and the checklist line "Stripe webhook" stays red |
| `STRIPE_APP_TAG` | optional, default `mouzoun`: the `metadata.app` value of every object Mouzoun creates |

The keys are never stored in the database, returned by any endpoint or logged. Without them the
API boots, logs one warning, and the checkout hides card payment even if `payments.stripe.enabled`
is true (turning it on is refused). The back office (Shop → Settings) shows: configured or not,
test or live (from the key prefix), and when the last webhook arrived.

The API version is pinned in code (`STRIPE_API_VERSION`, sent as `Stripe-Version`).

## Creating the keys

1. Stripe Dashboard → Developers → API keys: copy the publishable key; create a restricted key
   with the permissions above.
2. Developers → Webhooks → Add endpoint: `https://<domain>/api/v1/webhooks/stripe` (nginx strips
   `/api`; the spec's `/api/webhooks/stripe` became this path), with these events:
   - `checkout.session.completed`
   - `checkout.session.async_payment_succeeded`
   - `checkout.session.async_payment_failed`
   - `checkout.session.expired`
   - `charge.refunded`
   - `refund.updated`
   - `charge.dispute.created`
   - plus the booking events already used (`payment_intent.*`, `refund.created`, `refund.failed`).
3. Copy the signing secret into `STRIPE_WEBHOOK_SECRET`, restart the API.
4. Shop → Settings: switch on "Offer card payment".

## What the API does

- The session is created from the **order** (never the cart): one line per order line (price in
  EUR cents from the snapshot), shipping as a fixed shipping option, the order discount as a
  one-time coupon, `customer_email`, `client_reference_id = order id`, metadata `{order_id,
  payment_id}` on the session and its PaymentIntent, locale (Arabic → `auto`), expiry = the stock
  hold (≥ 30 min), success / cancel URLs on the signed order page.
- Every create and refund call carries an `Idempotency-Key`.
- After creating the session the API checks `amount_total == orders.total_minor`; otherwise the
  session is expired and the order cancelled (bug guard).
- The webhook verifies `Stripe-Signature` (5-minute tolerance), stores the event
  (`ON CONFLICT DO NOTHING`: a redelivery is ignored), queues it, answers 200. The worker applies
  it once under the order lock: paid → stock consumed, order open, emails, invoice; a different
  amount or currency → recorded but `risk_level = review` and a staff alert (never auto-fulfilled);
  expired / failed → stock released, order cancelled; refunds → refund rows settled (dashboard
  refunds are recorded too); dispute → `risk_level = high` and a staff alert.
- The success redirect never changes anything: the order page polls the status until the webhook
  has marked the order paid.

## Shared account

The Stripe account (and its keys) is shared with vitecamio. To keep the two sites apart:

- **Tag.** Every object Mouzoun creates carries `metadata.app = "mouzoun"` (`STRIPE_APP_TAG`):
  Checkout Sessions and their PaymentIntent (`payment_intent_data.metadata`, with `order_id`,
  `payment_id`, `order_number`), booking PaymentIntents and Customers, refunds and coupons.
- **Filter.** After the signature check, an event whose object has no `metadata.app = "mouzoun"`
  (for a charge, refund or dispute without metadata of its own: the tag of its PaymentIntent,
  read from the API) is stored in `payment_webhook_events` with status `ignored_foreign` (event
  id and type only, no customer data), answered **200** (so Stripe does not retry or disable the
  endpoint) and never reaches orders, bookings, alerts or the worker. Objects created before the
  tag existed are recognised by their `order_id` / `payment_id` / `booking_id` (our UUIDs).
- **Statement descriptor.** Checkout Sessions set `statement_descriptor_suffix` from the setting
  `payments.stripe.statement_descriptor` (default `MOUZOUN`; Latin capitals, digits and spaces,
  accents removed, 22 characters at most), so customers recognise the charge after the account's
  prefix.

### Owner steps (Stripe Dashboard, nothing is created by code)

1. Developers → Webhooks → **Add a second endpoint** (keep vitecamio's):
   `https://mouzoun.com/api/v1/webhooks/stripe`, with the events listed above.
2. Copy **this endpoint's** signing secret into Mouzoun's `STRIPE_WEBHOOK_SECRET` (local `.env`
   and the GitHub Actions secret of the back repository), restart the API.
3. Pay one test order (or `stripe trigger` with our metadata): the checklist line "Stripe webhook"
   turns green once one of our events has arrived.
4. Settings → Business → Public details → **Business website**: add the Mouzoun domain (card
   networks and Stripe check that the descriptor and site match the business).
5. Ask vitecamio to **ignore events without its own tag** too: Mouzoun's events also reach
   vitecamio's endpoint (same account), and its code must not act on them.
6. Checkout branding (logo, colours, name on the Stripe page) is the account's, so vitecamio's:
   Mouzoun customers will see it on the Stripe-hosted page. A separate Stripe account (or
   embedded mode, `payments.stripe.checkout_mode = embedded`) avoids this; out of scope here.

## Testing locally with the Stripe CLI

```sh
stripe login
stripe listen --forward-to http://127.0.0.1:8080/v1/webhooks/stripe
# copy the printed whsec_… into STRIPE_WEBHOOK_SECRET, with test keys in STRIPE_SECRET_KEY /
# STRIPE_PUBLISHABLE_KEY, then restart the API
stripe trigger checkout.session.completed   # untagged: stored as ignored_foreign, answered 200
stripe trigger checkout.session.completed --add checkout_session:metadata.app=mouzoun
```

Pay a real test order with `4242 4242 4242 4242` (any future date, any CVC); `4000 0025 0000 3155`
asks for 3-D Secure; `4000 0000 0000 9995` is declined.

Without Stripe at all, `PAYMENTS_FAKE=true` (development only) simulates card payments: the order
page offers "simulate a successful / failed payment", which goes through the same webhook pipeline.
