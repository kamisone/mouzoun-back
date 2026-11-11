DELETE FROM email_templates WHERE code IN (
    'booking_received', 'booking_confirmation', 'booking_cancelled', 'booking_modified', 'payment_receipt',
    'pre_arrival', 'review_request', 'inquiry_auto_reply', 'newsletter_confirm', 'password_reset',
    'email_verification', 'staff_invitation', 'staff_new_booking', 'staff_booking_cancelled', 'staff_new_inquiry'
);
DELETE FROM settings WHERE key IN (
    'notifications.staff_email', 'email.pre_arrival_days', 'email.review_request_days', 'email.logo_media_id',
    'booking.payment_instructions'
);
