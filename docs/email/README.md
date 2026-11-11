# Transactional emails

```
business transaction ──► outbox_events row (same transaction, ids only)
                              │  NOTIFY outbox_events (trigger, on commit)
                              ▼
          worker: claim due events FOR NO KEY UPDATE SKIP LOCKED
                              │  Registry: event_type → handlers
                 ┌────────────┴─────────────┐
           EmailHandler              CacheInvalidationHandler
                 │                          (Redis prefixes)
   load current state → template (locale) → minijinja → EmailSender (SMTP / log)
                 │
           email_log (queued → sending → sent | failed)
```

- **Instant and reliable**: the worker wakes up on `NOTIFY` (a confirmed booking is emailed in well
  under a second) and also polls every `OUTBOX_POLL_SECS` (5 s), so nothing is lost if a
  notification is missed.
- **Any number of workers**: events are claimed with `SKIP LOCKED`; each event is handled by one
  worker at a time (tested with two concurrent workers over 100 events).
- **Where it runs**: inside the API by default (`OUTBOX_WORKER_ENABLED=true`). To scale it
  separately: `OUTBOX_WORKER_ENABLED=false` on the API and `cargo run --bin worker` (same env).
  Both stop on SIGTERM after finishing the batch in progress.

## Event types and payloads

Events are produced only through `services::outbox::Event` (`outbox::enqueue(tx, &Event::...)`,
in the business transaction). **Payloads carry ids, never rendered data**: handlers load the
current state when they run, so an email always shows up-to-date information.

| event_type | payload | produced by |
|---|---|---|
| `booking.created` | `{booking_id}` | `book_accommodation`, `book_activity` |
| `booking.confirmed` | `{booking_id}` | `confirm_booking` |
| `booking.modified` | `{booking_id}` | (booking edits, to come with the admin API) |
| `booking.cancelled` | `{booking_id}` | `cancel_booking` |
| `booking.expired` | `{booking_id}` | hold-expiry job (`expire_holds`) |
| `booking.pre_arrival_due` | `{booking_id}` | email scheduler (future `available_at`) |
| `booking.review_due` | `{booking_id}` | email scheduler (future `available_at`) |
| `payment.succeeded` | `{payment_id, booking_id}` | (payment providers, to come) |
| `inquiry.created` | `{inquiry_id}` | (contact form, to come) |
| `newsletter.subscribed` | `{subscriber_id, secret: {token}}` | (newsletter form, to come) |
| `auth.password_reset_requested` | `{user_id, auth_token_id, secret: {token}}` | (auth, to come) |
| `auth.email_verification_requested` | `{user_id, auth_token_id, secret: {token}}` | (auth, to come) |
| `auth.staff_invited` | `{user_id, auth_token_id, invited_by, secret: {token}}` | (admin, to come) |
| `product.updated` · `settings.updated` · `menu.updated` · `page.updated` | `{product_id}` · `{key}` · `{menu_id}` · `{page_id}` | (admin, to come) |

`secret` is the one exception to "ids only": the raw one-time token of a link, which exists
nowhere else (the database stores its hash). The dispatcher **deletes `secret` from the payload**
as soon as the event is processed or dead. Consequence: replaying such an event after completion
cannot resend the link (the handler fails with a clear message); issue a new token instead.

## Which emails are sent

| event | to the guest | to the team (`notifications.staff_email`) |
|---|---|---|
| booking.created (pending / awaiting payment) | `booking_received` | `staff_new_booking` |
| booking.confirmed | `booking_confirmation` | |
| booking.modified | `booking_modified` | |
| booking.cancelled | `booking_cancelled` | `staff_booking_cancelled` |
| booking.expired | `booking_cancelled` (expired wording) | |
| booking.pre_arrival_due | `pre_arrival` | |
| booking.review_due | `review_request` | |
| payment.succeeded | `payment_receipt` | |
| inquiry.created | `inquiry_auto_reply` | `staff_new_inquiry` |
| newsletter.subscribed | `newsletter_confirm` | |
| auth.* | `password_reset` · `email_verification` · `staff_invitation` | |

Handlers re-check the current state first: an email that no longer makes sense is skipped (a
confirmation for a booking cancelled meanwhile, a pre-arrival email for a cancelled stay, a
token already used or expired, a guest without email address). Staff emails are skipped while
`notifications.staff_email` is empty.

**Language**: `booking.locale` > `customer.preferred_locale` > `user.preferred_locale` > default
locale (first *active* one wins). Staff emails always use the default locale. A missing
translation falls back to the default locale, with a warning in the logs.

**Scheduled emails** (`worker/scheduler.rs`, every 5 min): one `booking.pre_arrival_due` per
confirmed stay, due `email.pre_arrival_days` (3) days before arrival at 09:00 camp time, moved if
the arrival date changes before it is sent; one `booking.review_due` per completed stay,
`email.review_request_days` (2) days after completion at 10:00. `dedupe_key`
(`<event_type>:<booking_id>`, unique) guarantees one event per booking, whatever how often or
where the scheduler runs. Send times are computed in Rust (`camp_time`), never in SQL.

## Templates

