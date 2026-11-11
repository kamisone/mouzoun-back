//! Loads the facts an email talks about (always the current state: events carry ids only) and
//! turns them into template variables.

use std::collections::HashMap;

use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    EmailError, EmailSettings,
    delivery::Source,
    links,
    views::{
        self, BookingFacts, GuestFacts, InquiryFacts, LineFacts, PaymentFacts, SiteInfo, ViewEnv,
    },
};

pub const STAFF_EMAIL_KEY: &str = "notifications.staff_email";
pub const PAYMENT_INSTRUCTIONS_KEY: &str = "booking.payment_instructions";
pub const LOGO_KEY: &str = "email.logo_media_id";

const SITE_KEYS: [&str; 9] = [
    "site.name",
    "site.tagline",
    "contact.email",
    "contact.phone",
    "contact.whatsapp",
    "contact.address",
    "location.latitude",
    "location.longitude",
    LOGO_KEY,
];

/// Settings values by key.
async fn settings(pool: &PgPool, keys: &[&str]) -> sqlx::Result<HashMap<String, Value>> {
    let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    Ok(
        sqlx::query!("SELECT key, value FROM settings WHERE key = ANY($1)", &keys)
            .fetch_all(pool)
            .await?
            .into_iter()
            .map(|r| (r.key, r.value))
            .collect(),
    )
}

