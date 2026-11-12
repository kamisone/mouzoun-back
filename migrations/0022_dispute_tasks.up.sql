-- 0022: a staff task when a guest disputes a card payment (chargeback): evidence must be sent
-- to the payment provider before its deadline.
ALTER TABLE staff_tasks DROP CONSTRAINT staff_tasks_kind_check;
ALTER TABLE staff_tasks ADD CONSTRAINT staff_tasks_kind_check CHECK (kind IN ('refund_offline', 'balance_failed',
    'no_show_review', 'change_request', 'payment_after_expiry', 'webhook_error', 'dispute', 'other'));
