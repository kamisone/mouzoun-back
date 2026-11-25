//! Mouzoun's own legal pages (terms of sale, legal notice, privacy, cookies, withdrawal,
//! delivery): templates in `legal/pages/{key}.{locale}.html`, installed as `legal` pages with a
//! published revision (`mouzoun-cli legal-pages install`), and **tokens** such as
//! `{{seller.legal_name}}` filled at render time from the data (seller identity, mediator,
//! settings, shipping rates, withdrawal exemptions, the cookie inventory). An unknown token
//! fails the publication of a legal page. The admin edits the texts freely in the builder.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgConnection;

/// The six pages: settings key `legal.{key}_page_id`, and per locale (title, slug).
/// (locale, title, slug)
pub type PageText = (&'static str, &'static str, &'static str);

pub const PAGES: [(&str, [PageText; 3]); 6] = [
    (
        "cgv",
        [
            (
                "fr",
                "Conditions générales de vente",
                "conditions-generales-de-vente",
            ),
            ("en", "Terms of sale", "terms-of-sale"),
            ("ar", "الشروط العامة للبيع", "shurut-albay"),
        ],
    ),
    (
        "notice",
        [
            ("fr", "Mentions légales", "mentions-legales"),
            ("en", "Legal notice", "legal-notice"),
            ("ar", "إشعار قانوني", "ishaar-qanouni"),
        ],
    ),
    (
        "privacy",
        [
            ("fr", "Politique de confidentialité", "confidentialite"),
            ("en", "Privacy policy", "privacy-policy"),
            ("ar", "سياسة الخصوصية", "siyasat-alkhususiya"),
        ],
    ),
    (
        "cookies",
        [
            ("fr", "Politique cookies", "cookies"),
            ("en", "Cookie policy", "cookie-policy"),
            ("ar", "سياسة ملفات تعريف الارتباط", "cookies-ar"),
        ],
    ),
    (
        "withdrawal",
        [
            ("fr", "Droit de rétractation", "droit-de-retractation"),
            ("en", "Right of withdrawal", "right-of-withdrawal"),
            ("ar", "حق التراجع", "haq-attarajou"),
        ],
    ),
    (
        "shipping",
        [
            ("fr", "Livraison et retours", "livraison-et-retours"),
            ("en", "Delivery and returns", "delivery-and-returns"),
            ("ar", "التوصيل والإرجاع", "attawsil-walirja"),
        ],
    ),
];

/// The template of a page in a locale.
pub fn template(key: &str, locale: &str) -> Option<&'static str> {
    Some(match (key, locale) {
        ("cgv", "fr") => include_str!("../../../legal/pages/cgv.fr.html"),
        ("cgv", "en") => include_str!("../../../legal/pages/cgv.en.html"),
        ("cgv", "ar") => include_str!("../../../legal/pages/cgv.ar.html"),
        ("notice", "fr") => include_str!("../../../legal/pages/notice.fr.html"),
        ("notice", "en") => include_str!("../../../legal/pages/notice.en.html"),
        ("notice", "ar") => include_str!("../../../legal/pages/notice.ar.html"),
        ("privacy", "fr") => include_str!("../../../legal/pages/privacy.fr.html"),
        ("privacy", "en") => include_str!("../../../legal/pages/privacy.en.html"),
        ("privacy", "ar") => include_str!("../../../legal/pages/privacy.ar.html"),
        ("cookies", "fr") => include_str!("../../../legal/pages/cookies.fr.html"),
        ("cookies", "en") => include_str!("../../../legal/pages/cookies.en.html"),
        ("cookies", "ar") => include_str!("../../../legal/pages/cookies.ar.html"),
        ("withdrawal", "fr") => include_str!("../../../legal/pages/withdrawal.fr.html"),
        ("withdrawal", "en") => include_str!("../../../legal/pages/withdrawal.en.html"),
        ("withdrawal", "ar") => include_str!("../../../legal/pages/withdrawal.ar.html"),
        ("shipping", "fr") => include_str!("../../../legal/pages/shipping.fr.html"),
        ("shipping", "en") => include_str!("../../../legal/pages/shipping.en.html"),
        ("shipping", "ar") => include_str!("../../../legal/pages/shipping.ar.html"),
        _ => return None,
    })
}

/// Every token a legal page may use.
pub const KNOWN_TOKENS: &[&str] = &[
    "seller.legal_name",
    "seller.legal_form",
    "seller.share_capital",
    "seller.address",
    "seller.siren",
    "seller.siret",
    "seller.rcs_city",
    "seller.vat_number",
    "seller.ape_code",
    "seller.email",
    "seller.phone",
    "seller.publication_director",
    "host.name",
    "host.address",
    "host.phone",
    "mediator.name",
    "mediator.url",
    "mediator.address",
    "shop.name",
    "shop.site_url",
    "shop.withdrawal_days",
    "shop.refund_deadline_days",
    "shop.return_costs",
    "privacy.consent_months",
    "privacy.accounting_years",
    "privacy.prospects_years",
    "privacy.abandoned_carts_days",
    "privacy.customers_marketing_years",
    "privacy.cndp_receipt",
    "withdrawal.exemptions",
    "withdrawal.form",
    "withdrawal.form_pdf_url",
    "withdrawal.form_pdf_link",
    "shipping.rates",
    "shipping.customs",
    "cookies.table",
];

