-- 0018_account_emails: guest account emails (sign-in code, welcome, security notices, booking claim,
-- data export, deletion) in fr/en/ar. See docs/accounts/README.md.

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('sign_in_code', ARRAY['site', 'code', 'link', 'minutes']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl${{ code }} est votre code de connexion {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Votre code de connexion</h1>
<p>Saisissez ce code pour vous connecter ou créer votre compte :</p>
<p style="margin:24px 0;font-family:Georgia,serif;font-size:32px;letter-spacing:8px;color:#1d1a17;" dir="ltr">{{ code }}</p>
<p>Ou utilisez ce lien, valable {{ minutes }} minutes :</p>
{{ button(link, "Me connecter") }}
<p style="font-size:13px;color:#6b6258;">Vous n'avez rien demandé ? Ignorez cet e-mail : personne ne peut se connecter sans ce code.</p>
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl${{ code }} is your {{ site.name }} sign-in code$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Your sign-in code</h1>
<p>Enter this code to sign in or create your account:</p>
<p style="margin:24px 0;font-family:Georgia,serif;font-size:32px;letter-spacing:8px;color:#1d1a17;" dir="ltr">{{ code }}</p>
<p>Or use this link, valid for {{ minutes }} minutes:</p>
{{ button(link, "Sign me in") }}
<p style="font-size:13px;color:#6b6258;">Didn't ask for this? Ignore this email: nobody can sign in without this code.</p>
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl${{ code }} هو رمز الدخول إلى {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">رمز تسجيل الدخول</h1>
<p>أدخل هذا الرمز لتسجيل الدخول أو إنشاء حسابك:</p>
<p style="margin:24px 0;font-family:Georgia,serif;font-size:32px;letter-spacing:8px;color:#1d1a17;" dir="ltr">{{ code }}</p>
<p>أو استخدم هذا الرابط، الصالح لمدة {{ minutes }} دقائق:</p>
{{ button(link, "تسجيل دخولي") }}
<p style="font-size:13px;color:#6b6258;">لم تطلب ذلك؟ تجاهل هذه الرسالة: لا يمكن لأحد الدخول بدون هذا الرمز.</p>
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('account_welcome', ARRAY['site', 'first_name', 'account_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Bienvenue chez {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Bienvenue{% if first_name %}, {{ first_name }}{% endif %} !</h1>
<p>Votre compte est prêt. Vous y retrouvez vos voyages, vos billets, vos coordonnées et vos envies.</p>
{{ button(account_url, "Mon compte") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Welcome to {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Welcome{% if first_name %}, {{ first_name }}{% endif %}!</h1>
<p>Your account is ready: your trips, tickets, details and saved places, all in one place.</p>
{{ button(account_url, "My account") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$مرحبًا بك في {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">مرحبًا{% if first_name %} يا {{ first_name }}{% endif %}!</h1>
<p>حسابك جاهز: رحلاتك وتذاكرك ومعلوماتك والأماكن المحفوظة في مكان واحد.</p>
{{ button(account_url, "حسابي") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('security_new_device', ARRAY['site', 'first_name', 'device', 'when', 'security_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Nouvelle connexion à votre compte {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Nouvelle connexion</h1>
<p>Bonjour {{ first_name }}, votre compte vient d'être utilisé depuis un nouvel appareil :</p>
<p><strong>{{ device }}</strong><br>{{ datetime(when) }}</p>
<p style="font-size:13px;color:#6b6258;">Ce n'est pas vous ? Sécurisez votre compte : déconnectez tous les appareils depuis Connexion et sécurité, ou répondez à cet e-mail.</p>
{{ button(security_url, "Connexion et sécurité") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$New sign-in to your {{ site.name }} account$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">New sign-in</h1>
<p>Hello {{ first_name }}, your account was just used from a new device:</p>
<p><strong>{{ device }}</strong><br>{{ datetime(when) }}</p>
<p style="font-size:13px;color:#6b6258;">Wasn't you? Secure your account: sign out of all devices in Login &amp; security, or reply to this email.</p>
{{ button(security_url, "Login & security") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تسجيل دخول جديد إلى حسابك في {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تسجيل دخول جديد</h1>
<p>مرحبًا {{ first_name }}، تم استخدام حسابك للتو من جهاز جديد:</p>
<p><strong>{{ device }}</strong><br>{{ datetime(when) }}</p>
<p style="font-size:13px;color:#6b6258;">لست أنت؟ أمّن حسابك: سجّل الخروج من جميع الأجهزة من صفحة تسجيل الدخول والأمان، أو ردّ على هذه الرسالة.</p>
{{ button(security_url, "تسجيل الدخول والأمان") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('security_password_changed', ARRAY['site', 'first_name', 'change', 'security_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre mot de passe {{ site.name }} a été modifié$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Mot de passe {% if change == 'added' %}ajouté{% elif change == 'removed' %}supprimé{% else %}modifié{% endif %}</h1>
<p>Bonjour {{ first_name }}, le mot de passe de votre compte vient d'être {% if change == 'added' %}ajouté{% elif change == 'removed' %}supprimé{% else %}modifié{% endif %}.</p>
<p style="font-size:13px;color:#6b6258;">Ce n'est pas vous ? Sécurisez votre compte : déconnectez tous les appareils depuis Connexion et sécurité, ou répondez à cet e-mail.</p>
{{ button(security_url, "Connexion et sécurité") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your {{ site.name }} password was changed$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Password {% if change == 'added' %}added{% elif change == 'removed' %}removed{% else %}changed{% endif %}</h1>
<p>Hello {{ first_name }}, the password of your account was just {% if change == 'added' %}added{% elif change == 'removed' %}removed{% else %}changed{% endif %}.</p>
<p style="font-size:13px;color:#6b6258;">Wasn't you? Secure your account: sign out of all devices in Login &amp; security, or reply to this email.</p>
{{ button(security_url, "Login & security") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تم تغيير كلمة مرور {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">{% if change == 'added' %}تمت إضافة{% elif change == 'removed' %}تم حذف{% else %}تم تغيير{% endif %} كلمة المرور</h1>
<p>مرحبًا {{ first_name }}، {% if change == 'added' %}تمت إضافة{% elif change == 'removed' %}تم حذف{% else %}تم تغيير{% endif %} كلمة مرور حسابك للتو.</p>
<p style="font-size:13px;color:#6b6258;">لست أنت؟ أمّن حسابك: سجّل الخروج من جميع الأجهزة من صفحة تسجيل الدخول والأمان، أو ردّ على هذه الرسالة.</p>
{{ button(security_url, "تسجيل الدخول والأمان") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('email_change_code', ARRAY['site', 'code']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl${{ code }} : confirmez votre nouvelle adresse$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Confirmez votre nouvelle adresse</h1>
<p>Saisissez ce code dans votre compte {{ site.name }} pour utiliser cette adresse :</p>
<p style="margin:24px 0;font-family:Georgia,serif;font-size:32px;letter-spacing:8px;color:#1d1a17;" dir="ltr">{{ code }}</p>
<p style="font-size:13px;color:#6b6258;">Valable 15 minutes. Vous n'avez rien demandé ? Ignorez cet e-mail.</p>
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl${{ code }}: confirm your new address$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Confirm your new address</h1>
<p>Enter this code in your {{ site.name }} account to use this address:</p>
<p style="margin:24px 0;font-family:Georgia,serif;font-size:32px;letter-spacing:8px;color:#1d1a17;" dir="ltr">{{ code }}</p>
<p style="font-size:13px;color:#6b6258;">Valid for 15 minutes. Didn't ask for this? Ignore this email.</p>
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl${{ code }}: أكّد عنوانك الجديد$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">أكّد عنوانك الجديد</h1>
<p>أدخل هذا الرمز في حسابك على {{ site.name }} لاستخدام هذا العنوان:</p>
<p style="margin:24px 0;font-family:Georgia,serif;font-size:32px;letter-spacing:8px;color:#1d1a17;" dir="ltr">{{ code }}</p>
<p style="font-size:13px;color:#6b6258;">صالح لمدة 15 دقيقة. لم تطلب ذلك؟ تجاهل هذه الرسالة.</p>
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('email_change_notice', ARRAY['site', 'first_name', 'new_email', 'security_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Changement d'adresse demandé sur votre compte$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Changement d'adresse demandé</h1>
<p>Bonjour {{ first_name }}, une demande vise à remplacer l'adresse de votre compte par <strong>{{ new_email }}</strong>.</p>
<p style="font-size:13px;color:#6b6258;">Ce n'est pas vous ? Sécurisez votre compte : déconnectez tous les appareils depuis Connexion et sécurité, ou répondez à cet e-mail.</p>
{{ button(security_url, "Connexion et sécurité") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Email change requested on your account$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Email change requested</h1>
<p>Hello {{ first_name }}, someone asked to replace your account's address with <strong>{{ new_email }}</strong>.</p>
<p style="font-size:13px;color:#6b6258;">Wasn't you? Secure your account: sign out of all devices in Login &amp; security, or reply to this email.</p>
{{ button(security_url, "Login & security") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$طلب تغيير البريد الإلكتروني لحسابك$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">طلب تغيير البريد الإلكتروني</h1>
<p>مرحبًا {{ first_name }}، طُلب استبدال عنوان حسابك بـ <strong dir="ltr">{{ new_email }}</strong>.</p>
<p style="font-size:13px;color:#6b6258;">لست أنت؟ أمّن حسابك: سجّل الخروج من جميع الأجهزة من صفحة تسجيل الدخول والأمان، أو ردّ على هذه الرسالة.</p>
{{ button(security_url, "تسجيل الدخول والأمان") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('security_email_changed', ARRAY['site', 'first_name', 'old_email', 'new_email', 'security_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$L'adresse de votre compte {{ site.name }} a changé$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Adresse modifiée</h1>
<p>Bonjour {{ first_name }}, l'adresse de votre compte est maintenant <strong>{{ new_email }}</strong> (avant : {{ old_email }}).</p>
<p style="font-size:13px;color:#6b6258;">Ce n'est pas vous ? Sécurisez votre compte : déconnectez tous les appareils depuis Connexion et sécurité, ou répondez à cet e-mail.</p>
{{ button(security_url, "Connexion et sécurité") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your {{ site.name }} account address changed$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Email changed</h1>
<p>Hello {{ first_name }}, your account address is now <strong>{{ new_email }}</strong> (before: {{ old_email }}).</p>
<p style="font-size:13px;color:#6b6258;">Wasn't you? Secure your account: sign out of all devices in Login &amp; security, or reply to this email.</p>
{{ button(security_url, "Login & security") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تغيّر عنوان حسابك في {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تم تغيير البريد الإلكتروني</h1>
<p>مرحبًا {{ first_name }}، أصبح عنوان حسابك <strong dir="ltr">{{ new_email }}</strong> (سابقًا: <span dir="ltr">{{ old_email }}</span>).</p>
<p style="font-size:13px;color:#6b6258;">لست أنت؟ أمّن حسابك: سجّل الخروج من جميع الأجهزة من صفحة تسجيل الدخول والأمان، أو ردّ على هذه الرسالة.</p>
{{ button(security_url, "تسجيل الدخول والأمان") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('security_identity_linked', ARRAY['site', 'first_name', 'provider', 'security_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl${{ provider }} est lié à votre compte {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Compte {{ provider }} lié</h1>
<p>Bonjour {{ first_name }}, vous pouvez désormais vous connecter avec {{ provider }}.</p>
<p style="font-size:13px;color:#6b6258;">Ce n'est pas vous ? Sécurisez votre compte : déconnectez tous les appareils depuis Connexion et sécurité, ou répondez à cet e-mail.</p>
{{ button(security_url, "Connexion et sécurité") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl${{ provider }} is now linked to your {{ site.name }} account$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">{{ provider }} account linked</h1>
<p>Hello {{ first_name }}, you can now sign in with {{ provider }}.</p>
<p style="font-size:13px;color:#6b6258;">Wasn't you? Secure your account: sign out of all devices in Login &amp; security, or reply to this email.</p>
{{ button(security_url, "Login & security") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تم ربط {{ provider }} بحسابك في {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تم ربط حساب {{ provider }}</h1>
<p>مرحبًا {{ first_name }}، يمكنك الآن تسجيل الدخول باستخدام {{ provider }}.</p>
<p style="font-size:13px;color:#6b6258;">لست أنت؟ أمّن حسابك: سجّل الخروج من جميع الأجهزة من صفحة تسجيل الدخول والأمان، أو ردّ على هذه الرسالة.</p>
{{ button(security_url, "تسجيل الدخول والأمان") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('security_passkey_added', ARRAY['site', 'first_name', 'security_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Une clé d'accès a été ajoutée à votre compte$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Clé d'accès ajoutée</h1>
<p>Bonjour {{ first_name }}, une nouvelle clé d'accès (passkey) permet maintenant de vous connecter.</p>
<p style="font-size:13px;color:#6b6258;">Ce n'est pas vous ? Sécurisez votre compte : déconnectez tous les appareils depuis Connexion et sécurité, ou répondez à cet e-mail.</p>
{{ button(security_url, "Connexion et sécurité") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$A passkey was added to your account$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Passkey added</h1>
<p>Hello {{ first_name }}, a new passkey can now sign in to your account.</p>
<p style="font-size:13px;color:#6b6258;">Wasn't you? Secure your account: sign out of all devices in Login &amp; security, or reply to this email.</p>
{{ button(security_url, "Login & security") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$أضيف مفتاح مرور إلى حسابك$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تمت إضافة مفتاح مرور</h1>
<p>مرحبًا {{ first_name }}، أصبح بالإمكان تسجيل الدخول إلى حسابك بمفتاح مرور جديد.</p>
<p style="font-size:13px;color:#6b6258;">لست أنت؟ أمّن حسابك: سجّل الخروج من جميع الأجهزة من صفحة تسجيل الدخول والأمان، أو ردّ على هذه الرسالة.</p>
{{ button(security_url, "تسجيل الدخول والأمان") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_claim', ARRAY['site', 'reference', 'link']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Ajouter la réservation {{ reference }} à un compte$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Ajouter cette réservation à un compte ?</h1>
<p>Quelqu'un souhaite ajouter la réservation <strong>{{ reference }}</strong> à son compte {{ site.name }}. Si c'est vous, confirmez :</p>
{{ button(link, "Ajouter à mon compte") }}
<p style="font-size:13px;color:#6b6258;">Ce lien est valable 24 heures. Sinon, ignorez cet e-mail : rien ne change.</p>
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Add booking {{ reference }} to an account$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Add this booking to an account?</h1>
<p>Someone wants to add booking <strong>{{ reference }}</strong> to their {{ site.name }} account. If it is you, confirm:</p>
{{ button(link, "Add to my account") }}
<p style="font-size:13px;color:#6b6258;">This link is valid for 24 hours. Otherwise, ignore this email: nothing changes.</p>
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$إضافة الحجز {{ reference }} إلى حساب$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">إضافة هذا الحجز إلى حساب؟</h1>
<p>يرغب شخص ما في إضافة الحجز <strong>{{ reference }}</strong> إلى حسابه في {{ site.name }}. إن كنت أنت، فأكّد:</p>
{{ button(link, "إضافة إلى حسابي") }}
<p style="font-size:13px;color:#6b6258;">هذا الرابط صالح لمدة 24 ساعة. وإلا تجاهل هذه الرسالة: لن يتغير شيء.</p>
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('booking_claimed', ARRAY['site', 'first_name', 'reference', 'trip_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$La réservation {{ reference }} est dans votre compte$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Réservation ajoutée</h1>
<p>Bonjour {{ first_name }}, la réservation <strong>{{ reference }}</strong> fait maintenant partie de vos voyages.</p>
{{ button(trip_url, "Voir le voyage") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Booking {{ reference }} is in your account$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Booking added</h1>
<p>Hello {{ first_name }}, booking <strong>{{ reference }}</strong> is now part of your trips.</p>
{{ button(trip_url, "View the trip") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$الحجز {{ reference }} في حسابك$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تمت إضافة الحجز</h1>
<p>مرحبًا {{ first_name }}، أصبح الحجز <strong>{{ reference }}</strong> ضمن رحلاتك.</p>
{{ button(trip_url, "عرض الرحلة") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('account_export_ready', ARRAY['site', 'first_name', 'link', 'days']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Vos données {{ site.name }} sont prêtes$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Vos données sont prêtes</h1>
<p>Bonjour {{ first_name }}, l'export de vos données est prêt. Le lien est valable {{ days }} jours.</p>
{{ button(link, "Télécharger mes données") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your {{ site.name }} data is ready$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Your data is ready</h1>
<p>Hello {{ first_name }}, your data export is ready. The link is valid for {{ days }} days.</p>
{{ button(link, "Download my data") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$بياناتك في {{ site.name }} جاهزة$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">بياناتك جاهزة</h1>
<p>مرحبًا {{ first_name }}، ملف بياناتك جاهز. الرابط صالح لمدة {{ days }} أيام.</p>
{{ button(link, "تحميل بياناتي") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('account_deletion_scheduled', ARRAY['site', 'first_name', 'scheduled_for', 'cancel_url']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Suppression de votre compte {{ site.name }} programmée$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Suppression programmée</h1>
<p>Bonjour {{ first_name }}, votre compte sera supprimé le <strong>{{ date(scheduled_for) }}</strong>. Vos réservations et factures sont conservées de façon anonyme pour nos obligations légales.</p>
<p>Vous avez changé d'avis ?</p>
{{ button(cancel_url, "Annuler la suppression") }}
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Deletion of your {{ site.name }} account scheduled$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Deletion scheduled</h1>
<p>Hello {{ first_name }}, your account will be deleted on <strong>{{ date(scheduled_for) }}</strong>. Bookings and invoices are kept anonymised for legal reasons.</p>
<p>Changed your mind?</p>
{{ button(cancel_url, "Cancel the deletion") }}
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تمت جدولة حذف حسابك في {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تمت جدولة الحذف</h1>
<p>مرحبًا {{ first_name }}، سيُحذف حسابك في <strong>{{ date(scheduled_for) }}</strong>. تُحفظ الحجوزات والفواتير بشكل مجهول لأسباب قانونية.</p>
<p>غيّرت رأيك؟</p>
{{ button(cancel_url, "إلغاء الحذف") }}
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

WITH t AS (
    INSERT INTO email_templates (code, variables) VALUES ('account_deleted', ARRAY['site']) RETURNING id
)
INSERT INTO email_template_translations (email_template_id, locale, subject, body_html, body_text)
SELECT t.id, v.locale, v.subject, v.body_html, ''
FROM t, (VALUES
    ('fr', $tpl$Votre compte {{ site.name }} a été supprimé$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Compte supprimé</h1>
<p>Votre compte et vos données personnelles ont été supprimés. Merci d'avoir voyagé avec nous : vous serez toujours le bienvenu à M'ssici.</p>
<p>L'équipe {{ site.name }}</p>$tpl$),
    ('en', $tpl$Your {{ site.name }} account was deleted$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">Account deleted</h1>
<p>Your account and personal data were deleted. Thank you for travelling with us: you are always welcome in M'ssici.</p>
<p>The {{ site.name }} team</p>$tpl$),
    ('ar', $tpl$تم حذف حسابك في {{ site.name }}$tpl$, $tpl$<h1 style="margin:0 0 16px 0;font-family:Georgia,'Times New Roman',serif;font-size:22px;line-height:1.3;color:#1d1a17;">تم حذف الحساب</h1>
<p>تم حذف حسابك وبياناتك الشخصية. شكرًا لسفرك معنا: مرحبًا بك دائمًا في مسيسي.</p>
<p>فريق {{ site.name }}</p>$tpl$)
) AS v (locale, subject, body_html);

