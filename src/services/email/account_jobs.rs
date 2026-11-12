//! Outbox events of guest accounts → emails (sign-in codes, welcome, security notices, booking
//! claim, data export, deletion). Variables are explicit (`Source::Data`): most of these emails
//! are about the account itself, some go to addresses without an account yet.

use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::{EmailJob, Source};
use crate::{db::operations::OutboxEvent, services::outbox::types, worker::HandlerError};

pub const EVENT_TYPES: &[&str] = &[
    types::AUTH_LOGIN_CODE,
    types::ACCOUNT_CREATED,
    types::AUTH_NEW_DEVICE,
    types::AUTH_PASSWORD_CHANGED,
    types::AUTH_EMAIL_CHANGE_REQUESTED,
    types::AUTH_EMAIL_CHANGED,
    types::AUTH_PASSKEY_ADDED,
    types::AUTH_IDENTITY_LINKED,
    types::BOOKING_CLAIM_REQUESTED,
    types::BOOKING_CLAIMED,
    types::ACCOUNT_EXPORT_READY,
    types::ACCOUNT_DELETION_SCHEDULED,
    types::ACCOUNT_DELETED,
];

fn retry(e: sqlx::Error) -> HandlerError {
    HandlerError::Retry(e.into())
}

fn secret<'a>(event: &'a OutboxEvent, field: &str) -> Result<&'a str, HandlerError> {
    event.payload["secret"][field].as_str().ok_or_else(|| {
        HandlerError::Permanent(anyhow::anyhow!(
            "{}: secret already scrubbed",
            event.event_type
        ))
    })
}

fn id(event: &OutboxEvent, field: &str) -> Result<Uuid, HandlerError> {
    event.payload[field]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            HandlerError::Permanent(anyhow::anyhow!("{}: missing {field}", event.event_type))
        })
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `/{locale}{path}`, without prefix for the default language (fr).
fn page(site: &str, locale: &str, path: &str) -> String {
    if locale == "fr" {
        format!("{site}{path}")
    } else {
        format!("{site}/{locale}{path}")
    }
}

struct Account {
    email: String,
    first_name: String,
    locale: Option<String>,
}

async fn account(pool: &PgPool, user_id: Uuid) -> Result<Option<Account>, HandlerError> {
    Ok(sqlx::query!(
        r#"SELECT u.email::text AS "email!", u.preferred_locale,
                  COALESCE(NULLIF(c.first_name, ''), split_part(u.full_name, ' ', 1)) AS "first_name!"
           FROM users u LEFT JOIN customers c ON c.user_id = u.id AND c.deleted_at IS NULL
           WHERE u.id = $1 AND u.deleted_at IS NULL"#,
        user_id
    )
    .fetch_optional(pool)
    .await
    .map_err(retry)?
    .map(|r| Account {
        email: r.email,
        first_name: r.first_name,
        locale: r.preferred_locale,
    }))
}

fn job(
    template: &str,
    to: &str,
    name: Option<String>,
    locale: Option<String>,
    booking_id: Option<Uuid>,
    vars: Value,
) -> EmailJob {
    EmailJob {
        template: template.into(),
        to_email: to.into(),
        to_name: name,
        locales: vec![locale],
        booking_id,
        source: Source::Data(vars),
    }
}