/// Token names in a text, in order (`{{ name }}`, spaces allowed inside the braces).
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        out.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    out
}

/// Tokens of any string in a JSON value (a page snapshot) that are not in [`KNOWN_TOKENS`].
pub fn unknown_tokens(v: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.extend(
                tokens(s)
                    .into_iter()
                    .filter(|t| !KNOWN_TOKENS.contains(&t.as_str())),
            ),
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(m) => m.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Replaces the tokens of a text. Unknown tokens stay as they are (publication refuses them).
/// Two passes: generated blocks (the withdrawal form) hold tokens themselves.
pub fn render(text: &str, values: &HashMap<String, String>) -> String {
    let once = render_once(text, values);
    if once.contains("{{") {
        render_once(&once, values)
    } else {
        once
    }
}

fn render_once(text: &str, values: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let name = after[..end].trim();
        out.push_str(&rest[..start]);
        match values.get(name) {
            Some(v) => out.push_str(v),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Renders the tokens of every string of a section's content.
pub fn render_content(v: &mut Value, values: &HashMap<String, String>) {
    match v {
        Value::String(s) => {
            if s.contains("{{") {
                *s = render(s, values);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| render_content(x, values)),
        Value::Object(m) => m.values_mut().for_each(|x| render_content(x, values)),
        _ => {}
    }
}

// ---- Cookie inventory -----------------------------------------------------------------------------

/// Where the browser keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    Cookie,
    Local,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Strictly necessary (no consent): session, cart, security, language, the consent itself.
    Necessary,
    /// Remembers a choice the visitor made (theme, saved filters): exempt as requested by them.
    Preferences,
    /// Audience measurement: only after consent.
    Analytics,
}

impl Category {
    pub fn code(self) -> &'static str {
        match self {
            Self::Necessary => "necessary",
            Self::Preferences => "preferences",
            Self::Analytics => "analytics",
        }
    }
}

/// One cookie or storage key the site writes. `name` ending in `*` is a prefix.
#[derive(Debug, Clone, Copy)]
pub struct CookieDecl {
    pub name: &'static str,
    pub store: Store,
    pub category: Category,
    /// (fr, en, ar)
    pub purpose: (&'static str, &'static str, &'static str),
    pub duration: (&'static str, &'static str, &'static str),
}

macro_rules! c {
    ($n:expr, $s:ident, $c:ident, $p:expr, $d:expr) => {
        CookieDecl {
            name: $n,
            store: Store::$s,
            category: Category::$c,
            purpose: $p,
            duration: $d,
        }
    };
}

const SESSION: (&str, &str, &str) = ("Session du navigateur", "Browser session", "جلسة المتصفح");
const KEPT: (&str, &str, &str) = ("Jusqu'à suppression", "Until deleted", "إلى حين الحذف");

/// Everything the site stores on the visitor's device (inventory of the code, checked by a test
/// against the front-end sources). Admin-only keys are listed too: staff are visitors as well.
pub const COOKIES: &[CookieDecl] = &[
    c!(
        "__Host-mz_at",
        Cookie,
        Necessary,
        (
            "Connexion au compte client (jeton d'accès)",
            "Customer sign-in (access token)",
            "تسجيل الدخول إلى حساب الزبون"
        ),
        ("15 minutes", "15 minutes", "15 دقيقة")
    ),
    c!(
        "__Host-mz_rt",
        Cookie,
        Necessary,
        (
            "Maintien de la connexion au compte client",
            "Keeps the customer signed in",
            "إبقاء الزبون متصلاً"
        ),
        (
            "Session, ou 60 jours avec « rester connecté »",
            "Session, or 60 days with \"stay signed in\"",
            "الجلسة، أو 60 يوماً"
        )
    ),
    c!(
        "__Host-mz_cart",
        Cookie,
        Necessary,
        ("Panier de réservation", "Booking cart", "سلة الحجز"),
        ("7 jours", "7 days", "7 أيام")
    ),
    c!(
        "__Host-mz_shop_cart",
        Cookie,
        Necessary,
        ("Panier de la boutique", "Shop cart", "سلة المتجر"),
        ("30 jours", "30 days", "30 يوماً")
    ),
    c!(
        "__Host-mouzoun_session",
        Cookie,
        Necessary,
        (
            "Session de l'équipe (administration)",
            "Staff session (administration)",
            "جلسة الفريق (الإدارة)"
        ),
        SESSION
    ),
    c!(
        "mz-locale",
        Cookie,
        Necessary,
        ("Langue choisie", "Chosen language", "اللغة المختارة"),
        ("1 an", "1 year", "سنة واحدة")
    ),
    c!(
        "mz-admin-locale",
        Cookie,
        Necessary,
        (
            "Langue de l'administration",
            "Administration language",
            "لغة الإدارة"
        ),
        ("1 an", "1 year", "سنة واحدة")
    ),
    c!(
        "mz-dismissed-banners",
        Cookie,
        Preferences,
        (
            "Bandeaux d'information fermés",
            "Dismissed information banners",
            "اللافتات المغلقة"
        ),
        SESSION
    ),
    c!(
        "mz-consent",
        Local,
        Necessary,
        (
            "Votre choix en matière de cookies",
            "Your cookie choice",
            "اختيارك بشأن ملفات تعريف الارتباط"
        ),
        ("6 mois", "6 months", "6 أشهر")
    ),
    c!(
        "mz-consent-at",
        Local,
        Necessary,
        (
            "Date de ce choix",
            "Date of that choice",
            "تاريخ هذا الاختيار"
        ),
        ("6 mois", "6 months", "6 أشهر")
    ),
    c!(
        "mz-color-mode",
        Local,
        Preferences,
        (
            "Thème clair ou sombre",
            "Light or dark theme",
            "المظهر الفاتح أو الداكن"
        ),
        KEPT
    ),
    c!(
        "mz-color-mode-explicit",
        Local,
        Preferences,
        (
            "Thème choisi explicitement",
            "Theme chosen explicitly",
            "المظهر المختار"
        ),
        KEPT
    ),
    c!(
        "mz-signed-in",
        Local,
        Necessary,
        (
            "Indique qu'une connexion est ouverte (sans donnée personnelle)",
            "Signals an open sign-in (no personal data)",
            "يشير إلى تسجيل دخول مفتوح"
        ),
        KEPT
    ),
    c!(
        "mz-saved",
        Local,
        Preferences,
        (
            "Favoris (séjours et activités)",
            "Saved stays and activities",
            "المفضلة"
        ),
        KEPT
    ),
    c!(
        "mz-booking-access",
        Local,
        Necessary,
        (
            "Accès à vos réservations sans compte",
            "Access to your bookings without an account",
            "الوصول إلى حجوزاتك دون حساب"
        ),
        KEPT
    ),
    c!(
        "mz-last-search",
        Local,
        Preferences,
        (
            "Dernière recherche de séjour",
            "Last stay search",
            "آخر بحث"
        ),
        KEPT
    ),
    c!(
        "mz-purchase-*",
        Local,
        Analytics,
        (
            "Évite de compter deux fois un achat (mesure d'audience)",
            "Avoids counting a purchase twice (audience measurement)",
            "تجنب احتساب الشراء مرتين"
        ),
        KEPT
    ),
    c!(
        "mz-last-basket",
        Session,
        Necessary,
        (
            "Retour au panier après le paiement",
            "Back to the cart after payment",
            "العودة إلى السلة بعد الدفع"
        ),
        SESSION
    ),
    c!(
        "mz-checkout-form",
        Session,
        Necessary,
        (
            "Saisie du formulaire de commande en cours",
            "Checkout form in progress",
            "نموذج الطلب الجاري"
        ),
        SESSION
    ),
    c!(
        "mz-admin-booking-views",
        Local,
        Preferences,
        (
            "Vues enregistrées (administration)",
            "Saved views (administration)",
            "طرق العرض المحفوظة (الإدارة)"
        ),
        KEPT
    ),
    c!(
        "mz-shop-order-views",
        Local,
        Preferences,
        (
            "Vues enregistrées (administration)",
            "Saved views (administration)",
            "طرق العرض المحفوظة (الإدارة)"
        ),
        KEPT
    ),
    c!(
        "mz-admin-shop-categories-collapsed",
        Local,
        Preferences,
        (
            "Arborescence repliée (administration)",
            "Collapsed tree (administration)",
            "الشجرة المطوية (الإدارة)"
        ),
        KEPT
    ),
];

/// The categories the consent banner knows (anything else is undeclared).
pub const COOKIE_CATEGORIES: [Category; 3] = [
    Category::Necessary,
    Category::Preferences,
    Category::Analytics,
];

/// Is `key` declared (exactly or by a `*` prefix)?
pub fn cookie_declared(key: &str) -> Option<&'static CookieDecl> {
    COOKIES.iter().find(|c| match c.name.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => c.name == key,
    })
}

fn pick<'a>(t: (&'a str, &'a str, &'a str), locale: &str) -> &'a str {
    match locale {
        "fr" => t.0,
        "ar" => t.2,
        _ => t.1,
    }
}

