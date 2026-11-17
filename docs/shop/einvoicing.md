# E-invoicing and e-reporting (France)

The shop sells to consumers (B2C). French B2C sales are not e-invoiced but fall under
**e-reporting** (transaction and payment data sent to the tax administration through an
approved platform, "plateforme agréée" PA, formerly PDP), on the reform calendar (receiving
from September 2026, issuing / reporting from 2026–2027 depending on company size): **check the
current dates with the accountant**.

## What exists

- Every invoice / credit note: `legal_entity_id`, gap-free `number` per entity and year,
  `seller_snapshot`, `billing_details`, `lines`, `vat_breakdown` (per rate), `delivery_date`,
  `credit_note_for_invoice_id`, `format_version` (2), immutable once issued.
- `einvoice_status` = `b2c_ereporting` on shop documents (nothing transmitted yet).
- Sealed PDF in the bucket (`pdf_object_key`, `pdf_sha256`).
- Sales journal export per day / rate / country, the data e-reporting aggregates.

## Out of scope (not built)

- Factur-X (PDF/A-3 + CII XML) generation.
- Connection to a PA (API, statuses, lifecycle messages).
- B2B: reverse charge, VIES check of VAT numbers, SIREN of the buyer.

## To add later

1. Choose a PA with the accountant; map `invoices` + `vat_breakdown` to its B2C e-reporting
   format (daily aggregated amounts per rate) and payment data (`payments`).
2. A job sending pending documents, updating `einvoice_status` (`sent`, `accepted`,
   `rejected`), keeping the PA's identifiers.
3. If B2B sales start: buyer SIREN / VAT number on orders, Factur-X generation, reverse charge.
