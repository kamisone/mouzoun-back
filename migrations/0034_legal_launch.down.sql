DELETE FROM settings WHERE key IN ('shop.return_costs_paid_by', 'legal.cndp_receipt');
ALTER TABLE shipping_rates DROP COLUMN is_placeholder;
ALTER TABLE payment_webhook_events DROP COLUMN status;
ALTER TABLE pages DROP COLUMN reviewed_by, DROP COLUMN reviewed_at, DROP COLUMN legal_review_status;
