# Legal launch of the shop (France)

What is in place so the compliance checklist (Admin › Boutique › France) can pass, and what only
the owner can do. **The shop stays in `catalog_only`: opening it is the owner's decision.**

## How it works

| Area | Where |
|---|---|
| **Company facts, edited in the admin.** Admin › Settings › Legal entity edits the `mouzoun_fr` row (legal name, form, capital, address, country, email, phone, SIREN, SIRET, RCS city, VAT number, APE code, publication director) and the consumer mediator (name, URL, address). SIREN (9 digits, Luhn), SIRET (14 digits, starts with the SIREN, Luhn), the French VAT key and the postal code are checked; errors show next to each field (fr/en/ar). Saving writes an audit row naming the changed fields (no values) and drops the cached legal pages. Only staff with `settings.write`. Issued invoices keep their `seller_snapshot`. No environment variable holds these facts. | `src/services/shop/legal_entity.rs`, `src/modules/shop/admin/legal_entity.rs`, `front/app/pages/admin/settings/legal-entity.vue` |
| **Legal pages, written in the builder.** Six pages (CGV, mentions légales, confidentialité, cookies, rétractation, livraison) in fr/en/ar, created from `legal/pages/*.html` by `mouzoun-cli legal-pages install` (also by `seed_dev`), then edited freely in the builder and published like any page. | `legal/pages/`, `src/services/shop/launch.rs` |
| **Tokens.** `{{seller.*}}`, `{{mediator.*}}`, `{{host.*}}`, `{{shop.*}}`… are filled at render time from the data above, so a company change updates every page. The builder's text editor has **"Insérer un champ société"** to insert one without remembering its name. Publishing a legal page with an unknown token fails (`unknown_tokens`). Generated blocks: withdrawal exemptions, model withdrawal form and PDF, delivery table and customs notice, cookie table. | `src/services/shop/legal_pages.rs`, `GET /v1/admin/pages/legal-tokens` |
| **Checklist.** "Seller identity" and "Consumer mediator" list the missing fields and link to the form; "Stripe webhook", "Food information", placeholder delivery rates counted apart. Opening is blocked only by these lines. | `src/modules/shop/admin/france.rs` |
| **Stripe** shared with vitecamio (tags, foreign events ignored, statement descriptor). Keys stay in the environment. | [stripe.md](stripe.md) |
| **Placeholder delivery rates** for mainland France (`mouzoun-cli shipping seed-placeholders`): never offered until saved or confirmed. | Admin › Boutique › Livraison |

Set-up commands (idempotent):

```sh
cargo run --bin mouzoun-cli -- legal-pages install
cargo run --bin mouzoun-cli -- shipping seed-placeholders
# once, on databases installed with the first templates: removes their reviewer note paragraphs
cargo run --bin mouzoun-cli -- legal-pages remove-notes
```

## Owner to-do before opening

1. **Legal entity and mediator.** Fill Admin › Settings › Legal entity: every field the checklist
   lists (phone, APE code, publication director, real legal form and capital…) and the consumer
   mediator you joined.
2. **Legal pages.** Write or adjust the six pages in the builder (insert company fields with
   "Insérer un champ société"), then publish them.
3. **Shipping rates.** Enter the carrier contract prices (open or save each placeholder rate).
4. **Stripe.** Add Mouzoun's own webhook endpoint and paste its signing secret into
   `STRIPE_WEBHOOK_SECRET`; add the Mouzoun domain to the business websites ([stripe.md](stripe.md)).
5. **Checks with the authorities.** Confirm the mediator covers online food sales, and check the
   APE code and the food rules with the DDPP.
6. **CNDP and host.** Obtain the CNDP receipt number (Morocco, law 09-08; setting
   `legal.cndp_receipt`) and confirm the hosting entity and address from the Google Cloud contract
   (Admin › Boutique › France, entity "host").
7. **Open.** When every checklist line is green, switch Réglages › Statut to "Ouverte".

## Not covered here

- Bookings (stays, activities) are sold by `mouzoun_ma` (`bookings.legal_entity`): the CGV exclude
  them and the withdrawal page lists no dated-service exception.
- No carrier API, no change on vitecamio, nothing created in the Stripe Dashboard.