fn cookies_table(locale: &str) -> String {
    let head = pick(
        (
            "Nom|Type|Finalité|Catégorie|Durée",
            "Name|Type|Purpose|Category|Duration",
            "الاسم|النوع|الغرض|الفئة|المدة",
        ),
        locale,
    );
    let store = |s: Store| match s {
        Store::Cookie => pick(("Cookie", "Cookie", "ملف تعريف ارتباط"), locale),
        Store::Local => pick(("Stockage local", "Local storage", "تخزين محلي"), locale),
        Store::Session => pick(
            ("Stockage de session", "Session storage", "تخزين الجلسة"),
            locale,
        ),
    };
    let category = |c: Category| match c {
        Category::Necessary => pick(
            ("Strictement nécessaire", "Strictly necessary", "ضروري"),
            locale,
        ),
        Category::Preferences => pick(
            (
                "Préférence choisie par vous",
                "Your own preference",
                "تفضيلك",
            ),
            locale,
        ),
        Category::Analytics => pick(
            (
                "Mesure d'audience (avec consentement)",
                "Audience measurement (with consent)",
                "قياس الجمهور (بموافقة)",
            ),
            locale,
        ),
    };
    let mut h = String::from("<table><thead><tr>");
    for col in head.split('|') {
        h.push_str(&format!("<th>{}</th>", esc(col)));
    }
    h.push_str("</tr></thead><tbody>");
    for c in COOKIES {
        h.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            esc(c.name),
            esc(store(c.store)),
            esc(pick(c.purpose, locale)),
            esc(category(c.category)),
            esc(pick(c.duration, locale))
        ));
    }
    h.push_str("</tbody></table>");
    h
}

