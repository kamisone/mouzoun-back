-- 0014_email_templates: email settings and the transactional email templates (fr/en/ar).
-- Template bodies are minijinja: see docs/email/README.md for the variables and helpers.
-- body_text is left empty: the text version is generated from the HTML (an admin may override it).
-- Generated content: edit templates in the admin, not here (this only seeds new databases).

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('notifications.staff_email', 'notifications', 'string', '""', false, 'Address that receives staff notifications (new booking, cancellation, inquiry)'),
    ('email.pre_arrival_days', 'email', 'number', '3', false, 'Days before arrival the pre-arrival email is sent'),
    ('email.review_request_days', 'email', 'number', '2', false, 'Days after the stay the review request is sent'),
    ('email.logo_media_id', 'email', 'media_id', 'null', false, 'Logo shown at the top of emails (public media); empty = site name'),
    ('booking.payment_instructions', 'booking', 'localized_text', '{"fr": "", "en": "", "ar": ""}', true, 'How to pay the deposit (shown in the booking request email)');

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_received', ARRAY['site', 'guest', 'booking', 'lines', 'policy', 'payment_instructions']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Nous avons bien reçu votre demande de réservation {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Merci {{ guest.first_name }} !</h1>
<p>Nous avons bien reçu votre demande de réservation au camp de M'ssici. Elle n'est pas encore confirmée : voici la suite.</p>
{% if booking.deposit_remaining_minor > 0 %}
<p><strong>Pour confirmer votre séjour, merci de régler un acompte de {{ money(booking.deposit_remaining_minor, booking.currency) }}</strong>{% if booking.hold_expires_at %} avant le {{ datetime(booking.hold_expires_at) }}{% endif %}. Passé ce délai, les places que nous vous réservons seront libérées.</p>
{% if payment_instructions %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{{ payment_instructions }}</p>{% endif %}
{% else %}
<p>Notre équipe vérifie les disponibilités et vous confirme votre séjour très rapidement{% if booking.hold_expires_at %}, au plus tard le {{ datetime(booking.hold_expires_at) }}{% endif %}.</p>
{% endif %}
{% include "booking_summary.html" %}
{% include "cancellation_policy.html" %}
{{ button(booking.url, "Voir ma réservation") }}
<p>Une question ? Répondez simplement à cet e-mail{% if site.whatsapp %} ou écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$We have received your booking request {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Thank you, {{ guest.first_name }}!</h1>
<p>We have received your booking request for the camp in M'ssici. It is not confirmed yet; here is what happens next.</p>
{% if booking.deposit_remaining_minor > 0 %}
<p><strong>To confirm your stay, please pay a deposit of {{ money(booking.deposit_remaining_minor, booking.currency) }}</strong>{% if booking.hold_expires_at %} by {{ datetime(booking.hold_expires_at) }}{% endif %}. After that, the places we are holding for you will be released.</p>
{% if payment_instructions %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{{ payment_instructions }}</p>{% endif %}
{% else %}
<p>Our team is checking availability and will confirm your stay very shortly{% if booking.hold_expires_at %}, by {{ datetime(booking.hold_expires_at) }} at the latest{% endif %}.</p>
{% endif %}
{% include "booking_summary.html" %}
{% include "cancellation_policy.html" %}
{{ button(booking.url, "View my booking") }}
<p>Any questions? Just reply to this email{% if site.whatsapp %} or message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$لقد استلمنا طلب الحجز {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">شكرًا لك يا {{ guest.first_name }}!</h1>
<p>لقد استلمنا طلب حجزك في مخيم مسيسي. لم يتم تأكيد الحجز بعد، وإليك الخطوات التالية.</p>
{% if booking.deposit_remaining_minor > 0 %}
<p><strong>لتأكيد إقامتك، يُرجى دفع عربون قدره {{ money(booking.deposit_remaining_minor, booking.currency) }}</strong>{% if booking.hold_expires_at %} قبل {{ datetime(booking.hold_expires_at) }}{% endif %}. بعد هذا الموعد، سيتم تحرير الأماكن المحجوزة لك.</p>
{% if payment_instructions %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{{ payment_instructions }}</p>{% endif %}
{% else %}
<p>يتحقق فريقنا من التوفر وسيؤكد إقامتك في أقرب وقت{% if booking.hold_expires_at %}، في موعد أقصاه {{ datetime(booking.hold_expires_at) }}{% endif %}.</p>
{% endif %}
{% include "booking_summary.html" %}
{% include "cancellation_policy.html" %}
{{ button(booking.url, "عرض حجزي") }}
<p>هل لديك سؤال؟ يكفي الرد على هذه الرسالة{% if site.whatsapp %} أو مراسلتنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_confirmation', ARRAY['site', 'guest', 'booking', 'lines', 'policy']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre séjour est confirmé — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">C'est confirmé, {{ guest.first_name }} !</h1>
<p>Bonne nouvelle : votre réservation est confirmée. Nous avons hâte de vous accueillir à M'ssici{% if booking.arrival_date %} le {{ date(booking.arrival_date) }}{% endif %}.</p>
{% include "booking_summary.html" %}
{% if booking.balance_minor > 0 %}<p>Le solde de <strong>{{ money(booking.balance_minor, booking.currency) }}</strong> est à régler {% if booking.balance_due_on %}avant le {{ date(booking.balance_due_on) }}{% else %}à votre arrivée{% endif %}.</p>{% endif %}
<p>{% if booking.check_in_from %}L'accueil se fait à partir de {{ booking.check_in_from }}. {% endif %}Quelques jours avant votre arrivée, nous vous enverrons l'itinéraire et nos conseils pour préparer votre séjour.</p>
{% include "cancellation_policy.html" %}
{{ button(booking.url, "Gérer ma réservation") }}
<p>À très bientôt,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your stay is confirmed — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">You're all set, {{ guest.first_name }}!</h1>
<p>Good news: your booking is confirmed. We look forward to welcoming you to M'ssici{% if booking.arrival_date %} on {{ date(booking.arrival_date) }}{% endif %}.</p>
{% include "booking_summary.html" %}
{% if booking.balance_minor > 0 %}<p>The balance of <strong>{{ money(booking.balance_minor, booking.currency) }}</strong> is due {% if booking.balance_due_on %}by {{ date(booking.balance_due_on) }}{% else %}on arrival{% endif %}.</p>{% endif %}
<p>{% if booking.check_in_from %}Check-in is from {{ booking.check_in_from }}. {% endif %}A few days before you arrive, we will send you directions and our tips to prepare your stay.</p>
{% include "cancellation_policy.html" %}
{{ button(booking.url, "Manage my booking") }}
<p>See you soon,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تم تأكيد إقامتك — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تم التأكيد يا {{ guest.first_name }}!</h1>
<p>خبر سار: لقد تم تأكيد حجزك. نتطلع إلى استقبالك في مسيسي{% if booking.arrival_date %} يوم {{ date(booking.arrival_date) }}{% endif %}.</p>
{% include "booking_summary.html" %}
{% if booking.balance_minor > 0 %}<p>يُدفع المبلغ المتبقي وقدره <strong>{{ money(booking.balance_minor, booking.currency) }}</strong> {% if booking.balance_due_on %}قبل {{ date(booking.balance_due_on) }}{% else %}عند الوصول{% endif %}.</p>{% endif %}
<p>{% if booking.check_in_from %}يبدأ الاستقبال من الساعة {{ booking.check_in_from }}. {% endif %}قبل وصولك ببضعة أيام، سنرسل إليك مسار الطريق ونصائحنا للتحضير لإقامتك.</p>
{% include "cancellation_policy.html" %}
{{ button(booking.url, "إدارة حجزي") }}
<p>إلى اللقاء قريبًا،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_cancelled', ARRAY['site', 'guest', 'booking', 'lines']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl${% if booking.status == 'expired' %}Votre demande de réservation {{ booking.reference }} a expiré{% else %}Annulation de votre réservation {{ booking.reference }}{% endif %}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bonjour {{ guest.first_name }},</h1>
{% if booking.status == "expired" %}
<p>L'acompte n'ayant pas été reçu dans les délais, votre demande de réservation {{ booking.reference }} a expiré et les places retenues ont été libérées.</p>
<p>Vous souhaitez toujours venir ? Il suffit de refaire une réservation sur notre site ou de nous contacter : nous ferons notre possible pour vous trouver une place.</p>
{% else %}
<p>Nous vous confirmons l'annulation de votre réservation {{ booking.reference }}{% if booking.cancelled_at %} le {{ date(booking.cancelled_at) }}{% endif %}.</p>
{% if booking.amount_paid_minor > 0 %}<p>Vous avez déjà réglé {{ money(booking.amount_paid_minor, booking.currency) }}. Le remboursement éventuel est calculé selon les conditions d'annulation de votre tarif : nous revenons vers vous à ce sujet sous 7 jours.</p>{% endif %}
<p>Nous espérons avoir le plaisir de vous accueillir une autre fois.</p>
{% endif %}
{% include "booking_summary.html" %}
<p>Bien cordialement,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl${% if booking.status == 'expired' %}Your booking request {{ booking.reference }} has expired{% else %}Your booking {{ booking.reference }} has been cancelled{% endif %}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Hello {{ guest.first_name }},</h1>
{% if booking.status == "expired" %}
<p>As the deposit was not received in time, your booking request {{ booking.reference }} has expired and the places held for you have been released.</p>
<p>Still keen to come? Simply book again on our website or get in touch: we will do our best to find you a place.</p>
{% else %}
<p>This confirms that your booking {{ booking.reference }} has been cancelled{% if booking.cancelled_at %} on {{ date(booking.cancelled_at) }}{% endif %}.</p>
{% if booking.amount_paid_minor > 0 %}<p>You have already paid {{ money(booking.amount_paid_minor, booking.currency) }}. Any refund is calculated according to the cancellation policy of your rate; we will get back to you about it within 7 days.</p>{% endif %}
<p>We hope to welcome you another time.</p>
{% endif %}
{% include "booking_summary.html" %}
<p>Kind regards,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl${% if booking.status == 'expired' %}انتهت صلاحية طلب الحجز {{ booking.reference }}{% else %}تم إلغاء حجزك {{ booking.reference }}{% endif %}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا {{ guest.first_name }}،</h1>
{% if booking.status == "expired" %}
<p>نظرًا لعدم استلام العربون في الوقت المحدد، انتهت صلاحية طلب الحجز {{ booking.reference }} وتم تحرير الأماكن التي كانت محجوزة لك.</p>
<p>هل ما زلت ترغب في القدوم؟ يكفي أن تحجز من جديد عبر موقعنا أو أن تتواصل معنا، وسنبذل قصارى جهدنا لإيجاد مكان لك.</p>
{% else %}
<p>نؤكد لك إلغاء حجزك {{ booking.reference }}{% if booking.cancelled_at %} بتاريخ {{ date(booking.cancelled_at) }}{% endif %}.</p>
{% if booking.amount_paid_minor > 0 %}<p>لقد دفعت مبلغ {{ money(booking.amount_paid_minor, booking.currency) }}. يُحتسب الاسترداد المحتمل وفق شروط الإلغاء الخاصة بسعرك، وسنعود إليك بشأنه في غضون 7 أيام.</p>{% endif %}
<p>نأمل أن نسعد باستقبالك في مرة قادمة.</p>
{% endif %}
{% include "booking_summary.html" %}
<p>مع أطيب التحيات،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_modified', ARRAY['site', 'guest', 'booking', 'lines', 'policy']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre réservation {{ booking.reference }} a été modifiée$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bonjour {{ guest.first_name }},</h1>
<p>Votre réservation a bien été modifiée. Voici son récapitulatif à jour : merci de vérifier les dates et les prestations.</p>
{% include "booking_summary.html" %}
{% if booking.balance_minor > 0 %}<p>Reste à régler : <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} avant le {{ date(booking.balance_due_on) }}{% endif %}.</p>{% endif %}
{% include "cancellation_policy.html" %}
{{ button(booking.url, "Voir ma réservation") }}
<p>Si quelque chose ne correspond pas, répondez simplement à cet e-mail.</p>
<p>À bientôt,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your booking {{ booking.reference }} has been updated$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Hello {{ guest.first_name }},</h1>
<p>Your booking has been updated. Here is the new summary: please check the dates and services.</p>
{% include "booking_summary.html" %}
{% if booking.balance_minor > 0 %}<p>Balance due: <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} by {{ date(booking.balance_due_on) }}{% endif %}.</p>{% endif %}
{% include "cancellation_policy.html" %}
{{ button(booking.url, "View my booking") }}
<p>If anything looks wrong, just reply to this email.</p>
<p>See you soon,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تم تعديل حجزك {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا {{ guest.first_name }}،</h1>
<p>تم تعديل حجزك بنجاح. إليك الملخص المحدّث، يُرجى التحقق من التواريخ والخدمات.</p>
{% include "booking_summary.html" %}
{% if booking.balance_minor > 0 %}<p>المبلغ المتبقي: <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} قبل {{ date(booking.balance_due_on) }}{% endif %}.</p>{% endif %}
{% include "cancellation_policy.html" %}
{{ button(booking.url, "عرض حجزي") }}
<p>إذا لاحظت أي خطأ، يكفي الرد على هذه الرسالة.</p>
<p>إلى اللقاء قريبًا،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('payment_receipt', ARRAY['site', 'guest', 'booking', 'lines', 'payment']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Reçu de paiement — réservation {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Merci pour votre paiement, {{ guest.first_name }}</h1>
<p>Nous avons bien reçu votre paiement pour la réservation {{ booking.reference }}.</p>
{% set methods = {"cash": "espèces", "bank_transfer": "virement bancaire", "card_terminal": "carte bancaire (sur place)", "stripe": "carte bancaire en ligne", "cmi": "carte bancaire en ligne (CMI)", "paypal": "PayPal", "other": "autre"} %}
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{{ dir }}" style="border-collapse:collapse;margin:16px 0;font-size:14px;background-color:#fbf7f0;">
  <tr><td style="padding:10px 16px 4px 16px;color:#6b5e4e;text-align:{{ start }};">Montant</td><td style="padding:10px 16px 4px 16px;font-weight:bold;text-align:{{ end }};">{{ money(payment.amount_minor, payment.currency) }}</td></tr>
  {% if payment.paid_at %}<tr><td style="padding:4px 16px;color:#6b5e4e;text-align:{{ start }};">Date</td><td style="padding:4px 16px;text-align:{{ end }};">{{ datetime(payment.paid_at) }}</td></tr>{% endif %}
  <tr><td style="padding:4px 16px;color:#6b5e4e;text-align:{{ start }};">Moyen de paiement</td><td style="padding:4px 16px;text-align:{{ end }};">{{ methods[payment.provider] if payment.provider in methods else payment.provider }}</td></tr>
  {% if payment.reference %}<tr><td style="padding:4px 16px 10px 16px;color:#6b5e4e;text-align:{{ start }};">Référence</td><td style="padding:4px 16px 10px 16px;text-align:{{ end }};"><span dir="ltr">{{ payment.reference }}</span></td></tr>{% endif %}
</table>
{% if booking.balance_minor > 0 %}<p>Reste à régler : <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} avant le {{ date(booking.balance_due_on) }}{% endif %}.</p>{% else %}<p>Votre séjour est entièrement réglé.</p>{% endif %}
{% include "booking_summary.html" %}
{{ button(booking.url, "Voir ma réservation") }}
<p>Merci de votre confiance,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Payment receipt — booking {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Thank you for your payment, {{ guest.first_name }}</h1>
<p>We have received your payment for booking {{ booking.reference }}.</p>
{% set methods = {"cash": "cash", "bank_transfer": "bank transfer", "card_terminal": "card (on site)", "stripe": "card online", "cmi": "card online (CMI)", "paypal": "PayPal", "other": "other"} %}
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{{ dir }}" style="border-collapse:collapse;margin:16px 0;font-size:14px;background-color:#fbf7f0;">
  <tr><td style="padding:10px 16px 4px 16px;color:#6b5e4e;text-align:{{ start }};">Amount</td><td style="padding:10px 16px 4px 16px;font-weight:bold;text-align:{{ end }};">{{ money(payment.amount_minor, payment.currency) }}</td></tr>
  {% if payment.paid_at %}<tr><td style="padding:4px 16px;color:#6b5e4e;text-align:{{ start }};">Date</td><td style="padding:4px 16px;text-align:{{ end }};">{{ datetime(payment.paid_at) }}</td></tr>{% endif %}
  <tr><td style="padding:4px 16px;color:#6b5e4e;text-align:{{ start }};">Payment method</td><td style="padding:4px 16px;text-align:{{ end }};">{{ methods[payment.provider] if payment.provider in methods else payment.provider }}</td></tr>
  {% if payment.reference %}<tr><td style="padding:4px 16px 10px 16px;color:#6b5e4e;text-align:{{ start }};">Reference</td><td style="padding:4px 16px 10px 16px;text-align:{{ end }};"><span dir="ltr">{{ payment.reference }}</span></td></tr>{% endif %}
</table>
{% if booking.balance_minor > 0 %}<p>Balance due: <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} by {{ date(booking.balance_due_on) }}{% endif %}.</p>{% else %}<p>Your stay is fully paid.</p>{% endif %}
{% include "booking_summary.html" %}
{{ button(booking.url, "View my booking") }}
<p>Thank you for your trust,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$إيصال الدفع — الحجز {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">شكرًا على دفعتك يا {{ guest.first_name }}</h1>
<p>لقد استلمنا دفعتك الخاصة بالحجز {{ booking.reference }}.</p>
{% set methods = {"cash": "نقدًا", "bank_transfer": "تحويل بنكي", "card_terminal": "بطاقة بنكية (في عين المكان)", "stripe": "بطاقة بنكية عبر الإنترنت", "cmi": "بطاقة بنكية عبر الإنترنت (CMI)", "paypal": "PayPal", "other": "أخرى"} %}
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{{ dir }}" style="border-collapse:collapse;margin:16px 0;font-size:14px;background-color:#fbf7f0;">
  <tr><td style="padding:10px 16px 4px 16px;color:#6b5e4e;text-align:{{ start }};">المبلغ</td><td style="padding:10px 16px 4px 16px;font-weight:bold;text-align:{{ end }};">{{ money(payment.amount_minor, payment.currency) }}</td></tr>
  {% if payment.paid_at %}<tr><td style="padding:4px 16px;color:#6b5e4e;text-align:{{ start }};">التاريخ</td><td style="padding:4px 16px;text-align:{{ end }};">{{ datetime(payment.paid_at) }}</td></tr>{% endif %}
  <tr><td style="padding:4px 16px;color:#6b5e4e;text-align:{{ start }};">وسيلة الدفع</td><td style="padding:4px 16px;text-align:{{ end }};">{{ methods[payment.provider] if payment.provider in methods else payment.provider }}</td></tr>
  {% if payment.reference %}<tr><td style="padding:4px 16px 10px 16px;color:#6b5e4e;text-align:{{ start }};">المرجع</td><td style="padding:4px 16px 10px 16px;text-align:{{ end }};"><span dir="ltr">{{ payment.reference }}</span></td></tr>{% endif %}
</table>
{% if booking.balance_minor > 0 %}<p>المبلغ المتبقي: <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} قبل {{ date(booking.balance_due_on) }}{% endif %}.</p>{% else %}<p>تم دفع ثمن إقامتك بالكامل.</p>{% endif %}
{% include "booking_summary.html" %}
{{ button(booking.url, "عرض حجزي") }}
<p>شكرًا على ثقتك،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('pre_arrival', ARRAY['site', 'guest', 'booking', 'lines']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre séjour à M'ssici approche : infos pratiques$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Plus que quelques jours, {{ guest.first_name }} !</h1>
<p>Nous vous attendons le <strong>{{ date(booking.arrival_date) }}</strong>{% if booking.check_in_from %}, à partir de {{ booking.check_in_from }}{% endif %}. Voici tout ce qu'il faut savoir pour arriver sereinement.</p>
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">Comment venir</h2>
<p>M'ssici se trouve sur la route N12, entre Rissani et Alnif.</p>
<ul>
<li><strong>Depuis Rissani</strong> : suivez la N12 vers l'ouest, en direction d'Alnif et de Tazzarine (environ 45 km).</li>
<li><strong>Depuis Alnif</strong> : suivez la N12 vers l'est, en direction de Rissani (environ 45 km).</li>
</ul>
<p>En arrivant au village, appelez-nous{% if site.phone %} au <span dir="ltr">{{ site.phone }}</span>{% endif %} : nous venons à votre rencontre. Vous préférez ne pas conduire ? Nous proposons un transfert en 4x4 depuis Rissani, sur demande.</p>
{% if site.map_url %}{{ button(site.map_url, "Ouvrir l'itinéraire") }}{% endif %}
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">Heure d'arrivée</h2>
<p>{% if booking.estimated_arrival_time %}Vous nous avez indiqué une arrivée vers {{ booking.estimated_arrival_time }}. {% endif %}Prévenez-nous si vous arrivez après la tombée de la nuit : la route est plus agréable de jour.</p>
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">Que prévoir</h2>
<ul>
<li>Des vêtements chauds pour le soir : les nuits du désert sont fraîches, surtout d'octobre à avril.</li>
<li>Chapeau, lunettes de soleil et crème solaire.</li>
<li>Une lampe frontale.</li>
<li>Des chaussures confortables pour marcher dans le sable.</li>
<li>Un peu d'argent liquide en dirhams pour les achats sur place.</li>
</ul>
{% include "booking_summary.html" %}
{{ button(booking.url, "Voir ma réservation") }}
<p>Bonne route, et à très vite !<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your stay in M'ssici is coming up: practical information$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Only a few days to go, {{ guest.first_name }}!</h1>
<p>We are expecting you on <strong>{{ date(booking.arrival_date) }}</strong>{% if booking.check_in_from %}, from {{ booking.check_in_from }}{% endif %}. Here is everything you need for a smooth arrival.</p>
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">Getting here</h2>
<p>M'ssici is on the N12 road, between Rissani and Alnif.</p>
<ul>
<li><strong>From Rissani</strong>: follow the N12 west towards Alnif and Tazzarine (about 45 km).</li>
<li><strong>From Alnif</strong>: follow the N12 east towards Rissani (about 45 km).</li>
</ul>
<p>When you reach the village, give us a call{% if site.phone %} on <span dir="ltr">{{ site.phone }}</span>{% endif %} and we will come and meet you. Rather not drive? We offer a 4x4 transfer from Rissani on request.</p>
{% if site.map_url %}{{ button(site.map_url, "Open directions") }}{% endif %}
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">Arrival time</h2>
<p>{% if booking.estimated_arrival_time %}You told us you would arrive around {{ booking.estimated_arrival_time }}. {% endif %}Please let us know if you will arrive after dark: the road is easier in daylight.</p>
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">What to bring</h2>
<ul>
<li>Warm clothes for the evening: desert nights are cool, especially from October to April.</li>
<li>A hat, sunglasses and sunscreen.</li>
<li>A head torch.</li>
<li>Comfortable shoes for walking on sand.</li>
<li>Some cash in dirhams for purchases on site.</li>
</ul>
{% include "booking_summary.html" %}
{{ button(booking.url, "View my booking") }}
<p>Safe travels, see you very soon!<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$إقامتك في مسيسي تقترب: معلومات عملية$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">بقيت أيام قليلة يا {{ guest.first_name }}!</h1>
<p>ننتظرك يوم <strong>{{ date(booking.arrival_date) }}</strong>{% if booking.check_in_from %} ابتداءً من الساعة {{ booking.check_in_from }}{% endif %}. إليك كل ما تحتاج معرفته لوصول مريح.</p>
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">كيف تصل إلينا</h2>
<p>تقع مسيسي على الطريق الوطنية رقم 12، بين الريصاني وألنيف.</p>
<ul>
<li><strong>من الريصاني</strong>: اتبع الطريق الوطنية رقم 12 غربًا في اتجاه ألنيف وتازارين (حوالي 45 كلم).</li>
<li><strong>من ألنيف</strong>: اتبع الطريق الوطنية رقم 12 شرقًا في اتجاه الريصاني (حوالي 45 كلم).</li>
</ul>
<p>عند وصولك إلى القرية، اتصل بنا{% if site.phone %} على الرقم <span dir="ltr">{{ site.phone }}</span>{% endif %} وسنأتي لاستقبالك. تفضّل عدم القيادة؟ نوفر نقلًا بسيارة رباعية الدفع من الريصاني عند الطلب.</p>
{% if site.map_url %}{{ button(site.map_url, "فتح مسار الطريق") }}{% endif %}
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">موعد الوصول</h2>
<p>{% if booking.estimated_arrival_time %}أخبرتنا أنك ستصل حوالي الساعة {{ booking.estimated_arrival_time }}. {% endif %}يُرجى إعلامنا إذا كنت ستصل بعد حلول الظلام، فالطريق أسهل في النهار.</p>
<h2 style="margin:24px 0 8px 0;font-family:Georgia,'Times New Roman',serif;font-size:17px;color:#a3461f;">ماذا تحضر معك</h2>
<ul>
<li>ملابس دافئة للمساء: ليالي الصحراء باردة، خاصة من أكتوبر إلى أبريل.</li>
<li>قبعة ونظارات شمسية وواقٍ من الشمس.</li>
<li>مصباح رأس.</li>
<li>حذاء مريح للمشي على الرمال.</li>
<li>قليل من النقود بالدرهم للمشتريات في عين المكان.</li>
</ul>
{% include "booking_summary.html" %}
{{ button(booking.url, "عرض حجزي") }}
<p>طريق السلامة، وإلى اللقاء قريبًا!<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('review_request', ARRAY['site', 'guest', 'booking']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Comment s'est passé votre séjour à M'ssici ?$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Merci pour votre visite, {{ guest.first_name }} !</h1>
<p>Nous espérons que vous gardez un beau souvenir de vos nuits sous les étoiles de M'ssici. Votre avis compte beaucoup pour notre petite équipe et aide d'autres voyageurs à nous découvrir.</p>
{{ button(booking.review_url, "Donner mon avis") }}
<p>Cela ne prend que deux minutes. Merci d'avance, et au plaisir de vous revoir !</p>
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$How was your stay in M'ssici?$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Thank you for visiting, {{ guest.first_name }}!</h1>
<p>We hope you keep fond memories of your nights under the stars in M'ssici. Your review means a lot to our small team and helps other travellers discover us.</p>
{{ button(booking.review_url, "Leave a review") }}
<p>It only takes two minutes. Thank you, and we hope to see you again!</p>
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$كيف كانت إقامتك في مسيسي؟$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">شكرًا على زيارتك يا {{ guest.first_name }}!</h1>
<p>نأمل أن تحتفظ بذكريات جميلة عن لياليك تحت نجوم مسيسي. رأيك يهم فريقنا الصغير كثيرًا ويساعد مسافرين آخرين على اكتشافنا.</p>
{{ button(booking.review_url, "شارك رأيك") }}
<p>لن يستغرق ذلك سوى دقيقتين. شكرًا مسبقًا، ونتمنى أن نراك مجددًا!</p>
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('inquiry_auto_reply', ARRAY['site', 'inquiry']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Nous avons bien reçu votre message$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bonjour {{ inquiry.name }},</h1>
<p>Merci de nous avoir écrit. Nous avons bien reçu votre message et nous vous répondons sous 24 heures (parfois un peu plus en haute saison).</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{% if inquiry.subject %}<strong>{{ inquiry.subject }}</strong>
{% endif %}{{ inquiry.message }}</p>
{% if site.whatsapp %}<p>Pour une question urgente, écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>.</p>{% endif %}
<p>À très bientôt,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$We have received your message$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Hello {{ inquiry.name }},</h1>
<p>Thank you for writing to us. We have received your message and will reply within 24 hours (sometimes a little longer in high season).</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{% if inquiry.subject %}<strong>{{ inquiry.subject }}</strong>
{% endif %}{{ inquiry.message }}</p>
{% if site.whatsapp %}<p>For anything urgent, message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>.</p>{% endif %}
<p>Speak soon,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$لقد استلمنا رسالتك$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا {{ inquiry.name }}،</h1>
<p>شكرًا على مراسلتنا. لقد استلمنا رسالتك وسنرد عليك في غضون 24 ساعة (وأحيانًا أكثر قليلًا في موسم الذروة).</p>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{% if inquiry.subject %}<strong>{{ inquiry.subject }}</strong>
{% endif %}{{ inquiry.message }}</p>
{% if site.whatsapp %}<p>للأسئلة المستعجلة، راسلنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>.</p>{% endif %}
<p>إلى اللقاء قريبًا،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('newsletter_confirm', ARRAY['site', 'confirm_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Confirmez votre inscription à la lettre de {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Encore une étape !</h1>
<p>Pour recevoir nos nouvelles du désert (offres de saison, événements, vie de la ferme), confirmez votre adresse e-mail :</p>
{{ button(confirm_url, "Confirmer mon inscription") }}
<p>Si vous n'êtes pas à l'origine de cette demande, ignorez simplement cet e-mail : vous ne recevrez rien de notre part.</p>
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Confirm your subscription to the {{ site.name }} newsletter$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">One more step!</h1>
<p>To receive news from the desert (seasonal offers, events, life on the farm), please confirm your email address:</p>
{{ button(confirm_url, "Confirm my subscription") }}
<p>If you did not ask for this, simply ignore this email: you will not hear from us.</p>
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$أكّد اشتراكك في النشرة الإخبارية لـ {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">خطوة أخيرة!</h1>
<p>لتصلك أخبار الصحراء (عروض الموسم، الفعاليات، الحياة في المزرعة)، يُرجى تأكيد عنوان بريدك الإلكتروني:</p>
{{ button(confirm_url, "تأكيد اشتراكي") }}
<p>إذا لم تطلب ذلك، تجاهل هذه الرسالة ببساطة ولن تصلك منا أي رسالة.</p>
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('password_reset', ARRAY['site', 'user', 'action_url', 'expires_at']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Réinitialisation de votre mot de passe {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bonjour {{ user.full_name }},</h1>
<p>Nous avons reçu une demande de réinitialisation du mot de passe de votre compte {{ site.name }}.</p>
{{ button(action_url, "Choisir un nouveau mot de passe") }}
<p>Ce lien est valable jusqu'au {{ datetime(expires_at) }} et ne peut servir qu'une seule fois.</p>
<p>Vous n'êtes pas à l'origine de cette demande ? Ignorez cet e-mail : votre mot de passe reste inchangé.</p>$tpl$),
    ('en', $tpl$Reset your {{ site.name }} password$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Hello {{ user.full_name }},</h1>
<p>We received a request to reset the password of your {{ site.name }} account.</p>
{{ button(action_url, "Choose a new password") }}
<p>This link is valid until {{ datetime(expires_at) }} and can only be used once.</p>
<p>Didn't ask for this? Ignore this email: your password stays the same.</p>$tpl$),
    ('ar', $tpl$إعادة تعيين كلمة مرور {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا {{ user.full_name }}،</h1>
<p>تلقينا طلبًا لإعادة تعيين كلمة مرور حسابك على {{ site.name }}.</p>
{{ button(action_url, "اختيار كلمة مرور جديدة") }}
<p>هذا الرابط صالح حتى {{ datetime(expires_at) }} ولا يمكن استخدامه إلا مرة واحدة.</p>
<p>لم تطلب ذلك؟ تجاهل هذه الرسالة، وستبقى كلمة مرورك كما هي.</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('email_verification', ARRAY['site', 'user', 'action_url', 'expires_at']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Confirmez votre adresse e-mail$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bonjour {{ user.full_name }},</h1>
<p>Merci de confirmer votre adresse e-mail pour finaliser votre compte {{ site.name }}.</p>
{{ button(action_url, "Confirmer mon adresse") }}
<p>Ce lien est valable jusqu'au {{ datetime(expires_at) }}.</p>
<p>Si vous n'avez pas créé de compte, ignorez simplement cet e-mail.</p>$tpl$),
    ('en', $tpl$Confirm your email address$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Hello {{ user.full_name }},</h1>
<p>Please confirm your email address to finish setting up your {{ site.name }} account.</p>
{{ button(action_url, "Confirm my address") }}
<p>This link is valid until {{ datetime(expires_at) }}.</p>
<p>If you did not create an account, simply ignore this email.</p>$tpl$),
    ('ar', $tpl$أكّد عنوان بريدك الإلكتروني$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا {{ user.full_name }}،</h1>
<p>يُرجى تأكيد عنوان بريدك الإلكتروني لإتمام إنشاء حسابك على {{ site.name }}.</p>
{{ button(action_url, "تأكيد عنواني") }}
<p>هذا الرابط صالح حتى {{ datetime(expires_at) }}.</p>
<p>إذا لم تقم بإنشاء حساب، تجاهل هذه الرسالة ببساطة.</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('staff_invitation', ARRAY['site', 'user', 'action_url', 'expires_at', 'invited_by']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Invitation à l'administration de {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bienvenue dans l'équipe, {{ user.full_name }} !</h1>
<p>{% if invited_by %}{{ invited_by }} vous invite{% else %}Vous êtes invité(e){% endif %} à rejoindre l'espace d'administration de {{ site.name }} pour gérer les réservations, les activités et le contenu du site.</p>
{{ button(action_url, "Activer mon compte") }}
<p>Vous choisirez votre mot de passe à l'étape suivante. Ce lien expire le {{ datetime(expires_at) }}.</p>$tpl$),
    ('en', $tpl$Invitation to the {{ site.name }} back office$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Welcome to the team, {{ user.full_name }}!</h1>
<p>{% if invited_by %}{{ invited_by }} has invited you{% else %}You have been invited{% endif %} to join the {{ site.name }} back office to manage bookings, activities and website content.</p>
{{ button(action_url, "Activate my account") }}
<p>You will choose your password in the next step. This link expires on {{ datetime(expires_at) }}.</p>$tpl$),
    ('ar', $tpl$دعوة للانضمام إلى إدارة {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا بك في الفريق يا {{ user.full_name }}!</h1>
<p>{% if invited_by %}يدعوك {{ invited_by }}{% else %}أنت مدعو{% endif %} للانضمام إلى فضاء إدارة {{ site.name }} لتدبير الحجوزات والأنشطة ومحتوى الموقع.</p>
{{ button(action_url, "تفعيل حسابي") }}
<p>ستختار كلمة المرور في الخطوة التالية. تنتهي صلاحية هذا الرابط في {{ datetime(expires_at) }}.</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('staff_new_booking', ARRAY['site', 'guest', 'booking', 'lines', 'admin_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Nouvelle réservation {{ booking.reference }} — {{ guest.full_name }}$tpl$, $tpl${% set statuses = {"draft": "brouillon", "pending": "en attente de confirmation", "awaiting_payment": "en attente de l'acompte", "confirmed": "confirmée", "cancelled": "annulée", "expired": "expirée"} %}<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Nouvelle réservation</h1>
<p><strong>{{ guest.full_name }}</strong>{% if guest.email %} · <a href="mailto:{{ guest.email }}" style="color:#a3461f;">{{ guest.email }}</a>{% endif %}{% if guest.phone %} · <span dir="ltr">{{ guest.phone }}</span>{% endif %}</p>
<p>Statut : <strong>{{ statuses[booking.status] if booking.status in statuses else booking.status }}</strong>{% if booking.hold_expires_at %}, places retenues jusqu'au {{ datetime(booking.hold_expires_at) }}{% endif %}.</p>
{% include "booking_summary.html" %}
{% if booking.special_requests %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;"><strong>Demandes particulières</strong>
{{ booking.special_requests }}</p>{% endif %}
{{ button(admin_url, "Ouvrir dans l'administration") }}$tpl$),
    ('en', $tpl$New booking {{ booking.reference }} — {{ guest.full_name }}$tpl$, $tpl${% set statuses = {"draft": "draft", "pending": "awaiting confirmation", "awaiting_payment": "awaiting deposit", "confirmed": "confirmed", "cancelled": "cancelled", "expired": "expired"} %}<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">New booking</h1>
<p><strong>{{ guest.full_name }}</strong>{% if guest.email %} · <a href="mailto:{{ guest.email }}" style="color:#a3461f;">{{ guest.email }}</a>{% endif %}{% if guest.phone %} · <span dir="ltr">{{ guest.phone }}</span>{% endif %}</p>
<p>Status: <strong>{{ statuses[booking.status] if booking.status in statuses else booking.status }}</strong>{% if booking.hold_expires_at %}, places held until {{ datetime(booking.hold_expires_at) }}{% endif %}.</p>
{% include "booking_summary.html" %}
{% if booking.special_requests %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;"><strong>Special requests</strong>
{{ booking.special_requests }}</p>{% endif %}
{{ button(admin_url, "Open in the back office") }}$tpl$),
    ('ar', $tpl$حجز جديد {{ booking.reference }} — {{ guest.full_name }}$tpl$, $tpl${% set statuses = {"draft": "مسودة", "pending": "في انتظار التأكيد", "awaiting_payment": "في انتظار العربون", "confirmed": "مؤكد", "cancelled": "ملغى", "expired": "منتهي الصلاحية"} %}<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">حجز جديد</h1>
<p><strong>{{ guest.full_name }}</strong>{% if guest.email %} · <a href="mailto:{{ guest.email }}" style="color:#a3461f;">{{ guest.email }}</a>{% endif %}{% if guest.phone %} · <span dir="ltr">{{ guest.phone }}</span>{% endif %}</p>
<p>الحالة: <strong>{{ statuses[booking.status] if booking.status in statuses else booking.status }}</strong>{% if booking.hold_expires_at %}، الأماكن محجوزة حتى {{ datetime(booking.hold_expires_at) }}{% endif %}.</p>
{% include "booking_summary.html" %}
{% if booking.special_requests %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;"><strong>طلبات خاصة</strong>
{{ booking.special_requests }}</p>{% endif %}
{{ button(admin_url, "فتح في لوحة الإدارة") }}$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('staff_booking_cancelled', ARRAY['site', 'guest', 'booking', 'lines', 'admin_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Réservation annulée : {{ booking.reference }} — {{ guest.full_name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Réservation annulée</h1>
<p><strong>{{ guest.full_name }}</strong>{% if guest.email %} · <a href="mailto:{{ guest.email }}" style="color:#a3461f;">{{ guest.email }}</a>{% endif %}{% if guest.phone %} · <span dir="ltr">{{ guest.phone }}</span>{% endif %}</p>
<p>{% if booking.cancellation_reason %}Motif : {{ booking.cancellation_reason }}. {% endif %}Les places ont été libérées automatiquement.</p>
{% if booking.amount_paid_minor > 0 %}<p><strong>Déjà encaissé : {{ money(booking.amount_paid_minor, booking.currency) }}</strong> — vérifier le remboursement dû selon les conditions d'annulation.</p>{% endif %}
{% include "booking_summary.html" %}
{{ button(admin_url, "Ouvrir dans l'administration") }}$tpl$),
    ('en', $tpl$Booking cancelled: {{ booking.reference }} — {{ guest.full_name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Booking cancelled</h1>
<p><strong>{{ guest.full_name }}</strong>{% if guest.email %} · <a href="mailto:{{ guest.email }}" style="color:#a3461f;">{{ guest.email }}</a>{% endif %}{% if guest.phone %} · <span dir="ltr">{{ guest.phone }}</span>{% endif %}</p>
<p>{% if booking.cancellation_reason %}Reason: {{ booking.cancellation_reason }}. {% endif %}The places have been released automatically.</p>
{% if booking.amount_paid_minor > 0 %}<p><strong>Already collected: {{ money(booking.amount_paid_minor, booking.currency) }}</strong> — check the refund due under the cancellation policy.</p>{% endif %}
{% include "booking_summary.html" %}
{{ button(admin_url, "Open in the back office") }}$tpl$),
    ('ar', $tpl$حجز ملغى: {{ booking.reference }} — {{ guest.full_name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">حجز ملغى</h1>
<p><strong>{{ guest.full_name }}</strong>{% if guest.email %} · <a href="mailto:{{ guest.email }}" style="color:#a3461f;">{{ guest.email }}</a>{% endif %}{% if guest.phone %} · <span dir="ltr">{{ guest.phone }}</span>{% endif %}</p>
<p>{% if booking.cancellation_reason %}السبب: {{ booking.cancellation_reason }}. {% endif %}تم تحرير الأماكن تلقائيًا.</p>
{% if booking.amount_paid_minor > 0 %}<p><strong>المبلغ المحصّل: {{ money(booking.amount_paid_minor, booking.currency) }}</strong> — يُرجى التحقق من الاسترداد المستحق وفق شروط الإلغاء.</p>{% endif %}
{% include "booking_summary.html" %}
{{ button(admin_url, "فتح في لوحة الإدارة") }}$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('staff_new_inquiry', ARRAY['site', 'inquiry', 'admin_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Nouvelle demande : {{ inquiry.subject or inquiry.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Nouvelle demande de contact</h1>
{% set kinds = {"contact": "contact", "quote": "demande de devis", "group": "groupe", "event": "événement", "partnership": "partenariat", "press": "presse"} %}
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{{ dir }}" style="border-collapse:collapse;margin:16px 0;font-size:14px;">
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Nom</td><td style="padding:4px 0;text-align:{{ end }};">{{ inquiry.name }}</td></tr>
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">E-mail</td><td style="padding:4px 0;text-align:{{ end }};"><a href="mailto:{{ inquiry.email }}" style="color:#a3461f;">{{ inquiry.email }}</a></td></tr>
  {% if inquiry.phone %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Téléphone</td><td style="padding:4px 0;text-align:{{ end }};"><span dir="ltr">{{ inquiry.phone }}</span></td></tr>{% endif %}
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Type</td><td style="padding:4px 0;text-align:{{ end }};">{{ kinds[inquiry.kind] if inquiry.kind in kinds else inquiry.kind }}</td></tr>
  {% if inquiry.preferred_from %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Dates souhaitées</td><td style="padding:4px 0;text-align:{{ end }};">{{ date(inquiry.preferred_from) }}{% if inquiry.preferred_to %} → {{ date(inquiry.preferred_to) }}{% endif %}</td></tr>{% endif %}
  {% if inquiry.group_size %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Taille du groupe</td><td style="padding:4px 0;text-align:{{ end }};">{{ inquiry.group_size }}</td></tr>{% endif %}
</table>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{% if inquiry.subject %}<strong>{{ inquiry.subject }}</strong>
{% endif %}{{ inquiry.message }}</p>
<p>Une réponse automatique a été envoyée ; merci de répondre sous 24 heures.</p>
{{ button(admin_url, "Ouvrir dans l'administration") }}$tpl$),
    ('en', $tpl$New inquiry: {{ inquiry.subject or inquiry.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">New contact request</h1>
{% set kinds = {"contact": "contact", "quote": "quote request", "group": "group", "event": "event", "partnership": "partnership", "press": "press"} %}
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{{ dir }}" style="border-collapse:collapse;margin:16px 0;font-size:14px;">
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Name</td><td style="padding:4px 0;text-align:{{ end }};">{{ inquiry.name }}</td></tr>
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Email</td><td style="padding:4px 0;text-align:{{ end }};"><a href="mailto:{{ inquiry.email }}" style="color:#a3461f;">{{ inquiry.email }}</a></td></tr>
  {% if inquiry.phone %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Phone</td><td style="padding:4px 0;text-align:{{ end }};"><span dir="ltr">{{ inquiry.phone }}</span></td></tr>{% endif %}
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Type</td><td style="padding:4px 0;text-align:{{ end }};">{{ kinds[inquiry.kind] if inquiry.kind in kinds else inquiry.kind }}</td></tr>
  {% if inquiry.preferred_from %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Preferred dates</td><td style="padding:4px 0;text-align:{{ end }};">{{ date(inquiry.preferred_from) }}{% if inquiry.preferred_to %} → {{ date(inquiry.preferred_to) }}{% endif %}</td></tr>{% endif %}
  {% if inquiry.group_size %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">Group size</td><td style="padding:4px 0;text-align:{{ end }};">{{ inquiry.group_size }}</td></tr>{% endif %}
</table>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{% if inquiry.subject %}<strong>{{ inquiry.subject }}</strong>
{% endif %}{{ inquiry.message }}</p>
<p>An automatic reply has been sent; please answer within 24 hours.</p>
{{ button(admin_url, "Open in the back office") }}$tpl$),
    ('ar', $tpl$طلب جديد: {{ inquiry.subject or inquiry.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">طلب تواصل جديد</h1>
{% set kinds = {"contact": "تواصل", "quote": "طلب عرض سعر", "group": "مجموعة", "event": "مناسبة", "partnership": "شراكة", "press": "صحافة"} %}
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" border="0" dir="{{ dir }}" style="border-collapse:collapse;margin:16px 0;font-size:14px;">
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">الاسم</td><td style="padding:4px 0;text-align:{{ end }};">{{ inquiry.name }}</td></tr>
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">البريد الإلكتروني</td><td style="padding:4px 0;text-align:{{ end }};"><a href="mailto:{{ inquiry.email }}" style="color:#a3461f;">{{ inquiry.email }}</a></td></tr>
  {% if inquiry.phone %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">الهاتف</td><td style="padding:4px 0;text-align:{{ end }};"><span dir="ltr">{{ inquiry.phone }}</span></td></tr>{% endif %}
  <tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">النوع</td><td style="padding:4px 0;text-align:{{ end }};">{{ kinds[inquiry.kind] if inquiry.kind in kinds else inquiry.kind }}</td></tr>
  {% if inquiry.preferred_from %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">التواريخ المفضلة</td><td style="padding:4px 0;text-align:{{ end }};">{{ date(inquiry.preferred_from) }}{% if inquiry.preferred_to %} → {{ date(inquiry.preferred_to) }}{% endif %}</td></tr>{% endif %}
  {% if inquiry.group_size %}<tr><td style="padding:4px 0;color:#6b5e4e;text-align:{{ start }};vertical-align:top;">حجم المجموعة</td><td style="padding:4px 0;text-align:{{ end }};">{{ inquiry.group_size }}</td></tr>{% endif %}
</table>
<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;white-space:pre-line;">{% if inquiry.subject %}<strong>{{ inquiry.subject }}</strong>
{% endif %}{{ inquiry.message }}</p>
<p>تم إرسال رد تلقائي، يُرجى الرد خلال 24 ساعة.</p>
{{ button(admin_url, "فتح في لوحة الإدارة") }}$tpl$)
) AS v (locale, subject, body_html);
