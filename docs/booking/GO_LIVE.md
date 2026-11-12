# Going live with online booking

Everything below is done by you, in this order. Nothing here needs a code change: online payment
turns on when the Stripe secrets are on the server, and booking opens from the back office once
its checklist passes.

## 1. GitHub secrets (repository `mouzoun-back`, environment `dev`)

New and **required** (the deploy stops without it):

| Secret | Value |
|---|---|
| `GUEST_DATA_KEY` | `openssl rand -base64 32` — encrypts guests' ID numbers. Keep a copy offline: losing it makes stored ID numbers unreadable. |

Required by the go-live checklist in production (guests must receive their confirmation):

| Secret | Value |
|---|---|
| `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASSWORD`, `SMTP_TLS` | your email provider |
| `EMAIL_FROM` (and optionally `EMAIL_FROM_NAME`, `EMAIL_REPLY_TO`) | e.g. `reservations@mouzoun.com` |

Stripe — **add these last** (step 5). All three or none; the deploy and the API refuse a partial set:

| Secret | Value |
|---|---|
| `STRIPE_SECRET_KEY` | `sk_live_…` (or a restricted `rk_live_…`, see docs/booking/README.md) |
| `STRIPE_PUBLISHABLE_KEY` | `pk_live_…` |
| `STRIPE_WEBHOOK_SECRET` | `whsec_…` of the webhook endpoint (step 5) |

Optional: `TURNSTILE_SECRET_KEY` (bot protection), `GOOGLE_CLIENT_ID`/`GOOGLE_CLIENT_SECRET`,
`APPLE_CLIENT_ID`/`APPLE_TEAM_ID`/`APPLE_KEY_ID`/`APPLE_PRIVATE_KEY` (docs/accounts/README.md).

`APP_ENV=production`, `PAYMENTS_FAKE=false`, the API/WebAuthn URLs and secure cookies are set by
the workflow from `APP_URL`; production refuses to start with `PAYMENTS_FAKE=true`.

## 2. Push (back first, then front)

```
cd back  && git push origin dev     # CI: tests → image → blue/green deploy
cd front && git push origin dev
```

Why production was stuck on an old build: migrations 0006/0008/0012 had been edited after being
applied, so every API start since was refused (deploys 11–13 failed their health gate and the
previous release kept serving). They are restored and the change moved to 0020; CI now fails
before deploying if that happens again.

## 3. Check the deploy

- GitHub Actions → *CI/CD - DEV - BACK*: all three jobs green.
- `curl https://mouzoun.com/api/health/ready` → `"status":"ok"`, `"database":true`,
  `"cache":true`, `"storage":true`, `"build":{"commit":"<the commit you pushed>"}`,
  `"payments":{"online":false}` (until step 5).
- `curl https://mouzoun.com/api/v1/booking/config` → `"enabled":false` (no more 404).
- On the server, optional: `sudo docker exec mouzoun-back-<blue|green> /app/migrations status`
  → every migration `applied`, up to 0022.

Rollback (`workflow_dispatch` with `image_tag`) only to images built from this release or later:
older images do not know migrations 0013–0022 and refuse to start on the migrated database.
To stop bookings without a rollback, close booking from the back office (step 6).

## 4. Content and catalogue (back office)

Open **Back office → Opening online booking** (`/admin/go-live`). Fix every red item:

- **Prices**: every published stay, activity, package and extra has an active price in euros.
- **Rate plans**: at least one public rate plan, each with its cancellation policy.
- **Availability**: stays have places to sell in the next 60 days; activities with fixed
  departures have open slots in the next 60 days.
- **Texts**: name and short description of each offer, rate plans and cancellation policies in
  French, English and Arabic.
- **Emails**: SMTP configured (step 1).

## 5. Stripe

1. Stripe dashboard → Developers → Webhooks → Add endpoint
   `https://mouzoun.com/api/v1/payments/webhooks/stripe` with the events
   `payment_intent.succeeded`, `payment_intent.payment_failed`, `payment_intent.requires_action`,
   `payment_intent.canceled`, `refund.created`, `refund.updated`, `refund.failed`,
   `charge.dispute.created`. Details: docs/booking/README.md → *Stripe dashboard*.
2. Add the three `STRIPE_*` secrets (step 1) and re-run the last back deploy (Actions → the
   last run → *Re-run all jobs*), or push.
3. In Stripe → the endpoint → *Send test event* → `payment_intent.succeeded`. The go-live page
   then shows **Stripe live** and the time of the last webhook received; `/api/health/ready`
   shows `"payments":{"online":true,"provider":"stripe"}`.

## 6. Open booking

Back office → Opening online booking → **Open booking** (enabled only when every required item
is green; needs the `settings.write` permission). Closing is one click, any time, and the site
goes back to "Opening soon".

## 7. One real test booking

1. Book a cheap stay on https://mouzoun.com with your own card, paying the deposit.
2. Check: the confirmation page, the confirmation email, the booking in the back office
   (status *confirmed*, deposit paid), the payment in Stripe.
3. Cancel it from the booking page (or refund it from the back office): the refund shows in
   Stripe and in the booking, and the cancellation email arrives.
4. Your guest account: sign in with the booking's email and find the trip under *My trips*.

If anything fails, close booking (step 6) and look at the back-office alerts (webhook errors,
emails not sent) and the API logs (`sudo docker logs mouzoun-back-<color>`).

## Your development machine

A database created from the edited migrations (before this release) needs a one-time repair:

```
cd back && cargo run --bin migrations -- accept-corrections && cargo sqlx migrate run
```

It only records the restored checksums after checking the schema already has the corrections,
and refuses to run with `APP_ENV=production`.
