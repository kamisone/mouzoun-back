DELETE FROM settings WHERE key = 'email.balance_reminder_enabled';
DELETE FROM email_templates WHERE code IN ('booking_declined', 'booking_payment_link', 'balance_reminder', 'balance_failed', 'staff_balance_failed', 'precheckin_reminder', 'booking_magic_link', 'refund_receipt', 'cart_abandoned');
