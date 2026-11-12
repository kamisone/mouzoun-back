-- 0021: online payment is available exactly when a provider is configured (Stripe secrets in
-- the environment), so these two settings never had an effect. Removed to avoid a switch that
-- does nothing. Opening booking itself stays `booking.enabled` (default false), turned on from
-- the back office once the go-live checklist passes.
DELETE FROM settings WHERE key IN ('payments.online_enabled', 'payments.providers');
