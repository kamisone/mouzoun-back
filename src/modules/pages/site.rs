//! Small public endpoints used by page sections: active banners (top bar), newsletter sign-up
//! and the contact form. Writes are rate limited and pass the bot check when it is configured.

use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::public::public_settings;
use crate::{
    db::types::DateRange,
    error::{AppError, AppResult},
    services::turnstile,
    state::AppState,
    web::{
        client::{ClientInfo, RequestLocale},
        limits::rate_limit,
    },
};

#[derive(Debug, Deserialize)]
pub struct BannerQuery {
    pub placement: Option<String>,
}

/// Active banners with their message in the request language. Messages may contain
/// `{opening_date}`: the value comes in `variables` (the front formats the date).
pub async fn banners(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Query(q): Query<BannerQuery>,
) -> AppResult<Json<Value>> {
    let now = state.clock.now();
    let rows = sqlx::query!(
        r#"SELECT b.id, b.placement, b.style, b.link_url, b.media_id,
                  COALESCE(NULLIF(t.message, ''), d.message) AS "message?", COALESCE(t.cta_label, d.cta_label) AS "cta_label?"
           FROM banners b
           LEFT JOIN banner_translations t ON t.banner_id = b.id AND t.locale = $1
           LEFT JOIN banner_translations d ON d.banner_id = b.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE b.is_active AND (b.active_period IS NULL OR b.active_period @> $2::timestamptz)
             AND ($3::text IS NULL OR b.placement = $3)
           ORDER BY b.sort_order, b.id"#,
        locale.as_str(),
        now,
        q.placement
    )
    .fetch_all(&state.db)
    .await?;
    let settings = public_settings(&state, &["site.opening_date", "site.status"]).await?;
    Ok(Json(json!({
        "banners": rows.into_iter().filter(|r| r.message.is_some()).map(|r| json!({
            "id": r.id, "placement": r.placement, "style": r.style, "link_url": r.link_url,
            "message": r.message, "cta_label": r.cta_label })).collect::<Vec<_>>(),
        "variables": { "opening_date": settings.get("site.opening_date"), "site_status": settings.get("site.status") },
    })))
}

fn valid_email(email: &str) -> bool {
    let email = email.trim();
    email.len() <= 254
        && email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        })
        && !email.chars().any(char::is_whitespace)
}

fn field_error(field: &str, code: &'static str, message: &str) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, code, message.to_string())
        .with_details(json!({ "field": field }))
}

#[derive(Debug, Deserialize)]
pub struct NewsletterBody {
    pub email: String,
    #[serde(default)]
    pub list: Option<String>,
    #[serde(default)]
    pub consent: bool,
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Newsletter sign-up (`pending` until confirmed). The answer is the same whether the address
/// was known or not.
pub async fn newsletter(
    State(state): State<AppState>,
    client: ClientInfo,
    RequestLocale(locale): RequestLocale,
    Json(body): Json<NewsletterBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    rate_limit(
        &state.cache,
        "newsletter",
        &client.key(),
        10,
        Duration::from_secs(3600),
    )
    .await?;
    turnstile::verify(
        &state.http,
        state.config.booking.turnstile_secret.as_deref(),
        body.captcha_token.as_deref(),
        client.ip,
    )
    .await?;
    if !body.consent {
        return Err(field_error(
            "consent",
            "consent_required",
            "consent is required",
        ));
    }
    if !valid_email(&body.email) {
        return Err(field_error(
            "email",
            "invalid_email",
            "invalid email address",
        ));
    }
    let list = body
        .list
        .as_deref()
        .filter(|l| {
            !l.is_empty()
                && l.len() <= 40
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .unwrap_or("news");
    sqlx::query!(
        "INSERT INTO newsletter_subscribers (id, email, locale, status, source) VALUES ($1, $2, $3, 'pending', $4)
         ON CONFLICT (email) DO UPDATE SET
             status = CASE WHEN newsletter_subscribers.status = 'unsubscribed' THEN 'pending' ELSE newsletter_subscribers.status END,
             unsubscribed_at = NULL, locale = EXCLUDED.locale",
        Uuid::now_v7(),
        body.email.trim(),
        locale.as_str(),
        format!("site:{list}")
    )
    .execute(&state.db)
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "pending" }))))
}

#[derive(Debug, Deserialize)]
pub struct ContactBody {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    pub message: String,
    #[serde(default)]
    pub arrival: Option<NaiveDate>,
    #[serde(default)]
    pub departure: Option<NaiveDate>,
    #[serde(default)]
    pub group_size: Option<i32>,
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Contact form → `inquiries` (handled in the back office).
pub async fn contact(
    State(state): State<AppState>,
    client: ClientInfo,
    RequestLocale(locale): RequestLocale,
    Json(body): Json<ContactBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    rate_limit(
        &state.cache,
        "contact",
        &client.key(),
        5,
        Duration::from_secs(3600),
    )
    .await?;
    turnstile::verify(
        &state.http,
        state.config.booking.turnstile_secret.as_deref(),
        body.captcha_token.as_deref(),
        client.ip,
    )
    .await?;
    let name = body.name.trim();
    let message = body.message.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(field_error("name", "invalid_name", "a name is required"));
    }
    if !valid_email(&body.email) {
        return Err(field_error(
            "email",
            "invalid_email",
            "invalid email address",
        ));
    }
    if message.is_empty() || message.chars().count() > 5000 {
        return Err(field_error(
            "message",
            "invalid_message",
            "a message is required (5000 characters at most)",
        ));
    }
    if body.group_size.is_some_and(|g| !(1..=500).contains(&g)) {
        return Err(field_error(
            "group_size",
            "invalid_group_size",
            "invalid group size",
        ));
    }
    let dates = match (body.arrival, body.departure) {
        (Some(a), Some(d)) if d > a => Some(DateRange::new(a, d)),
        (Some(_), Some(_)) => {
            return Err(field_error(
                "departure",
                "invalid_dates",
                "the departure must be after the arrival",
            ));
        }
        _ => None,
    };
    let ip = client.ip_network();
    sqlx::query!(
        "INSERT INTO inquiries (id, kind, name, email, phone, locale, subject, message, preferred_dates, group_size, ip_address)
         VALUES ($1, 'contact', $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        Uuid::now_v7(),
        name,
        body.email.trim(),
        body.phone.as_deref().map(str::trim).filter(|p| !p.is_empty()),
        locale.as_str(),
        body.subject.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        message,
        dates as Option<DateRange>,
        body.group_size,
        ip
    )
    .execute(&state.db)
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "received" }))))
}
