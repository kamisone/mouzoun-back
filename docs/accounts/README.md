# Guest accounts

Guests can book without an account. An account adds: every booking in one place (the same
booking pages as the email links), an offline QR ticket, wishlists, saved travellers and cards,
faster checkout, and self-service privacy. Staff use a separate login with a second factor.

Code: `src/services/accounts` (sign-in, sessions, OIDC, passkeys, tickets, privacy),
`src/modules/account` (API `/v1/account`), `src/modules/booking_admin/customers.rs` (CRM),
`src/auth/{guest,mfa}.rs`, `src/jobs/accounts.rs`, migrations `0017`–`0019`. Front:
`front/app/pages/account/**`, `front/app/components/account`, `front/app/composables/useAccount*.ts`,
`front/app/pages/admin/{customers,scan,login}.vue`.

## Sign in or create an account

One flow for both; the answer never says whether an address has an account.

```
                 ┌─ Google / Apple ─► /oauth/{p}/start ─► provider ─► /oauth/{p}/callback ─┐
email field ─────┤                    (state, nonce, PKCE in Redis 10 min)                  │
 (autofill offers├─ passkey ─────────► /auth/passkey/options ─► navigator.credentials.get ──┤
  passkeys)      │                                                                          ├─► session
                 └─ Continue ─► /auth/identify ─► email: 6-digit code + link ─► /auth/code ─┤   cookies
                                 (same answer for every address)       or /auth/link ─────────┤
                                                    or password ─► /auth/password ───────────┘
                                                                         │
                         new account? ─► "What's your name?" (+ unticked offers consent)
```

- Code: 10 minutes, 5 wrong tries burn it (the account itself is never locked), resend after
  30 s. Kept in Redis (`acct:code:<sha256(email)>`, code and link hashed) because the address may
  not have an account yet: **no user row exists for an unverified address**.
- Magic link: same email, single use, needs one click on `/account/signin/link` (mail scanners
  open links on their own).
- Password: optional, ≥ 10 characters, checked against Have I Been Pwned with k-anonymity
  (5 hex characters leave; fails open). Wrong email or password give the same answer and timing.
- Rate limits: per IP and per address (identify 20/15 min per IP, 5/15 min per address,
  silently), Turnstile when configured.
- Google / Apple: authorization code + PKCE (S256), `state` single use (GETDEL), `nonce` checked
  in the ID token, which is verified against the provider JWKS (cached 1 h): signature, issuer,
  audience, expiry. Apple's client secret is an ES256 JWT signed with the team key.
- An identity is linked to an account **only through an email the provider marks verified**,
  or from "Login & security" by a signed-in user who confirmed it's them. A provider subject
  belongs to one account.
- Passkeys: WebAuthn, attestation `none`, ES256 only, discoverable credentials (username-less
  and browser autofill / conditional UI). The browser's JSON form carries the public key as
  SubjectPublicKeyInfo, so no CBOR parsing is needed; signatures are checked with `ring`.
  Challenge single use (Redis, 5 min), origin and RP id hash checked, a signature counter going
  backwards (cloned key) refuses the sign-in.
- Email codes always work: removing a password or unlinking a provider can never lock anyone out.

## Accounts and guest bookings

Bookings belong to `customers`; an account (`users`, `is_staff = false`) owns one customer.

- On sign-in with a verified address the account takes the customer record of that address
  (if not owned yet) — and with it every booking made as a guest with it. Names known from a
  booking fill the account.
- Never by an unverified address: a booking made with another address is added with **"Add a
  booking to my account"**: reference + that address → a link is emailed **to the booking's
  address** (same answer whether it exists) → opening it while signed in moves the booking to
  the account's customer (audited, `booking.claimed`).
- Changing the account email merges the unowned customer record of the new (verified) address.
- Booking pages (`/booking/{reference}`) accept the owner's session instead of the email token.
  Anyone else's booking answers **404**, not 403.
- Basket: the anonymous basket joins the account's basket at sign-in (no duplicates).
- Checkout: signed-in guests are prefilled and the booking goes to their customer record;
  guests can tick "Create an account" (a sign-in link to the booking address, nothing more),
  and the confirmation page offers the same in one tap. Nothing reveals whether an address
  already has an account.

## Sessions and tokens

| | Where | Lifetime |
|---|---|---|
| Access token | JWT in `__Host-mz_at` (HttpOnly, Secure, SameSite=Lax), carries the session id; separate HMAC key from staff tokens | 15 min |
| Refresh token | random, `__Host-mz_rt`, stored hashed in `user_sessions` | 60 days, or 12 h and no `Max-Age` on a shared device |
| Staff session | unchanged staff JWT cookie, issued only after the second factor | `JWT_TTL_MINUTES` |

- Every request checks the session is still active: "sign out everywhere" and suspension are
  immediate. An expired access token is refreshed transparently by middleware.
- Rotation: each refresh replaces the token and keeps the previous hash. Presenting a rotated
  token again after a 10 s grace (two tabs) revokes the session (theft detection, audited).
- CSRF: SameSite=Lax cookies, JSON bodies only, CORS limited to the site origins.
- The front never calls the account API during server rendering (`/account/**` is
  client-rendered) and never forwards the refresh cookie from SSR: a rotation there could not
  reach the browser.
- Sensitive actions (email, password, passkeys, providers, ID numbers, export, deletion) need a
  confirmation on this session within 10 minutes (`403 reauthentication_required` → the front
  asks for a code, the password or a passkey, then retries).