// ---- Values ---------------------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Shown for a value still missing (the compliance checklist blocks opening meanwhile).
pub fn missing(locale: &str) -> &'static str {
    pick(
        ("[à compléter]", "[to be completed]", "[يُستكمل لاحقاً]"),
        locale,
    )
}

fn money(minor: i64, locale: &str) -> String {
    let euros = minor / 100;
    let cents = minor % 100;
    let mut digits = euros.to_string();
    let mut grouped = String::new();
    while digits.len() > 3 {
        let tail = digits.split_off(digits.len() - 3);
        grouped = format!("\u{202f}{tail}{grouped}");
    }
    let whole = format!("{digits}{grouped}");
    let sep = if locale == "en" { "." } else { "," };
    if cents == 0 {
        if locale == "en" {
            format!("€{whole}")
        } else {
            format!("{whole} €")
        }
    } else if locale == "en" {
        format!("€{whole}{sep}{cents:02}")
    } else {
        format!("{whole}{sep}{cents:02} €")
    }
}

/// URLs the pages link to.
pub struct Urls<'a> {
    pub site: &'a str,
    pub api: &'a str,
}

/// Token → HTML value for a locale.
pub async fn values(
    conn: &mut PgConnection,
    locale: &str,
    urls: &Urls<'_>,
) -> sqlx::Result<HashMap<String, String>> {
    let mut v: HashMap<String, String> = HashMap::new();
    let miss = missing(locale);
    let mut text = |k: &str, s: Option<&str>| {
        let s = s.map(str::trim).filter(|s| {
            let l = s.to_lowercase();
            !s.is_empty() && !l.contains("compléter")
        });
        v.insert(k.into(), s.map(esc).unwrap_or_else(|| miss.into()));
    };

    let seller = sqlx::query!(
        r#"SELECT legal_name, legal_form, share_capital_minor, address, registration, email::text AS email, phone,
                  publication_director, host
           FROM legal_entities WHERE id = shop_entity_id()"#
    )
    .fetch_optional(&mut *conn)
    .await?;
    let s = |j: &Value, k: &str| j[k].as_str().map(str::to_string);
    if let Some(e) = &seller {
        text("seller.legal_name", Some(&e.legal_name));
        text("seller.legal_form", Some(&e.legal_form));
        let capital = e.share_capital_minor.map(|c| money(c, locale));
        text("seller.share_capital", capital.as_deref());
        let a = &e.address;
        let address = [
            s(a, "line1"),
            s(a, "line2"),
            s(a, "postal_code").map(|p| format!("{p} {}", s(a, "city").unwrap_or_default())),
            s(a, "country"),
        ]
        .into_iter()
        .flatten()
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
        let address_ok = !s(a, "line1")
            .unwrap_or_default()
            .to_lowercase()
            .contains("compléter");
        text("seller.address", address_ok.then_some(address.as_str()));
        let r = &e.registration;
        for k in ["siren", "siret", "rcs_city", "vat_number", "ape_code"] {
            text(&format!("seller.{k}"), s(r, k).as_deref());
        }
        text("seller.email", e.email.as_deref());
        text("seller.phone", e.phone.as_deref());
        text(
            "seller.publication_director",
            e.publication_director.as_deref(),
        );
        for k in ["name", "address", "phone"] {
            text(&format!("host.{k}"), s(&e.host, k).as_deref());
        }
    }

    let settings: HashMap<String, Value> = sqlx::query!(
        "SELECT key, value FROM settings WHERE key = ANY($1)",
        &[
            "shop.withdrawal_days",
            "shop.refund_deadline_days",
            "shop.return_costs_paid_by",
            "shop.consumer_mediator",
            "shop.legal_entity",
            "bookings.legal_entity",
            "privacy.data_retention",
            "legal.cndp_receipt",
        ]
        .map(String::from)[..]
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.key, r.value))
    .collect();
    let num = |k: &str| settings.get(k).and_then(Value::as_i64);
    let m = settings
        .get("shop.consumer_mediator")
        .cloned()
        .unwrap_or(Value::Null);
    for k in ["name", "url", "address"] {
        text(&format!("mediator.{k}"), m[k].as_str());
    }
    text("shop.name", Some("Mouzoun"));
    text("shop.site_url", Some(urls.site));
    let withdrawal_days = num("shop.withdrawal_days").unwrap_or(14);
    text("shop.withdrawal_days", Some(&withdrawal_days.to_string()));
    text(
        "shop.refund_deadline_days",
        Some(&num("shop.refund_deadline_days").unwrap_or(14).to_string()),
    );
    let seller_pays = settings
        .get("shop.return_costs_paid_by")
        .and_then(Value::as_str)
        == Some("seller");
    let return_costs = if seller_pays {
        pick(
            (
                "Nous prenons en charge les frais de renvoi des biens.",
                "We pay the cost of returning the goods.",
                "نتحمل مصاريف إرجاع المنتجات.",
            ),
            locale,
        )
    } else {
        pick(
            (
                "Les frais directs de renvoi des biens sont à votre charge.",
                "You bear the direct cost of returning the goods.",
                "تتحمل المصاريف المباشرة لإرجاع المنتجات.",
            ),
            locale,
        )
    };
    text("shop.return_costs", Some(return_costs));
    let retention = settings
        .get("privacy.data_retention")
        .cloned()
        .unwrap_or(Value::Null);
    for (token, key) in [
        ("privacy.consent_months", "consent_months"),
        ("privacy.accounting_years", "accounting_years"),
        ("privacy.prospects_years", "prospects_years"),
        ("privacy.abandoned_carts_days", "abandoned_carts_days"),
        (
            "privacy.customers_marketing_years",
            "customers_marketing_years",
        ),
    ] {
        text(
            token,
            retention[key].as_i64().map(|n| n.to_string()).as_deref(),
        );
    }
    text(
        "privacy.cndp_receipt",
        settings.get("legal.cndp_receipt").and_then(Value::as_str),
    );

    // Generated blocks (already HTML).
    let bookings_sold_here = settings.contains_key("bookings.legal_entity")
        && settings.get("bookings.legal_entity") == settings.get("shop.legal_entity");
    v.insert(
        "withdrawal.exemptions".into(),
        withdrawal_exemptions(conn, locale, bookings_sold_here).await?,
    );
    v.insert("withdrawal.form".into(), withdrawal_form_html(locale));
    let pdf_url = esc(&format!(
        "{}/v1/shop/legal/withdrawal-form.pdf",
        urls.api.trim_end_matches('/')
    ));
    v.insert(
        "withdrawal.form_pdf_link".into(),
        format!(
            "<a href=\"{pdf_url}\" target=\"_blank\" rel=\"noopener\">{}</a>",
            esc(pick(
                (
                    "Télécharger le formulaire (PDF)",
                    "Download the form (PDF, in French)",
                    "تحميل النموذج (PDF بالفرنسية)"
                ),
                locale
            ))
        ),
    );
    v.insert("withdrawal.form_pdf_url".into(), pdf_url);
    let (rates, customs) = shipping_blocks(conn, locale).await?;
    v.insert("shipping.rates".into(), rates);
    v.insert("shipping.customs".into(), customs);
    v.insert("cookies.table".into(), cookies_table(locale));
    Ok(v)
}

