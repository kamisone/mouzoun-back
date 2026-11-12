UPDATE settings SET value = '30' WHERE key = 'booking.hold_minutes' AND value = '15'::jsonb;
DELETE FROM settings WHERE key IN ('booking.request_answer_hours', 'booking.payment_link_hours',
    'booking.balance_due_days', 'booking.balance_auto_charge', 'booking.balance_reminder_days',
    'booking.balance_grace_days', 'booking.standalone_activities', 'booking.calendar_show_prices',
    'booking.urgency_threshold', 'booking.self_service_changes', 'booking.guest_cancellation',
    'booking.request_card_authorization', 'booking.precheckin_reminder_days', 'booking.magic_link_minutes',
    'booking.pitch_selection', 'booking.id_retention_days', 'legal.terms_version');

DELETE FROM role_permissions WHERE permission_code IN ('bookings.cancel', 'bookings.override_price',
    'payments.refund', 'payments.record_offline', 'rates.write', 'guests.read_identity', 'reports.read',
    'activities.manifest');
DELETE FROM permissions WHERE code IN ('bookings.cancel', 'bookings.override_price', 'payments.refund',
    'payments.record_offline', 'rates.write', 'guests.read_identity', 'reports.read', 'activities.manifest');

DROP TABLE price_observations;
ALTER TABLE activities DROP COLUMN standalone_bookable;
ALTER TABLE accommodation_types DROP COLUMN pitch_selection_extra_id;
ALTER TABLE accommodation_units DROP COLUMN map_x, DROP COLUMN map_y;
ALTER TABLE refunds DROP COLUMN idempotency_key, DROP COLUMN failure_reason, DROP COLUMN processed_at;

DROP TABLE staff_tasks;
DROP TABLE booking_notes;
DROP TABLE booking_change_requests;
DROP TABLE booking_access_tokens;

ALTER TABLE booking_items
    DROP COLUMN guide_language, DROP COLUMN requested_time, DROP COLUMN requested_unit_id,
    DROP COLUMN cancelled_at;

DROP INDEX bookings_created_idx;
DROP INDEX bookings_balance_due_idx;
ALTER TABLE bookings
    DROP COLUMN payment_option, DROP COLUMN balance_collection, DROP COLUMN saved_payment_method_id,
    DROP COLUMN balance_consent_at, DROP COLUMN terms_version, DROP COLUMN terms_accepted_at,
    DROP COLUMN checked_in_at, DROP COLUMN checked_out_at, DROP COLUMN precheckin_completed_at,
    DROP COLUMN skip_guest_emails, DROP COLUMN checkout_request_id;

ALTER TABLE customers DROP COLUMN stripe_customer_id, DROP COLUMN whatsapp_preferred, DROP COLUMN anonymized_at;
