-- Back to the 0006 / 0008 / 0012 shapes as first applied.
UPDATE settings SET value = 'null'::jsonb, description = 'Camp latitude (WGS84)' WHERE key = 'location.latitude';
UPDATE settings SET value = 'null'::jsonb, description = 'Camp longitude (WGS84)' WHERE key = 'location.longitude';

DROP TABLE IF EXISTS booking_reference_counters;
CREATE SEQUENCE IF NOT EXISTS booking_reference_seq;

ALTER TABLE promotions ADD COLUMN discount_value numeric(12, 2);
UPDATE promotions SET discount_value = COALESCE(discount_percent, discount_amount_minor / 100.0, discount_free_nights);
ALTER TABLE promotions
    DROP COLUMN discount_percent, DROP COLUMN discount_amount_minor, DROP COLUMN discount_free_nights,
    DROP CONSTRAINT IF EXISTS promotions_amounts_have_currency,
    ALTER COLUMN discount_value SET NOT NULL,
    ADD CHECK (discount_value > 0),
    ADD CHECK (discount_type <> 'percent' OR discount_value <= 100),
    ADD CHECK (discount_type <> 'fixed_amount' OR currency IS NOT NULL);

ALTER TABLE rate_plans ADD COLUMN deposit_value numeric(12, 2) CHECK (deposit_value >= 0);
UPDATE rate_plans SET deposit_value = COALESCE(deposit_percent, deposit_amount_minor / 100.0);
ALTER TABLE rate_plans
    DROP COLUMN deposit_percent, DROP COLUMN deposit_amount_minor, DROP COLUMN deposit_currency,
    ADD CHECK (deposit_type IN ('none', 'full') OR deposit_value IS NOT NULL),
    ADD CHECK (deposit_type <> 'percent' OR deposit_value <= 100);