/// The exemptions of article L221-28 that apply to products on sale, with the products
/// concerned (from `shop_products.withdrawal_exemption`), and the dated leisure services when the
/// same company sells the bookings.
pub async fn withdrawal_exemptions(
    conn: &mut PgConnection,
    locale: &str,
    dated_services: bool,
) -> sqlx::Result<String> {
    let rows = sqlx::query!(
        r#"SELECT sp.withdrawal_exemption AS "exemption!",
                  COALESCE(NULLIF(btrim(t.name), ''), NULLIF(btrim(f.name), ''), p.id::text) AS "name!"
           FROM shop_products sp JOIN products p ON p.id = sp.product_id
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $1
           LEFT JOIN product_translations f ON f.product_id = p.id AND f.locale = 'fr'
           WHERE p.status = 'published' AND p.deleted_at IS NULL AND sp.withdrawal_exemption <> 'none'
           ORDER BY 1, 2"#,
        locale
    )
    .fetch_all(conn)
    .await?;
    let label = |code: &str| match code {
        "perishable" => pick(
            (
                "les biens susceptibles de se détériorer ou de se périmer rapidement (article L221-28 4°)",
                "goods liable to deteriorate or expire rapidly (article L221-28 4°)",
                "المنتجات القابلة للتلف أو انتهاء الصلاحية بسرعة",
            ),
            locale,
        ),
        "unsealed_hygiene" => pick(
            (
                "les biens descellés après la livraison, qui ne peuvent être renvoyés pour des raisons d'hygiène ou de protection de la santé (article L221-28 5°)",
                "sealed goods unsealed after delivery that cannot be returned for health or hygiene reasons (article L221-28 5°)",
                "المنتجات المختومة التي فُتحت بعد التسليم ولا يمكن إرجاعها لأسباب صحية",
            ),
            locale,
        ),
        "personalised" => pick(
            (
                "les biens confectionnés selon vos spécifications ou nettement personnalisés (article L221-28 3°)",
                "goods made to your specifications or clearly personalised (article L221-28 3°)",
                "المنتجات المصنوعة حسب طلبك أو المخصصة بوضوح",
            ),
            locale,
        ),
        _ => "",
    };
    let mut html = String::from("<ul>");
    let mut current: Option<String> = None;
    let mut names: Vec<String> = Vec::new();
    let flush = |code: &Option<String>, names: &mut Vec<String>, html: &mut String| {
        if let Some(c) = code {
            html.push_str(&format!(
                "<li>{} : {}.</li>",
                esc(label(c)),
                names.iter().map(|n| esc(n)).collect::<Vec<_>>().join(", ")
            ));
        }
        names.clear();
    };
    for r in rows {
        if current.as_deref() != Some(r.exemption.as_str()) {
            flush(&current, &mut names, &mut html);
            current = Some(r.exemption.clone());
        }
        names.push(r.name);
    }
    flush(&current, &mut names, &mut html);
    if dated_services {
        html.push_str(&format!(
            "<li>{}</li>",
            esc(pick(("les prestations d'hébergement et de loisirs fournies à une date ou à une période déterminée (article L221-28 12°).", "accommodation and leisure services provided on a specific date or period (article L221-28 12°).", "خدمات الإقامة والترفيه المقدمة في تاريخ أو فترة محددة."), locale))
        ));
    }
    if html == "<ul>" {
        return Ok(format!(
            "<p>{}</p>",
            esc(pick(
                (
                    "Aucun produit actuellement en vente n'est concerné.",
                    "No product currently on sale is concerned.",
                    "لا يوجد حالياً أي منتج معني."
                ),
                locale
            ))
        ));
    }
    html.push_str("</ul>");
    Ok(html)
}