- Guests never reach `/v1/admin` (guest tokens are refused by the staff extractor; staff login
  refuses non-staff). Staff need a password **and** TOTP (RFC 6238, ±30 s, replay-protected;
  enrolment with a QR code on first login; setting `auth.staff_2fa_required`).

## Security emails and events

Outbox events (secrets under `secret`, scrubbed once processed) and their emails, in fr/en/ar
(migration 0018):

| Event | Email |
|---|---|
| `auth.login_code` | sign-in code + link |
| `account.created` | welcome |
| `auth.new_device` | sign-in from a new device |
| `auth.password_changed` | password added / changed / removed |
| `auth.email_change_requested` | code to the new address |
| `auth.email_changed` | notice to the old address |
| `auth.passkey_added`, `auth.identity_linked` | security notices |
| `booking.claim_requested`, `booking.claimed` | claim link / done |
| `account.export_ready` | download link (7 days) |
| `account.deletion_scheduled`, `account.deleted` | cancel link / goodbye |

## Privacy

- **Export**: requested from the account (one per day), built by the `accounts` job, emailed
  as a link valid 7 days; downloading needs the same signed-in account. JSON of the account,
  customer, bookings (guests without ID numbers, payments, invoice numbers), reviews,
  travellers, wishlists, preferences, sign-in methods and sessions.
- **Deletion**: refused while a stay is upcoming or a balance is unpaid; otherwise scheduled
  after `account.deletion_grace_days` (14), cancellable from the account or the email link.
  When due, the account is **anonymised**: contact details, identities, travellers, wishlists,
  preferences, sessions and newsletter subscription go; bookings, payments and invoices stay for
  accounting (invoices keep their billing details); the guest register keeps its own legal
  retention. The address can create a new account afterwards.
- Saved ID numbers: only with explicit consent, AES-GCM encrypted (`GUEST_DATA_KEY`), shown again
  only after confirming it's you (audited).

## Back office (CRM)

`/admin/customers`: search, with / without account, report (sign-ups, share of bookings made
with an account, sign-in methods, most wished products). Customer page: details, account,
bookings, history, and actions — sign-in link, suspend / reactivate, sign out everywhere
(`customers.write`), merge a duplicate (`customers.merge`; two accounts are never merged), view
as the customer (read-only, no session created, `customers.impersonate`), export and deletion
(`customers.privacy`). `/admin/scan` reads a guest's QR ticket (`bookings.read`) and checks
them in. Every action is audited.

QR tickets: `MZT1.<booking>.<expiry>.<HMAC>` — no personal data, valid until the day after
departure. The ticket page keeps a copy on the device and a small service worker (`/sw.js`)
caches the app shell so it opens without signal.

## Jobs

`accounts` (every 15 min): build pending exports, carry out due deletions, delete expired
sessions / tokens, drop expired export data.

## Configuring locally

```
HIBP_ENABLED=true
API_PUBLIC_URL=http://localhost:8080          # OAuth redirect URIs: <this>/v1/account/oauth/<provider>/callback
WEBAUTHN_RP_ID=localhost
WEBAUTHN_ORIGIN=http://localhost:3001         # exact site origin
PUBLIC_SITE_URL=http://localhost:3001         # where callbacks send the browser back
```

**Google**: Google Cloud console → APIs & Services → Credentials → OAuth client ID (Web).
Authorised redirect URI `http://localhost:8080/v1/account/oauth/google/callback` (and the
production one). Set `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET`. The button appears only when
configured.

**Apple**: Apple Developer → Identifiers → a Services ID (`APPLE_CLIENT_ID`), "Sign in with
Apple" with your domain and return URL `https://<domain>/api/v1/account/oauth/apple/callback`
(Apple refuses `localhost`: use a tunnel such as `cloudflared` and set `API_PUBLIC_URL` to it);
Keys → a key with Sign in with Apple (`APPLE_KEY_ID`, the `.p8` content in `APPLE_PRIVATE_KEY`
with `\n` for newlines); `APPLE_TEAM_ID`. Apple answers with a form POST.

**Passkeys**: work on `http://localhost` (secure context). The RP id must be the site's domain
(`localhost`, then `mouzoun.com` in production, `WEBAUTHN_ORIGIN=https://mouzoun.com`). Test
without a device in Chrome DevTools → WebAuthn → "Enable virtual authenticator environment".

All secrets go in GitHub secrets for deployment, one per value.

## Tests

`cargo test --test it accounts:: oidc:: passkeys:: crm::` (rotation and reuse,
enumeration, single-use codes, claims, ownership 404s, wishlists, travellers, cards, export and
deletion, tickets, OIDC token checks with a local key, passkeys with a software authenticator,
CRM permissions). Front: `npm run test:e2e` (`tests/e2e/accounts.spec.ts`: code sign-in with a
wrong then pasted code, RTL, account pages, staff 2FA, axe on desktop and phone);
`tests/fullstack/staff.ts` signs staff in with TOTP against a real API.

## Decisions (November 2026)

- Staff sign in with password + authenticator app only; no staff passkeys for now.
- Paying with a saved card at checkout: later, once real bookings come in. Saved cards serve
  the deposit balance and the account's "Payment methods" page.
- No Facebook sign-in (`account.facebook_enabled` stays false). Phone checks by WhatsApp / SMS
  can wait: email codes only.
- `customers.merge`, `customers.impersonate` and `customers.privacy` stay with super admin and
  manager only; reception keeps `customers.read` / `customers.write`.