/// A `string` setting, or a `localized_text` one in `locale` (then the default locale, then
/// any). Empty strings count as unset.
pub(crate) fn text(value: Option<&Value>, locale: &str, default_locale: &str) -> Option<String> {
    let s = match value? {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => [locale, default_locale]
            .iter()
            .find_map(|l| {
                map.get(*l)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| map.values().find_map(Value::as_str))
            .map(str::to_string),
        _ => None,
    }?;
    (!s.trim().is_empty()).then_some(s)
}

pub async fn site(
    pool: &PgPool,
    cfg: &EmailSettings,
    locale: &str,
    default_locale: &str,
) -> sqlx::Result<SiteInfo> {
    let s = settings(pool, &SITE_KEYS).await?;
    let get = |key: &str| text(s.get(key), locale, default_locale);
    let logo_url = match s
        .get(LOGO_KEY)
        .and_then(Value::as_str)
        .and_then(|v| v.parse::<Uuid>().ok())
    {
        Some(id) => sqlx::query_scalar!(
            "SELECT object_key FROM media_assets WHERE id = $1 AND deleted_at IS NULL",
            id
        )
        .fetch_optional(pool)
        .await?
        .map(|key| format!("{}/{key}", cfg.media_base_url)),
        None => None,
    };
    let coordinate = |key: &str| s.get(key).and_then(Value::as_f64);
    let whatsapp = get("contact.whatsapp");
    Ok(SiteInfo {
        name: get("site.name").unwrap_or_else(|| "Mouzoun".into()),
        url: cfg.public_site_url.clone(),
        logo_url,
        tagline: get("site.tagline"),
        address: get("contact.address"),
        phone: get("contact.phone"),
        whatsapp_url: whatsapp.as_ref().map(|w| {
            format!(
                "https://wa.me/{}",
                w.chars().filter(char::is_ascii_digit).collect::<String>()
            )
        }),
        whatsapp,
        email: get("contact.email"),
        map_url: match (
            coordinate("location.latitude"),
            coordinate("location.longitude"),
        ) {
            (Some(lat), Some(lng)) => Some(format!("https://www.google.com/maps?q={lat},{lng}")),
            _ => None,
        },
    })
}

/// Where staff notifications go (`notifications.staff_email`), if set.
pub async fn staff_email(pool: &PgPool) -> sqlx::Result<Option<String>> {
    let s = settings(pool, &[STAFF_EMAIL_KEY]).await?;
    Ok(text(s.get(STAFF_EMAIL_KEY), "", ""))
}

/// Who a booking email goes to, with the locale preferences in priority order.
pub struct BookingRecipient {
    pub status: String,
    pub arrival: Option<chrono::NaiveDate>,
    pub email: Option<String>,
    pub name: String,
    /// booking.locale > customer.preferred_locale > user.preferred_locale.
    pub locales: Vec<Option<String>>,
}

pub async fn booking_recipient(
    pool: &PgPool,
    booking_id: Uuid,
) -> sqlx::Result<Option<BookingRecipient>> {
    Ok(sqlx::query!(
        r#"SELECT b.status, b.arrival_date, b.locale, c.email::text AS email, c.first_name, c.last_name,
                  c.preferred_locale AS customer_locale, u.preferred_locale AS "user_locale?"
           FROM bookings b
           JOIN customers c ON c.id = b.customer_id
           LEFT JOIN users u ON u.id = c.user_id
           WHERE b.id = $1"#,
        booking_id
    )
    .fetch_optional(pool)
    .await?
    .map(|r| BookingRecipient {
        status: r.status,
        arrival: r.arrival_date,
        email: r.email.filter(|e| !e.is_empty()),
        name: match r.last_name {
            Some(last) if !last.is_empty() => format!("{} {last}", r.first_name),
            _ => r.first_name,
        },
        locales: vec![Some(r.locale), r.customer_locale, r.user_locale],
    }))
}

async fn booking_facts(
    pool: &PgPool,
    booking_id: Uuid,
    locale: &str,
    default_locale: &str,
) -> Result<(BookingFacts, GuestFacts, Vec<LineFacts>), EmailError> {
    let b = sqlx::query!(
        r#"SELECT b.id, b.reference, b.status, b.currency::text AS "currency!", b.arrival_date, b.departure_date,
                  b.adults, b.children, b.infants, b.subtotal_minor, b.discount_minor, b.tax_minor, b.total_minor,
                  b.amount_paid_minor, b.deposit_due_minor, b.balance_due_on, b.hold_expires_at,
                  b.special_requests, b.estimated_arrival_time, b.cancelled_at, b.cancellation_reason,
                  b.created_at, b.cancellation_policy_snapshot,
                  c.first_name, c.last_name, c.email::text AS email, c.phone,
                  (SELECT min(a.check_in_from) FROM booking_items i
                   JOIN accommodation_types a ON a.product_id = i.product_id
                   WHERE i.booking_id = b.id) AS check_in_from
           FROM bookings b
           JOIN customers c ON c.id = b.customer_id
           WHERE b.id = $1"#,
        booking_id
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| EmailError::Data(format!("booking {booking_id} not found")))?;

    let closed = matches!(b.status.as_str(), "cancelled" | "expired");
    let lines = sqlx::query!(
        r#"SELECT i.product_kind AS kind, i.quantity, lower(i.service_period) AS start_date,
                  upper(i.service_period) AS end_date, i.starts_at, i.total_minor,
                  COALESCE(tr.name, dtr.name, i.product_snapshot->>'name', '') AS "name!",
                  COALESCE((SELECT sum(g.guest_count) FROM booking_item_guests g
                            WHERE g.booking_item_id = i.id), 0)::bigint AS "participants!"
           FROM booking_items i
           LEFT JOIN product_translations tr ON tr.product_id = i.product_id AND tr.locale = $2
           LEFT JOIN product_translations dtr ON dtr.product_id = i.product_id AND dtr.locale = $3
           WHERE i.booking_id = $1 AND (i.status = 'active' OR $4)
           ORDER BY COALESCE(i.starts_at, lower(i.service_period)::timestamptz), i.created_at"#,
        booking_id,
        locale,
        default_locale,
        closed
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|l| LineFacts {
        name: l.name,
        kind: l.kind,
        quantity: i64::from(l.quantity),
        start_date: l.start_date,
        end_date: l.end_date,
        starts_at: l.starts_at,
        participants: l.participants,
        total_minor: l.total_minor,
    })
    .collect();

    let facts = BookingFacts {
        id: b.id,
        reference: b.reference,
        status: b.status,
        currency: b.currency,
        arrival: b.arrival_date,
        departure: b.departure_date,
        adults: i64::from(b.adults),
        children: i64::from(b.children),
        infants: i64::from(b.infants),
        subtotal_minor: b.subtotal_minor,
        discount_minor: b.discount_minor,
        tax_minor: b.tax_minor,
        total_minor: b.total_minor,
        amount_paid_minor: b.amount_paid_minor,
        deposit_due_minor: b.deposit_due_minor,
        balance_due_on: b.balance_due_on,
        hold_expires_at: b.hold_expires_at,
        special_requests: b.special_requests,
        estimated_arrival_time: b.estimated_arrival_time,
        check_in_from: b.check_in_from,
        cancelled_at: b.cancelled_at,
        cancellation_reason: b.cancellation_reason,
        created_at: b.created_at,
        policy_rules: b.cancellation_policy_snapshot,
    };
    let guest = GuestFacts {
        first_name: b.first_name,
        last_name: b.last_name,
        email: b.email,
        phone: b.phone,
    };
    Ok((facts, guest, lines))
}

async fn payment_facts(pool: &PgPool, payment_id: Uuid) -> Result<PaymentFacts, EmailError> {
    sqlx::query!(
        r#"SELECT amount_minor, currency::text AS "currency!", provider, purpose, paid_at, provider_payment_id
           FROM payments WHERE id = $1"#,
        payment_id
    )
    .fetch_optional(pool)
    .await?
    .map(|p| PaymentFacts {
        amount_minor: p.amount_minor,
        currency: p.currency,
        provider: p.provider,
        purpose: p.purpose,
        paid_at: p.paid_at,
        provider_reference: p.provider_payment_id,
    })
    .ok_or_else(|| EmailError::Data(format!("payment {payment_id} not found")))
}

pub async fn inquiry_facts(
    pool: &PgPool,
    inquiry_id: Uuid,
) -> sqlx::Result<Option<(InquiryFacts, Option<String>)>> {
    Ok(sqlx::query!(
        r#"SELECT id, kind, name, email::text AS "email!", phone, subject, message, locale,
                  lower(preferred_dates) AS preferred_from, upper(preferred_dates) AS preferred_to,
                  group_size, created_at
           FROM inquiries WHERE id = $1"#,
        inquiry_id
    )
    .fetch_optional(pool)
    .await?
    .map(|i| {
        (
            InquiryFacts {
                id: i.id,
                kind: i.kind,
                name: i.name,
                email: i.email,
                phone: i.phone,
                subject: i.subject,
                message: i.message,
                preferred_from: i.preferred_from,
                preferred_to: i.preferred_to,
                group_size: i.group_size,
                created_at: i.created_at,
            },
            i.locale,
        )
    }))
}

/// Every variable of the email, in `locale`.
pub async fn build(
    pool: &PgPool,
    cfg: &EmailSettings,
    source: &Source,
    locale: &str,
    default_locale: &str,
) -> Result<Value, EmailError> {
    let site = site(pool, cfg, locale, default_locale).await?;
    let env = ViewEnv {
        locale,
        site_url: &cfg.public_site_url,
        link_secret: &cfg.link_secret,
    };
    let mut vars = Map::new();
    match source {
        Source::Booking {
            booking_id,
            payment_id,
        } => {
            let (facts, guest, lines) =
                booking_facts(pool, *booking_id, locale, default_locale).await?;
            let (booking, lines, policy) = views::booking(&facts, &lines, &env);
            let instructions = settings(pool, &[PAYMENT_INSTRUCTIONS_KEY]).await?;
            vars.insert("guest".into(), views::guest(&guest));
            vars.insert("booking".into(), booking);
            vars.insert("lines".into(), lines);
            vars.insert("policy".into(), policy);
            vars.insert(
                "payment_instructions".into(),
                json!(text(
                    instructions.get(PAYMENT_INSTRUCTIONS_KEY),
                    locale,
                    default_locale
                )),
            );
            let payment = match payment_id {
                Some(id) => views::payment(&payment_facts(pool, *id).await?),
                None => Value::Null,
            };
            vars.insert("payment".into(), payment);
            vars.insert(
                "admin_url".into(),
                json!(links::admin_url(
                    &cfg.public_site_url,
                    &format!("bookings/{booking_id}")
                )),
            );
        }
        Source::Inquiry { inquiry_id } => {
            let (inquiry, _) = inquiry_facts(pool, *inquiry_id)
                .await?
                .ok_or_else(|| EmailError::Data(format!("inquiry {inquiry_id} not found")))?;
            vars.insert("inquiry".into(), views::inquiry(&inquiry));
            vars.insert(
                "admin_url".into(),
                json!(links::admin_url(
                    &cfg.public_site_url,
                    &format!("inquiries/{inquiry_id}")
                )),
            );
        }
        Source::Newsletter { token } => {
            let path = format!("{locale}/newsletter/confirm");
            vars.insert(
                "confirm_url".into(),
                json!(links::token_url(&cfg.public_site_url, &path, token)),
            );
        }
        Source::Account {
            user_id,
            purpose,
            token,
            expires_at,
            invited_by,
            staff,
        } => {
            let user = sqlx::query!(
                "SELECT full_name, email::text AS \"email!\" FROM users WHERE id = $1",
                user_id
            )
            .fetch_optional(pool)
            .await?
            .ok_or_else(|| EmailError::Data(format!("user {user_id} not found")))?;
            let path = match (purpose, staff) {
                (crate::db::identity::TokenPurpose::PasswordReset, true) => {
                    "admin/reset-password".to_string()
                }
                (crate::db::identity::TokenPurpose::PasswordReset, false) => {
                    format!("{locale}/account/reset-password")
                }
                (crate::db::identity::TokenPurpose::EmailVerification, _) => {
                    format!("{locale}/account/verify-email")
                }
                (crate::db::identity::TokenPurpose::Invitation, _) => {
                    "admin/accept-invitation".to_string()
                }
            };
            let inviter = match invited_by {
                Some(id) => {
                    sqlx::query_scalar!("SELECT full_name FROM users WHERE id = $1", id)
                        .fetch_optional(pool)
                        .await?
                }
                None => None,
            };
            vars.insert(
                "user".into(),
                json!({ "full_name": user.full_name, "email": user.email }),
            );
            vars.insert(
                "action_url".into(),
                json!(links::token_url(&cfg.public_site_url, &path, token)),
            );
            vars.insert("expires_at".into(), json!(expires_at.to_rfc3339()));
            vars.insert("invited_by".into(), json!(inviter));
        }
        Source::Data(data) => {
            if let Some(map) = data.as_object() {
                vars.extend(map.clone());
            }
        }
    }
    vars.insert(
        "site".into(),
        serde_json::to_value(&site).expect("serializable"),
    );
    Ok(Value::Object(vars))
}
