# Stripe for the shop

The shop takes card payments (and Apple Pay / Google Pay) through **Stripe Checkout Sessions**:
the customer pays on Stripe's page (or the embedded form), card data never reaches our servers
(PCI SAQ A). Bookings keep their PaymentIntents flow; both share the same Stripe account,
keys and webhook endpoint.

## Keys (environment only)

| variable | value |
|---|---|
| `STRIPE_SECRET_KEY` | a **restricted key** (`rk_live_…` / `rk_test_…`) with write access to Checkout Sessions, Coupons, Refunds, PaymentIntents, Customers, SetupIntents (bookings) and read access to Charges |
| `STRIPE_PUBLISHABLE_KEY` | `pk_live_…` / `pk_test_…` (embedded mode and bookings) |
| `STRIPE_WEBHOOK_SECRET` | `whsec_…` of the endpoint below |

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

## Testing locally with the Stripe CLI

```sh
stripe login
stripe listen --forward-to http://127.0.0.1:8080/v1/webhooks/stripe
# copy the printed whsec_… into STRIPE_WEBHOOK_SECRET, with test keys in STRIPE_SECRET_KEY /
# STRIPE_PUBLISHABLE_KEY, then restart the API
stripe trigger checkout.session.completed   # an event without our metadata is ignored
```

Pay a real test order with `4242 4242 4242 4242` (any future date, any CVC); `4000 0025 0000 3155`
asks for 3-D Secure; `4000 0000 0000 9995` is declined.

Without Stripe at all, `PAYMENTS_FAKE=true` (development only) simulates card payments: the order
page offers "simulate a successful / failed payment", which goes through the same webhook pipeline.
