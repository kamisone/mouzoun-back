-- 0020_schema_corrections: corrections that were first written by editing 0006, 0008 and 0012
-- after those had been applied in production. SQLx refuses to start when an applied migration
-- changes (checksum), so they live here instead. Idempotent: a database built from the edited
-- files already has every change and is left as it is.
--
--   * rate_plans / promotions: one column per unit (percent | amount in minor units | nights)
--     instead of a numeric(12,2) holding a percentage or an amount in major units;
--   * booking references: a per-year counter table instead of a global sequence;
--   * the camp's coordinates (M'ssici).

-- ---- rate_plans.deposit_* -----------------------------------------------------------------------
ALTER TABLE rate_plans
    ADD COLUMN IF NOT EXISTS deposit_percent      numeric(5, 2),
    ADD COLUMN IF NOT EXISTS deposit_amount_minor bigint,
    ADD COLUMN IF NOT EXISTS deposit_currency     char(3) REFERENCES currencies (code) ON UPDATE CASCADE;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.columns
               WHERE table_schema = current_schema() AND table_name = 'rate_plans' AND column_name = 'deposit_value') THEN
        UPDATE rate_plans SET deposit_percent = deposit_value WHERE deposit_type = 'percent';
        UPDATE rate_plans
           SET deposit_amount_minor = round(deposit_value * 100)::bigint,
               deposit_currency = (SELECT code FROM currencies WHERE is_default)
         WHERE deposit_type = 'fixed';
        -- Dropping the column drops the two old CHECK constraints that use it.
        ALTER TABLE rate_plans DROP COLUMN deposit_value;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'rate_plans_deposit_percent_range') THEN
        ALTER TABLE rate_plans
            ADD CONSTRAINT rate_plans_deposit_percent_range CHECK (deposit_percent > 0 AND deposit_percent <= 100),
            ADD CONSTRAINT rate_plans_deposit_amount_positive CHECK (deposit_amount_minor > 0),
            ADD CONSTRAINT rate_plans_deposit_percent_when_percent CHECK ((deposit_type = 'percent') = (deposit_percent IS NOT NULL)),
            ADD CONSTRAINT rate_plans_deposit_amount_when_fixed CHECK ((deposit_type = 'fixed') = (deposit_amount_minor IS NOT NULL)),
            ADD CONSTRAINT rate_plans_deposit_amount_has_currency CHECK ((deposit_amount_minor IS NULL) = (deposit_currency IS NULL));
    END IF;
END $$;

-- ---- promotions.discount_* ----------------------------------------------------------------------
ALTER TABLE promotions
    ADD COLUMN IF NOT EXISTS discount_percent      numeric(5, 2),
    ADD COLUMN IF NOT EXISTS discount_amount_minor bigint,
    ADD COLUMN IF NOT EXISTS discount_free_nights  integer;

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.columns
               WHERE table_schema = current_schema() AND table_name = 'promotions' AND column_name = 'discount_value') THEN
        UPDATE promotions SET discount_percent = discount_value WHERE discount_type = 'percent';
        UPDATE promotions SET discount_amount_minor = round(discount_value * 100)::bigint WHERE discount_type = 'fixed_amount';
        UPDATE promotions SET discount_free_nights = discount_value::integer WHERE discount_type = 'free_nights';
        UPDATE promotions SET currency = (SELECT code FROM currencies WHERE is_default)
         WHERE currency IS NULL AND (discount_amount_minor IS NOT NULL OR min_subtotal_minor IS NOT NULL);
        ALTER TABLE promotions DROP COLUMN discount_value;
    END IF;
END $$;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'promotions_discount_percent_range') THEN
        ALTER TABLE promotions
            ADD CONSTRAINT promotions_discount_percent_range CHECK (discount_percent > 0 AND discount_percent <= 100),
            ADD CONSTRAINT promotions_discount_amount_positive CHECK (discount_amount_minor > 0),
            ADD CONSTRAINT promotions_discount_nights_positive CHECK (discount_free_nights > 0),
            ADD CONSTRAINT promotions_percent_when_percent CHECK ((discount_type = 'percent') = (discount_percent IS NOT NULL)),
            ADD CONSTRAINT promotions_amount_when_fixed CHECK ((discount_type = 'fixed_amount') = (discount_amount_minor IS NOT NULL)),
            ADD CONSTRAINT promotions_nights_when_free_nights CHECK ((discount_type = 'free_nights') = (discount_free_nights IS NOT NULL)),
            ADD CONSTRAINT promotions_amounts_have_currency CHECK ((discount_amount_minor IS NULL AND min_subtotal_minor IS NULL) OR currency IS NOT NULL);
    END IF;
END $$;

-- ---- Booking references: MZ-<yy>-<number>, restarting every year ---------------------------------
CREATE TABLE IF NOT EXISTS booking_reference_counters (
    year        smallint PRIMARY KEY CHECK (year BETWEEN 2000 AND 2999),
    last_number integer NOT NULL DEFAULT 0 CHECK (last_number >= 0),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now()
);
DROP SEQUENCE IF EXISTS booking_reference_seq;
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'booking_reference_counters_set_updated_at') THEN
        CREATE TRIGGER booking_reference_counters_set_updated_at BEFORE UPDATE ON booking_reference_counters
            FOR EACH ROW EXECUTE FUNCTION set_updated_at();
    END IF;
END $$;

-- ---- Camp coordinates (only when still unset) ------------------------------------------------
UPDATE settings SET value = '31.238933', description = 'Camp latitude (WGS84), M''ssici'
 WHERE key = 'location.latitude' AND value = 'null'::jsonb;
UPDATE settings SET value = '-4.701349', description = 'Camp longitude (WGS84), M''ssici'
 WHERE key = 'location.longitude' AND value = 'null'::jsonb;
