# Personal data of the shop (GDPR, CNIL)

## Cookies

- Banner (`MzCookieBanner`): "Tout refuser" and "Tout accepter" are identical buttons; no
  measurement before consent; the choice is stored in local storage with its date and asked
  again after 6 months (`utils/consent.ts`); "Gérer les cookies" in the footer reopens it;
  refusing empties the measurement queue. Cart and session cookies are strictly necessary.

## Marketing

- Separate, unticked checkbox at checkout; the value is copied on the order
  (`marketing_consent_snapshot`) and sets `customers.marketing_consent`.
- Abandoned-cart emails only with consent (`shop.abandoned_cart_requires_consent`).

## Retention (`privacy.data_retention`, daily job `shop retention`)

| data | duration | action |
|---|---|---|
| carts not converted | 30 days | deleted |
| back-in-stock requests | 3 years | deleted |
| marketing consent without order or booking | 3 years | withdrawn |
| orders, invoices, payments | 10 years (Commercial Code L123-22) | kept |

## Rights

- Export (account › privacy export, `services/accounts/privacy.rs`): now includes shop orders (lines, addresses, consents,
  invoices, returns) and back-in-stock requests.
- Erasure: blocked while an order is open; otherwise the account is anonymised and shop orders
  lose email, phone, delivery address, relay point, notes, IP and browser; invoices keep the
  billing details they were issued with (accounting duty); carts and back-in-stock requests are
  deleted. Logged in the audit log (`account.deleted`).

## Processors (to list in the privacy policy)

| processor | purpose | location |
|---|---|---|
| Google Cloud (Cloud SQL, Storage, Run) | hosting, invoices PDFs | EU region to use (europe-west9 Paris / europe-west1): check the project's region |
| Stripe | card payments, fraud | EU entity, transfers under SCCs |
| Colissimo (La Poste), Mondial Relay, Chronopost, DPD | delivery | France / EU |
| Email provider (SMTP) | transactional emails | to state |

Data of the French shop should stay in an EU region; the current bucket and database region
must be checked before opening.