/// The model withdrawal form (annex to article R221-1), as HTML.
pub fn withdrawal_form_html(locale: &str) -> String {
    let lines: &[&str] = match locale {
        "fr" => &[
            "(Veuillez compléter et renvoyer le présent formulaire uniquement si vous souhaitez vous rétracter du contrat.)",
            "À l'attention de {{seller.legal_name}}, {{seller.address}}, {{seller.email}} :",
            "Je/nous (*) vous notifie/notifions (*) par la présente ma/notre (*) rétractation du contrat portant sur la vente du bien (*)/pour la prestation de services (*) ci-dessous :",
            "Commandé le (*)/reçu le (*) :",
            "Nom du (des) consommateur(s) :",
            "Adresse du (des) consommateur(s) :",
            "Signature du (des) consommateur(s) (uniquement en cas de notification du présent formulaire sur papier) :",
            "Date :",
            "(*) Rayez la mention inutile.",
        ],
        "ar" => &[
            "(املأ هذا النموذج وأرسله فقط إذا كنت ترغب في التراجع عن العقد.)",
            "إلى {{seller.legal_name}}، {{seller.address}}، {{seller.email}}:",
            "أُبلغكم بموجب هذا بتراجعي عن عقد بيع المنتج أدناه:",
            "تاريخ الطلب / تاريخ الاستلام:",
            "اسم المستهلك:",
            "عنوان المستهلك:",
            "توقيع المستهلك (فقط في حالة الإرسال الورقي):",
            "التاريخ:",
        ],
        _ => &[
            "(Complete and return this form only if you wish to withdraw from the contract.)",
            "To {{seller.legal_name}}, {{seller.address}}, {{seller.email}}:",
            "I/We (*) hereby give notice that I/We (*) withdraw from my/our (*) contract of sale of the following goods (*)/for the provision of the following service (*):",
            "Ordered on (*)/received on (*):",
            "Name of consumer(s):",
            "Address of consumer(s):",
            "Signature of consumer(s) (only if this form is notified on paper):",
            "Date:",
            "(*) Delete as appropriate.",
        ],
    };
    let mut html = String::from("<blockquote>");
    for l in lines {
        html.push_str(&format!("<p>{l}</p>"));
    }
    html.push_str("</blockquote>");
    html
}