/// Emails of an account event (`None`: not an account event).
pub async fn jobs(
    pool: &PgPool,
    site: &str,
    event: &OutboxEvent,
) -> Result<Option<Vec<EmailJob>>, HandlerError> {
    let t = event.event_type.as_str();
    if !EVENT_TYPES.contains(&t) {
        return Ok(None);
    }
    let locale_of = |a: &Option<Account>| {
        a.as_ref()
            .and_then(|a| a.locale.clone())
            .unwrap_or_else(|| "fr".into())
    };
    let jobs = match t {
        types::AUTH_LOGIN_CODE => {
            let email = secret(event, "email")?;
            let locale = event.payload["locale"].as_str().unwrap_or("fr").to_string();
            let mut link = format!(
                "{}?e={}&t={}",
                page(site, &locale, "/account/signin/link"),
                encode(email),
                encode(secret(event, "token")?)
            );
            if let Some(r) = event.payload["secret"]["return_to"].as_str() {
                link.push_str(&format!("&r={}", encode(r)));
            }
            vec![job(
                "sign_in_code",
                email,
                None,
                Some(locale),
                None,
                json!({
                    "code": secret(event, "code")?, "link": link, "minutes": 10,
                }),
            )]
        }
        types::ACCOUNT_CREATED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            a.map(|a| {
                vec![job(
                    "account_welcome",
                    &a.email,
                    None,
                    a.locale.clone(),
                    None,
                    json!({
                        "first_name": a.first_name, "account_url": page(site, &loc, "/account"),
                    }),
                )]
            })
            .unwrap_or_default()
        }
        types::AUTH_NEW_DEVICE => {
            let a = account(pool, id(event, "user_id")?).await?;
            let s = sqlx::query!(
                r#"SELECT user_agent, created_at FROM user_sessions WHERE id = $1"#,
                id(event, "session_id")?
            )
            .fetch_optional(pool)
            .await
            .map_err(retry)?;
            let loc = locale_of(&a);
            match (a, s) {
                (Some(a), Some(s)) => vec![job(
                    "security_new_device",
                    &a.email,
                    None,
                    a.locale.clone(),
                    None,
                    json!({
                        "first_name": a.first_name,
                        "device": describe_device(s.user_agent.as_deref()),
                        "when": s.created_at.to_rfc3339(),
                        "security_url": page(site, &loc, "/account/security"),
                    }),
                )],
                _ => vec![],
            }
        }
        types::AUTH_PASSWORD_CHANGED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            a.map(|a| {
                vec![job(
                    "security_password_changed",
                    &a.email,
                    None,
                    a.locale.clone(),
                    None,
                    json!({
                        "first_name": a.first_name, "change": event.payload["change"],
                        "security_url": page(site, &loc, "/account/security"),
                    }),
                )]
            })
            .unwrap_or_default()
        }
        types::AUTH_EMAIL_CHANGE_REQUESTED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            let new_email = secret(event, "new_email")?;
            let mut out = vec![job(
                "email_change_code",
                new_email,
                None,
                Some(loc.clone()),
                None,
                json!({ "code": secret(event, "code")? }),
            )];
            if let Some(a) = a {
                out.push(job(
                    "email_change_notice",
                    &a.email,
                    None,
                    a.locale.clone(),
                    None,
                    json!({
                        "first_name": a.first_name, "new_email": new_email,
                        "security_url": page(site, &loc, "/account/security"),
                    }),
                ));
            }
            out
        }
        types::AUTH_EMAIL_CHANGED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            let old = secret(event, "old_email")?;
            match a {
                Some(a) => {
                    let vars = json!({
                        "first_name": a.first_name, "old_email": old, "new_email": a.email,
                        "security_url": page(site, &loc, "/account/security"),
                    });
                    vec![
                        job(
                            "security_email_changed",
                            old,
                            None,
                            a.locale.clone(),
                            None,
                            vars.clone(),
                        ),
                        job(
                            "security_email_changed",
                            &a.email,
                            None,
                            a.locale.clone(),
                            None,
                            vars,
                        ),
                    ]
                }
                None => vec![],
            }
        }
        types::AUTH_PASSKEY_ADDED | types::AUTH_IDENTITY_LINKED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            let provider = match event.payload["provider"].as_str() {
                Some("google") => "Google",
                Some("apple") => "Apple",
                Some("facebook") => "Facebook",
                _ => "",
            };
            let template = if t == types::AUTH_PASSKEY_ADDED {
                "security_passkey_added"
            } else {
                "security_identity_linked"
            };
            a.map(|a| {
                vec![job(
                    template,
                    &a.email,
                    None,
                    a.locale.clone(),
                    None,
                    json!({
                        "first_name": a.first_name, "provider": provider,
                        "security_url": page(site, &loc, "/account/security"),
                    }),
                )]
            })
            .unwrap_or_default()
        }
        types::BOOKING_CLAIM_REQUESTED => {
            let booking_id = id(event, "booking_id")?;
            let b = sqlx::query!(
                r#"SELECT b.reference, b.locale, c.email::text AS "email?" FROM bookings b
                   JOIN customers c ON c.id = b.customer_id WHERE b.id = $1"#,
                booking_id
            )
            .fetch_optional(pool)
            .await
            .map_err(retry)?;
            match b.and_then(|b| b.email.clone().map(|e| (b, e))) {
                Some((b, email)) => vec![job(
                    "booking_claim",
                    &email,
                    None,
                    Some(b.locale.clone()),
                    Some(booking_id),
                    json!({
                        "reference": b.reference,
                        "link": format!("{}?token={}", page(site, &b.locale, "/account/trips/claim"), encode(secret(event, "token")?)),
                    }),
                )],
                None => vec![],
            }
        }
        types::BOOKING_CLAIMED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            let booking_id = id(event, "booking_id")?;
            let reference =
                sqlx::query_scalar!("SELECT reference FROM bookings WHERE id = $1", booking_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(retry)?;
            match (a, reference) {
                (Some(a), Some(r)) => vec![job(
                    "booking_claimed",
                    &a.email,
                    None,
                    a.locale.clone(),
                    Some(booking_id),
                    json!({
                        "first_name": a.first_name, "reference": r, "trip_url": page(site, &loc, &format!("/booking/{r}")),
                    }),
                )],
                _ => vec![],
            }
        }
        types::ACCOUNT_EXPORT_READY => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            a.map(|a| vec![job("account_export_ready", &a.email, None, a.locale.clone(), None, json!({
                "first_name": a.first_name, "days": 7,
                "link": format!("{}?token={}", page(site, &loc, "/account/privacy/export"), encode(secret(event, "token").unwrap_or_default())),
            }))])
            .unwrap_or_default()
        }
        types::ACCOUNT_DELETION_SCHEDULED => {
            let a = account(pool, id(event, "user_id")?).await?;
            let loc = locale_of(&a);
            let when = sqlx::query_scalar!(
                "SELECT scheduled_for FROM account_deletion_requests WHERE id = $1",
                id(event, "request_id")?
            )
            .fetch_optional(pool)
            .await
            .map_err(retry)?;
            match (a, when) {
                (Some(a), Some(when)) => vec![job(
                    "account_deletion_scheduled",
                    &a.email,
                    None,
                    a.locale.clone(),
                    None,
                    json!({
                        "first_name": a.first_name, "scheduled_for": when.to_rfc3339(),
                        "cancel_url": format!("{}?token={}", page(site, &loc, "/account/privacy/cancel-deletion"), encode(secret(event, "token")?)),
                    }),
                )],
                _ => vec![],
            }
        }
        types::ACCOUNT_DELETED => {
            let locale = event.payload["locale"].as_str().unwrap_or("fr").to_string();
            vec![job(
                "account_deleted",
                secret(event, "email")?,
                None,
                Some(locale),
                None,
                json!({}),
            )]
        }
        _ => vec![],
    };
    Ok(Some(jobs))
}

/// "Chrome on macOS" from a user agent (rough, enough for a security notice).
pub fn describe_device(ua: Option<&str>) -> String {
    let ua = ua.unwrap_or_default();
    let browser = ["Edg", "Firefox", "Chrome", "Safari"]
        .iter()
        .find(|b| ua.contains(*b))
        .map(|b| if *b == "Edg" { "Edge" } else { b })
        .unwrap_or("Browser");
    let os = [
        ("iPhone", "iPhone"),
        ("iPad", "iPad"),
        ("Android", "Android"),
        ("Mac OS", "macOS"),
        ("Windows", "Windows"),
        ("Linux", "Linux"),
    ]
    .iter()
    .find(|(k, _)| ua.contains(k))
    .map_or("unknown system", |(_, v)| v);
    format!("{browser} · {os}")
}
