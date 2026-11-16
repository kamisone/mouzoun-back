-- Staff two-factor sign-in becomes a personal choice: each staff member turns it on (and off)
-- in the back-office settings. Sign-in asks for a code only when it is on, so the global
-- "required" switch no longer applies.
DELETE FROM settings WHERE key = 'auth.staff_2fa_required';
