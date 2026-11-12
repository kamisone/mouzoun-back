-- 0016_booking_emails: templates of the booking funnel events (fr/en/ar). See docs/email/README.md
-- and docs/booking/README.md. body_text is generated from the HTML.
WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_declined', ARRAY['site', 'guest', 'booking']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre demande {{ booking.reference }} n'a pas pu être acceptée$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Nous sommes désolés, {{ guest.first_name }}</h1>
<p>Nous ne pouvons malheureusement pas vous accueillir aux dates demandées{% if booking.arrival_date %} (à partir du {{ date(booking.arrival_date) }}){% endif %}.</p>
{% if booking.cancellation_reason %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ booking.cancellation_reason }}</p>{% endif %}
<p>Si un montant a été réglé, il vous est intégralement remboursé sur votre moyen de paiement d'origine.</p>
<p>D'autres dates sont peut-être encore libres : nous serions heureux de vous recevoir une autre fois.</p>
{{ button(site.url, "Voir les disponibilités") }}
<p>Une question ? Répondez simplement à cet e-mail{% if site.whatsapp %} ou écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$We could not accept your request {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">We are sorry, {{ guest.first_name }}</h1>
<p>Unfortunately we cannot host you on the requested dates{% if booking.arrival_date %} (from {{ date(booking.arrival_date) }}){% endif %}.</p>
{% if booking.cancellation_reason %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ booking.cancellation_reason }}</p>{% endif %}
<p>If you paid anything, it is refunded in full to your original payment method.</p>
<p>Other dates may still be free: we would be happy to welcome you another time.</p>
{{ button(site.url, "See availability") }}
<p>Any questions? Just reply to this email{% if site.whatsapp %} or message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تعذّر قبول طلبك {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">نعتذر منك يا {{ guest.first_name }}</h1>
<p>للأسف لا يمكننا استقبالك في التواريخ المطلوبة{% if booking.arrival_date %} (ابتداءً من {{ date(booking.arrival_date) }}){% endif %}.</p>
{% if booking.cancellation_reason %}<p style="margin:16px 0;padding:12px 16px;background-color:#fbf7f0;border-radius:6px;">{{ booking.cancellation_reason }}</p>{% endif %}
<p>إذا كنت قد دفعت أي مبلغ، فسيُعاد إليك كاملًا عبر وسيلة الدفع الأصلية.</p>
<p>قد تكون تواريخ أخرى متاحة، ويسعدنا استقبالك في وقت آخر.</p>
{{ button(site.url, "عرض التوفر") }}
<p>هل لديك سؤال؟ يكفي الرد على هذه الرسالة{% if site.whatsapp %} أو مراسلتنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_payment_link', ARRAY['site', 'guest', 'booking', 'lines', 'policy']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Bonne nouvelle : votre demande {{ booking.reference }} est acceptée$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bonne nouvelle, {{ guest.first_name }} !</h1>
<p>Nous pouvons vous accueillir. Pour confirmer votre séjour, il reste à régler <strong>{{ money(booking.deposit_remaining_minor, booking.currency) }}</strong>{% if booking.hold_expires_at %} avant le {{ datetime(booking.hold_expires_at) }}{% endif %}. Nous gardons vos places jusque-là.</p>
{{ button(booking.url, "Payer et confirmer") }}
{% include "booking_summary.html" %}
{% include "cancellation_policy.html" %}
<p>Une question ? Répondez simplement à cet e-mail{% if site.whatsapp %} ou écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Good news: your request {{ booking.reference }} is accepted$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Good news, {{ guest.first_name }}!</h1>
<p>We can host you. To confirm your stay, please pay <strong>{{ money(booking.deposit_remaining_minor, booking.currency) }}</strong>{% if booking.hold_expires_at %} by {{ datetime(booking.hold_expires_at) }}{% endif %}. We are holding your places until then.</p>
{{ button(booking.url, "Pay and confirm") }}
{% include "booking_summary.html" %}
{% include "cancellation_policy.html" %}
<p>Any questions? Just reply to this email{% if site.whatsapp %} or message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$خبر سار: تم قبول طلبك {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">خبر سار يا {{ guest.first_name }}!</h1>
<p>يسعدنا استقبالك. لتأكيد إقامتك، يتبقى دفع <strong>{{ money(booking.deposit_remaining_minor, booking.currency) }}</strong>{% if booking.hold_expires_at %} قبل {{ datetime(booking.hold_expires_at) }}{% endif %}. نحتفظ لك بالأماكن حتى ذلك الحين.</p>
{{ button(booking.url, "ادفع وأكّد") }}
{% include "booking_summary.html" %}
{% include "cancellation_policy.html" %}
<p>هل لديك سؤال؟ يكفي الرد على هذه الرسالة{% if site.whatsapp %} أو مراسلتنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('balance_reminder', ARRAY['site', 'guest', 'booking']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Solde de votre séjour {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Votre séjour approche, {{ guest.first_name }}</h1>
<p>Il reste <strong>{{ money(booking.balance_minor, booking.currency) }}</strong> à régler{% if booking.balance_due_on %} avant le {{ date(booking.balance_due_on) }}{% endif %}. Vous pouvez payer en ligne en toute sécurité.</p>
{{ button(booking.url, "Régler le solde") }}
<p>Une question ? Répondez simplement à cet e-mail{% if site.whatsapp %} ou écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Balance of your stay {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Your stay is coming up, {{ guest.first_name }}</h1>
<p><strong>{{ money(booking.balance_minor, booking.currency) }}</strong> remains to be paid{% if booking.balance_due_on %} by {{ date(booking.balance_due_on) }}{% endif %}. You can pay online securely.</p>
{{ button(booking.url, "Pay the balance") }}
<p>Any questions? Just reply to this email{% if site.whatsapp %} or message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$رصيد إقامتك {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">اقترب موعد إقامتك يا {{ guest.first_name }}</h1>
<p>يتبقى دفع <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>{% if booking.balance_due_on %} قبل {{ date(booking.balance_due_on) }}{% endif %}. يمكنك الدفع عبر الإنترنت بأمان.</p>
{{ button(booking.url, "ادفع الرصيد") }}
<p>هل لديك سؤال؟ يكفي الرد على هذه الرسالة{% if site.whatsapp %} أو مراسلتنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('balance_failed', ARRAY['site', 'guest', 'booking']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Le paiement du solde n'a pas abouti ({{ booking.reference }})$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Un petit souci de paiement</h1>
<p>Bonjour {{ guest.first_name }}, nous n'avons pas pu débiter votre carte pour le solde de <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>. Votre réservation reste valable.</p>
<p>Vous pouvez régler le solde en ligne avec une autre carte, ou nous contacter pour payer à l'arrivée.</p>
{{ button(booking.url, "Régler le solde") }}
<p>Une question ? Répondez simplement à cet e-mail{% if site.whatsapp %} ou écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$The balance payment did not go through ({{ booking.reference }})$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">A small payment issue</h1>
<p>Hello {{ guest.first_name }}, we could not charge your card for the balance of <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>. Your booking is still valid.</p>
<p>You can pay the balance online with another card, or contact us to pay on arrival.</p>
{{ button(booking.url, "Pay the balance") }}
<p>Any questions? Just reply to this email{% if site.whatsapp %} or message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$لم يتم دفع الرصيد ({{ booking.reference }})$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مشكلة بسيطة في الدفع</h1>
<p>مرحبًا {{ guest.first_name }}، لم نتمكن من خصم الرصيد البالغ <strong>{{ money(booking.balance_minor, booking.currency) }}</strong> من بطاقتك. يبقى حجزك ساري المفعول.</p>
<p>يمكنك دفع الرصيد عبر الإنترنت ببطاقة أخرى، أو التواصل معنا للدفع عند الوصول.</p>
{{ button(booking.url, "ادفع الرصيد") }}
<p>هل لديك سؤال؟ يكفي الرد على هذه الرسالة{% if site.whatsapp %} أو مراسلتنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('staff_balance_failed', ARRAY['site', 'guest', 'booking', 'admin_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Solde refusé — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Le débit du solde a échoué</h1>
<p>{{ guest.full_name }} — {{ booking.reference }}{% if booking.arrival_date %}, arrivée le {{ date(booking.arrival_date) }}{% endif %}.</p>
<p>Solde : <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>. Le client a reçu un lien pour payer. Une tâche a été créée ; la réservation n'est jamais annulée automatiquement.</p>
{{ button(admin_url, "Ouvrir la réservation") }}$tpl$),
    ('en', $tpl$Balance declined — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">The balance charge failed</h1>
<p>{{ guest.full_name }} — {{ booking.reference }}{% if booking.arrival_date %}, arriving {{ date(booking.arrival_date) }}{% endif %}.</p>
<p>Balance: <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>. The guest received a payment link. A task was created; the booking is never cancelled automatically.</p>
{{ button(admin_url, "Open the booking") }}$tpl$),
    ('ar', $tpl$تم رفض الرصيد — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">فشل خصم الرصيد</h1>
<p>{{ guest.full_name }} — {{ booking.reference }}{% if booking.arrival_date %}، الوصول في {{ date(booking.arrival_date) }}{% endif %}.</p>
<p>الرصيد: <strong>{{ money(booking.balance_minor, booking.currency) }}</strong>. تلقى الضيف رابطًا للدفع. تم إنشاء مهمة، ولا يُلغى الحجز تلقائيًا أبدًا.</p>
{{ button(admin_url, "فتح الحجز") }}$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('precheckin_reminder', ARRAY['site', 'guest', 'booking']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Gagnez du temps à l'arrivée : fiche voyageurs en ligne ({{ booking.reference }})$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Préparez votre arrivée, {{ guest.first_name }}</h1>
<p>La loi marocaine nous demande d'enregistrer chaque voyageur (fiche de police). Remplissez-la en ligne en deux minutes : nom, nationalité, date de naissance et pièce d'identité de chaque adulte. À votre arrivée, il ne restera qu'à boire le thé.</p>
{{ button(booking.url, "Remplir la fiche") }}
<p style="font-size:13px;color:#6b6258;">Vos numéros de pièce d'identité sont chiffrés et supprimés après la durée légale de conservation.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Save time on arrival: guest register online ({{ booking.reference }})$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Get ready for your arrival, {{ guest.first_name }}</h1>
<p>Moroccan law requires us to register every guest (police form). Fill it in online in two minutes: name, nationality, date of birth and ID document of each adult. On arrival, all that is left is the tea.</p>
{{ button(booking.url, "Fill in the form") }}
<p style="font-size:13px;color:#6b6258;">Your ID numbers are encrypted and deleted after the legal retention period.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$وفّر الوقت عند الوصول: استمارة النزلاء عبر الإنترنت ({{ booking.reference }})$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">استعد لوصولك يا {{ guest.first_name }}</h1>
<p>يُلزمنا القانون المغربي بتسجيل كل نزيل (استمارة الشرطة). املأها عبر الإنترنت في دقيقتين: الاسم والجنسية وتاريخ الميلاد ووثيقة الهوية لكل بالغ. وعند وصولك، لا يبقى سوى الشاي.</p>
{{ button(booking.url, "ملء الاستمارة") }}
<p style="font-size:13px;color:#6b6258;">أرقام وثائق الهوية مشفّرة وتُحذف بعد انتهاء مدة الحفظ القانونية.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_magic_link', ARRAY['site', 'guest', 'booking', 'access_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre lien d'accès à la réservation {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Votre réservation {{ booking.reference }}</h1>
<p>Bonjour {{ guest.first_name }}, voici votre lien personnel pour consulter et gérer votre réservation. Il est valable une heure et ne fonctionne qu'une fois.</p>
{{ button(access_url, "Ouvrir ma réservation") }}
<p style="font-size:13px;color:#6b6258;">Vous n'êtes pas à l'origine de cette demande ? Ignorez simplement cet e-mail : personne ne peut accéder à votre réservation sans ce lien.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your access link for booking {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Your booking {{ booking.reference }}</h1>
<p>Hello {{ guest.first_name }}, here is your personal link to view and manage your booking. It is valid for one hour and works once.</p>
{{ button(access_url, "Open my booking") }}
<p style="font-size:13px;color:#6b6258;">Did not ask for this? Simply ignore this email: nobody can open your booking without this link.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$رابط الوصول إلى الحجز {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">حجزك {{ booking.reference }}</h1>
<p>مرحبًا {{ guest.first_name }}، إليك رابطك الشخصي لعرض حجزك وإدارته. الرابط صالح لمدة ساعة ويعمل مرة واحدة فقط.</p>
{{ button(access_url, "فتح حجزي") }}
<p style="font-size:13px;color:#6b6258;">لم تطلب هذا الرابط؟ تجاهل هذه الرسالة ببساطة: لا يمكن لأحد فتح حجزك بدون هذا الرابط.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('refund_receipt', ARRAY['site', 'guest', 'booking', 'refund']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Remboursement de {{ money(refund.amount_minor, refund.currency) }} — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Votre remboursement est en route</h1>
<p>Bonjour {{ guest.first_name }}, nous avons remboursé <strong>{{ money(refund.amount_minor, refund.currency) }}</strong> sur votre moyen de paiement d'origine. Selon votre banque, le montant apparaît sous 5 à 10 jours ouvrés.</p>
{{ button(booking.url, "Voir ma réservation") }}
<p>Une question ? Répondez simplement à cet e-mail{% if site.whatsapp %} ou écrivez-nous sur WhatsApp au <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Refund of {{ money(refund.amount_minor, refund.currency) }} — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Your refund is on its way</h1>
<p>Hello {{ guest.first_name }}, we refunded <strong>{{ money(refund.amount_minor, refund.currency) }}</strong> to your original payment method. Depending on your bank, it shows within 5 to 10 business days.</p>
{{ button(booking.url, "View my booking") }}
<p>Any questions? Just reply to this email{% if site.whatsapp %} or message us on WhatsApp at <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$استرداد {{ money(refund.amount_minor, refund.currency) }} — {{ booking.reference }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">استردادك في الطريق</h1>
<p>مرحبًا {{ guest.first_name }}، أعدنا <strong>{{ money(refund.amount_minor, refund.currency) }}</strong> إلى وسيلة الدفع الأصلية. حسب بنكك، يظهر المبلغ خلال 5 إلى 10 أيام عمل.</p>
{{ button(booking.url, "عرض حجزي") }}
<p>هل لديك سؤال؟ يكفي الرد على هذه الرسالة{% if site.whatsapp %} أو مراسلتنا عبر واتساب على الرقم <span dir="ltr">{{ site.whatsapp }}</span>{% endif %}.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('cart_abandoned', ARRAY['site', 'guest', 'booking']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Vos dates à M'ssici vous attendent peut-être encore$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Toujours tenté par le désert, {{ guest.first_name }} ?</h1>
<p>Vous n'avez pas terminé votre réservation{% if booking.arrival_date %} pour le {{ date(booking.arrival_date) }}{% endif %}. Nous ne retenons plus vos places, mais elles sont peut-être encore libres.</p>
{{ button(site.url, "Reprendre ma réservation") }}
<p style="font-size:13px;color:#6b6258;">Vous recevez cet e-mail parce que vous avez accepté nos communications. Il n'y en aura pas d'autre à ce sujet.</p>
<p>À très bientôt sous les étoiles,<br>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your dates in M'ssici may still be free$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Still dreaming of the desert, {{ guest.first_name }}?</h1>
<p>You did not finish your booking{% if booking.arrival_date %} for {{ date(booking.arrival_date) }}{% endif %}. We are no longer holding your places, but they may still be free.</p>
{{ button(site.url, "Resume my booking") }}
<p style="font-size:13px;color:#6b6258;">You receive this email because you accepted our communications. There will be no other one about this.</p>
<p>See you soon under the stars,<br>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$قد تكون تواريخك في مسيسي ما تزال متاحة$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">ما زالت الصحراء تناديك يا {{ guest.first_name }}؟</h1>
<p>لم تُكمل حجزك{% if booking.arrival_date %} ليوم {{ date(booking.arrival_date) }}{% endif %}. لم نعد نحتفظ بالأماكن، لكنها قد تكون ما تزال متاحة.</p>
{{ button(site.url, "متابعة الحجز") }}
<p style="font-size:13px;color:#6b6258;">تصلك هذه الرسالة لأنك وافقت على تلقي رسائلنا. لن نرسل غيرها بهذا الشأن.</p>
<p>إلى اللقاء قريبًا تحت النجوم،<br>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

INSERT INTO settings (key, group_name, value_type, value, is_public, description) VALUES
    ('email.balance_reminder_enabled', 'email', 'boolean', 'true', false, 'Email a payment link before the balance is due (payment-link collection)');