/// The delivery table (active, confirmed rates of active zones) and the customs notice (zones
/// that require customs).
pub async fn shipping_blocks(
    conn: &mut PgConnection,
    locale: &str,
) -> sqlx::Result<(String, String)> {
    let rates = sqlx::query!(
        r#"SELECT COALESCE(zt.name, zf.name, z.code::text) AS "zone!",
                  COALESCE(rt.name, rf.name, r.code::text) AS "name!",
                  r.carrier, r.delivery_type, r.kind, r.amount_minor, r.per_kg_minor,
                  r.min_weight_grams, r.max_weight_grams, r.free_over_minor,
                  r.delivery_days_min, r.delivery_days_max
           FROM shipping_rates r JOIN shipping_zones z ON z.id = r.zone_id
           LEFT JOIN shipping_zone_translations zt ON zt.zone_id = z.id AND zt.locale = $1
           LEFT JOIN shipping_zone_translations zf ON zf.zone_id = z.id AND zf.locale = 'fr'
           LEFT JOIN shipping_rate_translations rt ON rt.rate_id = r.id AND rt.locale = $1
           LEFT JOIN shipping_rate_translations rf ON rf.rate_id = r.id AND rf.locale = 'fr'
           WHERE r.is_active AND z.is_active AND NOT r.is_placeholder
           ORDER BY z.sort_order, r.sort_order, r.min_weight_grams NULLS FIRST"#,
        locale
    )
    .fetch_all(&mut *conn)
    .await?;
    let table = if rates.is_empty() {
        format!(
            "<p>{}</p>",
            esc(pick(
                (
                    "Les tarifs de livraison seront publiés ici avant l'ouverture de la boutique.",
                    "Delivery prices will be published here before the shop opens.",
                    "ستُنشر أسعار التوصيل هنا قبل افتتاح المتجر."
                ),
                locale
            ))
        )
    } else {
        let head = pick(
            (
                "Zone|Mode|Poids|Prix|Délai",
                "Area|Method|Weight|Price|Delivery time",
                "المنطقة|الطريقة|الوزن|السعر|المدة",
            ),
            locale,
        );
        let mut h = String::from("<table><thead><tr>");
        for col in head.split('|') {
            h.push_str(&format!("<th>{}</th>", esc(col)));
        }
        h.push_str("</tr></thead><tbody>");
        let kg = |g: i32| {
            let x = g as f64 / 1000.0;
            if x.fract() == 0.0 {
                format!("{x:.0}")
            } else {
                format!("{x}").replace('.', if locale == "en" { "." } else { "," })
            }
        };
        for r in &rates {
            let weight = match (r.min_weight_grams, r.max_weight_grams) {
                (Some(a), Some(b)) => format!("{}–{} kg", kg(a), kg(b)),
                (None, Some(b)) => format!("≤ {} kg", kg(b)),
                (Some(a), None) => format!("> {} kg", kg(a)),
                (None, None) => "—".into(),
            };
            let mut price = match r.kind.as_str() {
                "free" => pick(("Offerte", "Free", "مجاني"), locale).to_string(),
                _ => r
                    .amount_minor
                    .map(|a| money(a, locale))
                    .unwrap_or_else(|| "—".into()),
            };
            if let Some(f) = r.free_over_minor {
                price.push_str(&format!(
                    " ({} {})",
                    pick(("offerte dès", "free from", "مجاني ابتداءً من"), locale),
                    money(f, locale)
                ));
            }
            let days = match (r.delivery_days_min, r.delivery_days_max) {
                (Some(a), Some(b)) if a != b => format!(
                    "{a}–{b} {}",
                    pick(("jours ouvrés", "working days", "أيام عمل"), locale)
                ),
                (Some(a), _) | (None, Some(a)) => format!(
                    "{a} {}",
                    pick(("jours ouvrés", "working days", "أيام عمل"), locale)
                ),
                _ => "—".into(),
            };
            let mode = match r.delivery_type.as_str() {
                "relay_point" => pick(
                    ("en point relais", "to a pick-up point", "إلى نقطة استلام"),
                    locale,
                ),
                _ => pick(("à domicile", "to your home", "إلى المنزل"), locale),
            };
            let method = format!("{} — {}", r.name, mode);
            h.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&r.zone),
                esc(&method),
                esc(&weight),
                esc(&price),
                esc(&days)
            ));
        }
        h.push_str("</tbody></table>");
        h
    };
    let customs_zones: Vec<String> = sqlx::query_scalar!(
        r#"SELECT COALESCE(zt.name, zf.name, z.code::text) AS "zone!" FROM shipping_zones z
           LEFT JOIN shipping_zone_translations zt ON zt.zone_id = z.id AND zt.locale = $1
           LEFT JOIN shipping_zone_translations zf ON zf.zone_id = z.id AND zf.locale = 'fr'
           WHERE z.is_active AND z.requires_customs
             AND EXISTS (SELECT 1 FROM shipping_rates r WHERE r.zone_id = z.id AND r.is_active AND NOT r.is_placeholder)
           ORDER BY z.sort_order"#,
        locale
    )
    .fetch_all(&mut *conn)
    .await?;
    let customs = if customs_zones.is_empty() {
        String::new()
    } else {
        format!(
            "<h3>{}</h3><p>{} {}</p>",
            esc(pick(
                ("Formalités douanières", "Customs", "الإجراءات الجمركية"),
                locale
            )),
            esc(pick(
                (
                    "Les envois vers les zones suivantes passent la douane :",
                    "Parcels to the following areas go through customs:",
                    "تمر الشحنات إلى المناطق التالية عبر الجمارك:"
                ),
                locale
            )),
            esc(&format!(
                "{}. {}",
                customs_zones.join(", "),
                pick(
                    (
                        "Les droits de douane, taxes d'importation et frais de dédouanement éventuels du pays de destination sont à la charge du destinataire et ne sont pas inclus dans nos prix.",
                        "Any customs duties, import taxes and clearance fees of the destination country are paid by the recipient and are not included in our prices.",
                        "الرسوم الجمركية وضرائب الاستيراد يتحملها المستلم وليست مشمولة في أسعارنا."
                    ),
                    locale
                )
            ))
        )
    };
    Ok((table, customs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_uses_known_tokens_only() {
        for (key, locales) in PAGES {
            for (locale, _, _) in locales {
                let t = template(key, locale).expect("template");
                let unknown: Vec<String> = tokens(t)
                    .into_iter()
                    .filter(|x| !KNOWN_TOKENS.contains(&x.as_str()))
                    .collect();
                assert!(unknown.is_empty(), "{key}.{locale}: {unknown:?}");
            }
        }
        // The form's own tokens too.
        for l in ["fr", "en", "ar"] {
            assert!(
                tokens(&withdrawal_form_html(l))
                    .iter()
                    .all(|t| KNOWN_TOKENS.contains(&t.as_str()))
            );
        }
    }

    #[test]
    fn tokens_render_and_unknown_ones_are_reported() {
        let values = HashMap::from([("seller.siren".to_string(), "732829320".to_string())]);
        let mut v = serde_json::json!({ "body": "<p>SIREN {{ seller.siren }} {{x.y}}</p>" });
        render_content(&mut v, &values);
        assert_eq!(v["body"], "<p>SIREN 732829320 {{x.y}}</p>");
        assert_eq!(unknown_tokens(&v), vec!["x.y".to_string()]);
    }

    #[test]
    fn the_cookie_table_lists_every_declared_entry() {
        let t = cookies_table("fr");
        for c in COOKIES {
            assert!(t.contains(c.name), "{}", c.name);
            assert!(COOKIE_CATEGORIES.contains(&c.category));
        }
    }

    /// The cookie policy must declare every cookie and storage key the site writes: scans the
    /// front-end sources (when present next to `back/`) and the API's cookie names.
    #[test]
    fn every_cookie_used_in_code_is_declared() {
        let front = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../front"));
        let mut keys = cookie_keys_in_front(front);
        if front.join("app").is_dir() {
            assert!(keys.len() >= 15, "the scan found too few keys: {keys:?}");
        }
        keys.extend(
            [
                "__Host-mz_at",
                "__Host-mz_rt",
                "__Host-mz_cart",
                "__Host-mz_shop_cart",
                "__Host-mouzoun_session",
            ]
            .map(String::from),
        );
        let undeclared: Vec<&String> = keys
            .iter()
            .filter(|k| cookie_declared(k).is_none())
            .collect();
        assert!(
            undeclared.is_empty(),
            "declare these in services/shop/legal_pages.rs COOKIES: {undeclared:?}"
        );
    }

    #[test]
    fn an_undeclared_cookie_fails_the_check() {
        assert!(cookie_declared("mz-tracking-id").is_none());
        assert!(cookie_declared("mz-purchase-MZ123").is_some());
    }

    /// `'mz-…'` string literals used as cookie or storage keys in the front-end (`.ts`/`.vue`),
    /// plus `mz-purchase-*`. Empty when the front-end is not checked out next to the API.
    pub fn cookie_keys_in_front(root: &std::path::Path) -> Vec<String> {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "ts" || x == "vue") {
                    let Ok(src) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    let storage = src.contains("useCookie")
                        || src.contains("localStorage")
                        || src.contains("sessionStorage")
                        || src.contains("useLocalStorage")
                        || src.contains("storageKey")
                        || src.contains("cookieKey");
                    if !storage {
                        continue;
                    }
                    let mut rest = src.as_str();
                    while let Some(i) = rest.find("'mz-") {
                        let after = &rest[i + 1..];
                        let end = after.find('\'').unwrap_or(after.len());
                        let key = &after[..end];
                        if key
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                        {
                            out.push(key.to_string());
                        }
                        rest = &after[end.min(after.len())..];
                    }
                    if src.contains("`mz-purchase-") {
                        out.push("mz-purchase-x".into());
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&root.join("app"), &mut out);
        let config = std::fs::read_to_string(root.join("nuxt.config.ts")).unwrap_or_default();
        if config.contains("'mz-locale'") {
            out.push("mz-locale".into());
        }
        out.sort();
        out.dedup();
        out
    }
}
