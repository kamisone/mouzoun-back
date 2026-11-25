-- 0034_legal_launch: what the shop's compliance checklist needs before the owner opens the shop.
--
--  - legal pages carry a lawyer review status (the checklist warns while any is `draft`, and
--    opening the shop then needs an explicit confirmation);
--  - Stripe webhook events from other sites sharing the Stripe account are kept as
--    `ignored_foreign` (never processed);
--  - placeholder shipping rates (seeded so the screens work) are never offered at checkout and
--    do not count for the checklist until an admin confirms them;
--  - settings the legal pages render: who pays withdrawal returns, the CNDP receipt (Morocco).

ALTER TABLE pages
    ADD COLUMN legal_review_status text NOT NULL DEFAULT 'draft' CHECK (legal_review_status IN ('draft', 'reviewed')),
    ADD COLUMN reviewed_at timestamptz,
    ADD COLUMN reviewed_by uuid REFERENCES users (id) ON DELETE SET NULL;

ALTER TABLE payment_webhook_events
    ADD COLUMN status text NOT NULL DEFAULT 'received' CHECK (status IN ('received', 'ignored_foreign'));

ALTER TABLE shipping_rates ADD COLUMN is_placeholder boolean NOT NULL DEFAULT false;

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('shop.return_costs_paid_by', 'shop', 'string', '"customer"', true,
     'Who pays the return of goods after a withdrawal: customer | seller (shown in the legal pages)'),
    ('legal.cndp_receipt', 'legal', 'string', '""', false,
     'Receipt number of the declaration to the Moroccan CNDP (law 09-08), shown in the privacy policy');
