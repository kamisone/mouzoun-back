DELETE FROM staff_tasks WHERE kind = 'dispute';
ALTER TABLE staff_tasks DROP CONSTRAINT staff_tasks_kind_check;
ALTER TABLE staff_tasks ADD CONSTRAINT staff_tasks_kind_check CHECK (kind IN ('refund_offline', 'balance_failed',
    'no_show_review', 'change_request', 'payment_after_expiry', 'webhook_error', 'other'));