Stored in `email_templates` (`code`, `is_active`, `variables`) and
`email_template_translations` (`subject`, `body_html`, `body_text` per locale), seeded by
migration `0014_email_templates` in fr/en/ar. Bodies are [minijinja](https://docs.rs/minijinja)
(Jinja2 syntax), **HTML autoescaped**, wrapped in one shared layout
(`src/services/email/templates/layout.html`: tables + inline CSS for Gmail/Outlook, logo from the
public bucket via the `email.logo_media_id` setting, footer with the contact settings,
`dir="rtl"` / `lang="ar"` and right alignment for Arabic). `body_text` may stay empty: the text
part is then generated from the HTML (every email is multipart HTML + text).

Inside a template:

| | |
|---|---|
| variables | only those listed in `email_templates.variables` (`site` everywhere) |
| always available | `dir` (`ltr`/`rtl`), `lang`, `start` / `end` (`left`/`right`, mirrored in RTL) |
| `money(cents, currency)` | `120,00 €` (fr) · `€120.00` (en) · `120,00 €` (ar, Moroccan format) |
| `date(v)` · `datetime(v)` · `time(v)` | `vendredi 15 janvier 2027` · `… à 08:00` · `08:00`, camp time, localized |
| `button(url, label)` | call-to-action button |
| `{% include "booking_summary.html" %}` | reference, dates, guests, lines with time slots, totals, paid, balance and due date (needs `booking`, `lines`) |
| `{% include "cancellation_policy.html" %}` | the booking's `cancellation_policy_snapshot` as sentences (needs `policy`) |

Variables of the booking emails: `guest` (first_name, full_name, email, phone), `booking`
(reference, status, url, review_url, arrival_date, departure_date, nights, nights_label,
guests_label, currency, *_minor amounts incl. balance_minor and deposit_remaining_minor,
balance_due_on, hold_expires_at, check_in_from, estimated_arrival_time, special_requests,
cancelled_at, cancellation_reason), `lines`, `policy`, `payment_instructions`, `payment`,
`admin_url`. `booking.url` carries an HMAC token (`t`) so the booking page can be opened without
an account.

**Validation**, on save (`templates::save_translation`) and again before every send: syntax, and
every placeholder must be a declared variable (or a helper/partial whose variables are
declared). Rendering is strict: a missing attribute is an error, never an empty string. A render
error is permanent: the event is marked dead at once and an `alert=true` error is logged.

### Add a new email

1. A migration inserting the template and its fr/en/ar translations (copy one block of
   `0014_email_templates.up.sql`), with its `variables`.
2. Sample data in `services/email/samples.rs` (`TEMPLATE_CODES` + `for_template`): previews and
   the "every template renders in every locale" test use it.
3. Send it from a handler (below). Check with `cargo run --bin email -- validate` and
   `cargo run --bin email -- preview <code> <locale> out.html`.

### Add a handler (new reaction to an event)

1. Add the event to `services::outbox::Event` (+ its payload struct) and enqueue it in the
   business transaction.
2. Implement `worker::OutboxHandler` (`event_types()` + `handle()`); return
   `HandlerError::Retry` for temporary failures, `Permanent` when retrying cannot help, `Defer(at)`
   when it is too early. Handlers must be idempotent.
3. Register it in `worker::registry()`. An event type without handler is logged and marked
   processed, never retried forever.

## Providers

| `EMAIL_PROVIDER` | |
|---|---|
| `log` (default) | prints each email (text part) in the logs; nothing is sent |
| `smtp` | lettre over TLS: `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASSWORD`, `SMTP_TLS` = `starttls` (587, default) · `tls` (465) · `none` (local Mailpit only) |

Common: `EMAIL_FROM` (required with smtp), `EMAIL_FROM_NAME` (Mouzoun), `EMAIL_REPLY_TO`,
`PUBLIC_SITE_URL` (links). Locally, `docker compose up -d mailpit` and
`EMAIL_PROVIDER=smtp SMTP_HOST=localhost SMTP_TLS=none` show every email on
http://localhost:8025. An HTTP API provider (Brevo, Postmark, SES...) is one more implementation
of `EmailSender` selected in `sender::from_config`; callers do not change.

For deliverability in production, the sending domain needs SPF, DKIM and DMARC records for the
SMTP provider.

## Retries, dead events and replay

- A temporary failure (SMTP down, database error) retries after 30 s, 2 min, 10 min, 1 h, then
  every 6 h, each ±10 % jitter; after `OUTBOX_MAX_ATTEMPTS` (8) the event is **dead**
  (`failed_at` set, `alert=true` error in the logs). Permanent failures die at once.
- **Never twice**: `email_log` has one row per (event, recipient, template), written in its own
  commits: `queued` → `sending` (just before calling the provider) → `sent`. On retry, `sent`
  is skipped; `sending` means a crash during the send (delivery unknown): it is **not** resent,
  it stays `sending` with an error and an alert.
- Inspect and replay:

```bash
cargo run --bin email -- list-dead                 # dead events with their last error
cargo run --bin email -- retry <event_id>          # back in the queue (attempts reset)
cargo run --bin email -- preview booking_received ar out.html
cargo run --bin email -- send-test booking_confirmation fr me@example.com
cargo run --bin email -- validate                  # every stored translation
```

On the VPS the same tools are in the API image:
`sudo docker exec mouzoun-back-blue /app/email list-dead` (or `-green`, whichever is live).

Replaying never resends what was already `sent`. To force one email again after an uncertain
delivery: `UPDATE email_log SET status = 'failed' WHERE id = …`, then replay its event.
