//! The account: personal info (kept in sync with the customer record bookings use), trips,
//! notification preferences, sessions, password, email change.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::{GuestUser, password},
    camp_time,
    db::identity::{NotificationCategory, NotificationChannel},
    error::{AppError, AppResult},
    services::{
        accounts::{
            passwords, sessions, signin,
            tokens::{normalize_email, sha256, six_digit_code},
        },
        audit::{self, Actor},
        outbox::{self, Event},
    },
    state::AppState,
    web::{client::ClientInfo, limits::rate_limit},
};

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub id: Uuid,
    pub email: String,
    pub email_verified: bool,
    pub first_name: String,
    pub last_name: Option<String>,
    pub phone: Option<String>,
    pub country_code: Option<String>,
    pub preferred_locale: Option<String>,
    pub display_currency: Option<String>,
    pub date_of_birth: Option<NaiveDate>,
    pub marketing_consent: bool,
    pub has_password: bool,
    pub identities: Vec<Value>,
    pub created_at: chrono::DateTime<Utc>,
    pub next_trip: Option<Value>,
}

pub(crate) async fn account_view(state: &AppState, user_id: Uuid) -> AppResult<AccountView> {
    let u = sqlx::query!(
        r#"SELECT u.id, u.email::text AS "email!", u.email_verified_at, u.phone AS user_phone, u.preferred_locale,
                  u.display_currency::text AS "display_currency?", u.date_of_birth, u.password_hash IS NOT NULL AS "has_password!",
                  u.created_at, c.first_name AS "first_name?", c.last_name, c.phone AS "phone?", c.country_code::text AS "country_code?",
                  COALESCE(c.marketing_consent, false) AS "marketing_consent!", c.id AS "customer_id?"
           FROM users u LEFT JOIN customers c ON c.user_id = u.id AND c.deleted_at IS NULL
           WHERE u.id = $1 AND u.deleted_at IS NULL"#,
        user_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Unauthorized)?;
    let identities = sqlx::query!(
        r#"SELECT id, provider, email::text AS "email?", created_at, last_used_at FROM user_identities WHERE user_id = $1 ORDER BY created_at"#,
        user_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|i| json!({ "id": i.id, "provider": i.provider, "email": i.email, "created_at": i.created_at, "last_used_at": i.last_used_at }))
    .collect();
    let today = camp_time::today(Utc::now());
    let next_trip = match u.customer_id {
        Some(cid) => sqlx::query!(
            r#"SELECT reference, arrival_date AS "arrival!", departure_date, status
               FROM bookings WHERE customer_id = $1 AND status IN ('pending', 'awaiting_payment', 'confirmed', 'checked_in')
                 AND arrival_date >= $2
               ORDER BY arrival_date LIMIT 1"#,
            cid,
            today
        )
        .fetch_optional(&state.db)
        .await?
        .map(|t| json!({ "reference": t.reference, "arrival": t.arrival, "departure": t.departure_date, "status": t.status,
                          "days": (t.arrival - today).num_days() })),
        None => None,
    };
    Ok(AccountView {
        id: u.id,
        email: u.email,
        email_verified: u.email_verified_at.is_some(),
        first_name: u.first_name.unwrap_or_default(),
        last_name: u.last_name,
        phone: u.phone.or(u.user_phone),
        country_code: u.country_code,
        preferred_locale: u.preferred_locale,
        display_currency: u.display_currency,
        date_of_birth: u.date_of_birth,
        marketing_consent: u.marketing_consent,
        has_password: u.has_password,
        identities,
        created_at: u.created_at,
        next_trip,
    })
}

pub async fn me(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<AccountView>> {
    Ok(Json(account_view(&state, guest.user_id).await?))
}

#[derive(Debug, Deserialize)]
pub struct ProfileBody {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub phone: Option<String>,
    pub country_code: Option<String>,
    pub preferred_locale: Option<String>,
    pub display_currency: Option<String>,
    pub date_of_birth: Option<NaiveDate>,
    pub marketing_consent: Option<bool>,
}

fn field(name: &str, reason: &str) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_field",
        format!("{name}: {reason}"),
    )
    .with_details(json!({ "field": name, "reason": reason }))
}

pub async fn update_me(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<ProfileBody>,
) -> AppResult<Json<AccountView>> {
    for (name, value) in [
        ("first_name", &body.first_name),
        ("last_name", &body.last_name),
    ] {
        if let Some(v) = value
            && (v.trim().is_empty() || v.chars().count() > 80)
        {
            return Err(field(name, "required"));
        }
    }
    if let Some(cc) = &body.country_code
        && (cc.len() != 2 || !cc.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return Err(field("country_code", "invalid"));
    }
    if let Some(cur) = &body.display_currency
        && !["EUR", "MAD", "USD"].contains(&cur.as_str())
    {
        return Err(field("display_currency", "invalid"));
    }
    if let Some(dob) = body.date_of_birth
        && (dob > camp_time::today(Utc::now())
            || dob < NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default())
    {
        return Err(field("date_of_birth", "invalid"));
    }
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    let email = sqlx::query_scalar!(
        r#"SELECT email::text AS "email!" FROM users WHERE id = $1"#,
        guest.user_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let customer = signin::link_customer(
        &mut tx,
        guest.user_id,
        &email,
        &crate::db::types::Locale::new("fr"),
    )
    .await?;
    sqlx::query!(
        "UPDATE customers SET first_name = COALESCE($2, first_name), last_name = COALESCE($3, last_name),
                phone = COALESCE($4, phone), country_code = COALESCE($5, country_code),
                preferred_locale = COALESCE($6, preferred_locale),
                marketing_consent = COALESCE($7, marketing_consent),
                marketing_consent_at = CASE WHEN $7 AND NOT marketing_consent THEN $8 ELSE marketing_consent_at END
         WHERE id = $1",
        customer,
        body.first_name.as_deref().map(str::trim),
        body.last_name.as_deref().map(str::trim),
        body.phone.as_deref().map(str::trim),
        body.country_code.as_deref().map(str::to_ascii_uppercase),
        body.preferred_locale.as_deref(),
        body.marketing_consent,
        now
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE users u SET full_name = btrim(c.first_name || ' ' || COALESCE(c.last_name, '')),
                phone = c.phone, preferred_locale = COALESCE($2, u.preferred_locale),
                display_currency = COALESCE($3, u.display_currency), date_of_birth = COALESCE($4, u.date_of_birth)
         FROM customers c WHERE u.id = $1 AND c.id = $5",
        guest.user_id,
        body.preferred_locale.as_deref(),
        body.display_currency.as_deref(),
        body.date_of_birth,
        customer
    )
    .execute(&mut *tx)
    .await?;
    if let Some(consent) = body.marketing_consent {
        sqlx::query!(
            "INSERT INTO notification_preferences (user_id, category, channel, enabled) VALUES ($1, 'offers', 'email', $2)
             ON CONFLICT (user_id, category, channel) DO UPDATE SET enabled = EXCLUDED.enabled",
            guest.user_id,
            consent
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(&mut tx, &client.actor(Actor::customer()), "account.updated", "user", &guest.user_id.to_string(),
        json!({ "fields": [body.first_name.is_some().then_some("first_name"), body.last_name.is_some().then_some("last_name"),
                            body.phone.is_some().then_some("phone"), body.country_code.is_some().then_some("country_code"),
                            body.marketing_consent.is_some().then_some("marketing_consent")] })).await?;
    tx.commit().await?;
    Ok(Json(account_view(&state, guest.user_id).await?))
}

#[derive(Debug, Deserialize)]
pub struct TripsQuery {
    /// `upcoming` (default), `past`, `cancelled`.
    pub tab: Option<String>,
    pub page: Option<i64>,
}

/// The guest's bookings. Each opens the booking page (same screens and endpoints as the email
/// link, authorised by ownership).
pub async fn trips(
    State(state): State<AppState>,
    guest: GuestUser,
    Query(q): Query<TripsQuery>,
) -> AppResult<Json<Value>> {
    let today = camp_time::today(Utc::now());
    let tab = q.tab.as_deref().unwrap_or("upcoming");
    let page = q.page.unwrap_or(1).max(1);
    let rows = sqlx::query!(
        r#"SELECT b.reference, b.status, b.arrival_date, b.departure_date, b.total_minor, b.amount_paid_minor,
                  b.balance_due_on, b.precheckin_completed_at IS NOT NULL AS "precheckin!", b.currency::text AS "currency!",
                  b.adults, b.children, b.infants,
                  (SELECT string_agg(COALESCE(i.product_snapshot->>'localized_name', i.product_snapshot->>'name'), ' · ' ORDER BY i.created_at)
                   FROM booking_items i WHERE i.booking_id = b.id AND i.parent_item_id IS NULL AND i.status = 'active') AS "products?",
                  (SELECT p.id FROM booking_items i JOIN products p ON p.id = i.product_id
                   WHERE i.booking_id = b.id AND i.parent_item_id IS NULL ORDER BY i.created_at LIMIT 1) AS "product_id?",
                  (SELECT m.object_key FROM booking_items i JOIN products p ON p.id = i.product_id
                   JOIN media_assets m ON m.id = p.cover_media_id
                   WHERE i.booking_id = b.id AND i.parent_item_id IS NULL ORDER BY i.created_at LIMIT 1) AS "cover_key?",
                  count(*) OVER () AS "total!"
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE c.user_id = $1
             AND CASE $2
                   WHEN 'cancelled' THEN b.status IN ('cancelled', 'expired', 'no_show')
                   WHEN 'past' THEN b.status IN ('completed') OR (b.status IN ('confirmed', 'checked_in')
                                    AND COALESCE(b.departure_date, b.arrival_date) < $3)
                   ELSE b.status IN ('pending', 'awaiting_payment', 'confirmed', 'checked_in')
                        AND COALESCE(b.departure_date, b.arrival_date + 1) >= $3
                 END
           ORDER BY CASE WHEN $2 = 'upcoming' THEN b.arrival_date END ASC,
                    CASE WHEN $2 <> 'upcoming' THEN b.arrival_date END DESC,
                    b.created_at DESC
           LIMIT 20 OFFSET $4"#,
        guest.user_id,
        tab,
        today,
        (page - 1) * 20
    )
    .fetch_all(&state.db)
    .await?;
    let total = rows.first().map_or(0, |r| r.total);
    let trips: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "reference": r.reference, "status": r.status, "arrival": r.arrival_date, "departure": r.departure_date,
                "days_until": r.arrival_date.map(|a| (a - today).num_days()),
                "total_minor": r.total_minor, "paid_minor": r.amount_paid_minor,
                "balance_minor": (r.total_minor - r.amount_paid_minor).max(0), "balance_due_on": r.balance_due_on,
                "precheckin_completed": r.precheckin, "currency": r.currency, "products": r.products,
                "party": { "adults": r.adults, "children": r.children, "infants": r.infants },
                "product_id": r.product_id,
                "cover_url": r.cover_key.map(|k| state.media.public_url(&k)),
            })
        })
        .collect();
    Ok(Json(
        json!({ "tab": tab, "page": page, "total": total, "trips": trips }),
    ))
}

// ---- Notifications --------------------------------------------------------------------------

pub async fn notifications(
    State(state): State<AppState>,
    guest: GuestUser,
) -> AppResult<Json<Value>> {
    let stored = sqlx::query!(
        r#"SELECT category, channel, enabled FROM notification_preferences WHERE user_id = $1"#,
        guest.user_id
    )
    .fetch_all(&state.db)
    .await?;
    let consent = sqlx::query_scalar!(
        "SELECT marketing_consent FROM customers WHERE user_id = $1 AND deleted_at IS NULL",
        guest.user_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    let mut out = serde_json::Map::new();
    for category in NotificationCategory::ALL {
        let mut channels = serde_json::Map::new();
        for channel in NotificationChannel::ALL {
            // Defaults: reminders and review requests by email on; offers only with consent.
            let default = match (category, channel) {
                (NotificationCategory::Offers, NotificationChannel::Email) => consent,
                (NotificationCategory::Offers, _) => false,
                (_, NotificationChannel::Email) => true,
                _ => false,
            };
            let enabled = stored
                .iter()
                .find(|s| s.category == category.as_str() && s.channel == channel.as_str())
                .map_or(default, |s| s.enabled);
            channels.insert(channel.as_str().into(), json!(enabled));
        }
        out.insert(category.as_str().into(), Value::Object(channels));
    }
    Ok(Json(
        json!({ "preferences": out, "always_on": ["booking_updates", "security"] }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PreferenceBody {
    pub category: NotificationCategory,
    pub channel: NotificationChannel,
    pub enabled: bool,
}

pub async fn set_notification(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<PreferenceBody>,
) -> AppResult<Json<Value>> {
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO notification_preferences (user_id, category, channel, enabled) VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, category, channel) DO UPDATE SET enabled = EXCLUDED.enabled",
        guest.user_id,
        body.category.as_str(),
        body.channel.as_str(),
        body.enabled
    )
    .execute(&mut *tx)
    .await?;
    // Offers by email = the marketing consent of the customer record and the newsletter.
    if body.category == NotificationCategory::Offers && body.channel == NotificationChannel::Email {
        sqlx::query!(
            "UPDATE customers SET marketing_consent = $2,
                    marketing_consent_at = CASE WHEN $2 THEN $3 ELSE marketing_consent_at END
             WHERE user_id = $1",
            guest.user_id,
            body.enabled,
            now
        )
        .execute(&mut *tx)
        .await?;
        if !body.enabled {
            sqlx::query!(
                "UPDATE newsletter_subscribers SET status = 'unsubscribed', unsubscribed_at = $2
                 WHERE email = (SELECT email FROM users WHERE id = $1) AND status <> 'unsubscribed'",
                guest.user_id,
                now
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    audit::record(
        &mut tx,
        &client.actor(Actor::customer()),
        "account.notifications_updated",
        "user",
        &guest.user_id.to_string(),
        json!({ "category": body.category, "channel": body.channel, "enabled": body.enabled }),
    )
    .await?;
    tx.commit().await?;
    notifications(State(state), guest).await
}

// ---- Sessions -------------------------------------------------------------------------------

pub async fn list_sessions(
    State(state): State<AppState>,
    guest: GuestUser,
) -> AppResult<Json<Value>> {
    let list = sessions::list(&state.db, guest.user_id, guest.session_id, Utc::now()).await?;
    Ok(Json(json!({ "sessions": list })))
}

pub async fn revoke_session(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let owned = sqlx::query_scalar!(
        "SELECT user_id = $2 AS \"ok!\" FROM user_sessions WHERE id = $1",
        id,
        guest.user_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !owned {
        return Err(AppError::NotFound);
    }
    sessions::revoke(&state.db, id, "signed_out_by_user", Utc::now()).await?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "auth.session_revoked",
        "user",
        &guest.user_id.to_string(),
        json!({ "session_id": id }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Password -------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NewPasswordBody {
    pub password: String,
}

/// Adds or changes the password (fresh authentication required).
pub async fn set_password(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<NewPasswordBody>,
) -> AppResult<StatusCode> {
    let now = Utc::now();
    sessions::require_recent_auth(&state.db, guest.session_id, now).await?;
    passwords::validate_new(
        &state.http,
        state.config.booking.hibp_enabled,
        &body.password,
    )
    .await?;
    let hash = password::hash(body.password).await?;
    let had = sqlx::query_scalar!(
        "UPDATE users u SET password_hash = $2 FROM users old WHERE u.id = $1 AND old.id = $1
         RETURNING old.password_hash IS NOT NULL AS \"had!\"",
        guest.user_id,
        hash
    )
    .fetch_one(&state.db)
    .await?;
    let change = if had { "changed" } else { "added" };
    // Other devices must sign in again after a password change.
    sessions::revoke_all(
        &state.db,
        guest.user_id,
        Some(guest.session_id),
        "password_changed",
        now,
    )
    .await?;
    let mut tx = state.db.begin().await?;
    audit::record(
        &mut tx,
        &client.actor(Actor::customer()),
        "auth.password_set",
        "user",
        &guest.user_id.to_string(),
        json!({ "change": change }),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::AuthPasswordChanged {
            user_id: guest.user_id,
            change: change.into(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes the password: email codes (and linked accounts) remain.
pub async fn remove_password(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
) -> AppResult<StatusCode> {
    let now = Utc::now();
    sessions::require_recent_auth(&state.db, guest.session_id, now).await?;
    let verified = sqlx::query_scalar!(
        "SELECT email_verified_at IS NOT NULL AS \"v!\" FROM users WHERE id = $1",
        guest.user_id
    )
    .fetch_one(&state.db)
    .await?;
    let identities = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM user_identities WHERE user_id = $1",
        guest.user_id
    )
    .fetch_one(&state.db)
    .await?;
    if !verified && identities == 0 {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "last_sign_in_method",
            "keep at least one way to sign in",
        ));
    }
    sqlx::query!(
        "UPDATE users SET password_hash = NULL WHERE id = $1",
        guest.user_id
    )
    .execute(&state.db)
    .await?;
    let mut tx = state.db.begin().await?;
    audit::record(
        &mut tx,
        &client.actor(Actor::customer()),
        "auth.password_removed",
        "user",
        &guest.user_id.to_string(),
        json!({}),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::AuthPasswordChanged {
            user_id: guest.user_id,
            change: "removed".into(),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Email change ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EmailBody {
    pub email: String,
}

/// Sends a code to the new address (and a notice to the current one). Fresh authentication
/// required. Answers the same whether the new address is taken or not.
pub async fn request_email_change(
    State(state): State<AppState>,
    guest: GuestUser,
    Json(body): Json<EmailBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let now = Utc::now();
    sessions::require_recent_auth(&state.db, guest.session_id, now).await?;
    rate_limit(
        &state.cache,
        "acct_email_change",
        &guest.user_id.to_string(),
        5,
        std::time::Duration::from_secs(3600),
    )
    .await?;
    let new_email = normalize_email(&body.email);
    if !new_email.contains('@') || new_email.len() > 254 {
        return Err(field("email", "invalid"));
    }
    let code = six_digit_code();
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "UPDATE auth_tokens SET used_at = $2 WHERE user_id = $1 AND purpose = 'email_change' AND used_at IS NULL",
        guest.user_id,
        now
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO auth_tokens (user_id, purpose, token_hash, expires_at, data) VALUES ($1, 'email_change', $2, $3, $4)",
        guest.user_id,
        sha256(&format!("{}:{code}:{}", guest.user_id, Uuid::now_v7())),
        now + Duration::minutes(15),
        json!({ "email": new_email, "code_hash": crate::services::accounts::tokens::sha256_hex(&format!("{}:{code}", guest.user_id)) })
    )
    .execute(&mut *tx)
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::AuthEmailChangeRequested {
            user_id: guest.user_id,
            new_email: new_email.clone(),
            code,
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "masked_email": crate::services::accounts::tokens::mask_email(&new_email) })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct CodeOnly {
    pub code: String,
}

/// Applies the new address. A customer record already using it (guest bookings made with the
/// new address) is merged into the account: the address is now verified as the user's.
pub async fn confirm_email_change(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<CodeOnly>,
) -> AppResult<Json<AccountView>> {
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    let token = sqlx::query!(
        "SELECT id, data, attempts FROM auth_tokens
         WHERE user_id = $1 AND purpose = 'email_change' AND used_at IS NULL AND expires_at > $2
         ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
        guest.user_id,
        now
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| {
        AppError::domain(
            StatusCode::GONE,
            "code_expired",
            "this code expired: ask for a new one",
        )
    })?;
    let expected = token.data["code_hash"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let given = crate::services::accounts::tokens::sha256_hex(&format!(
        "{}:{}",
        guest.user_id,
        body.code.trim()
    ));
    if !crate::services::accounts::tokens::constant_time_eq(expected.as_bytes(), given.as_bytes()) {
        let attempts = token.attempts + 1;
        sqlx::query!(
            "UPDATE auth_tokens SET attempts = $2, used_at = CASE WHEN $2 >= 5 THEN $3::timestamptz ELSE NULL END WHERE id = $1",
            token.id,
            attempts,
            now
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_code",
            "this code is not valid",
        )
        .with_details(json!({ "attempts_left": (5 - attempts).max(0) })));
    }
    let new_email = token.data["email"].as_str().unwrap_or_default().to_string();
    let taken = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM users WHERE email = $1::text::citext AND id <> $2 AND deleted_at IS NULL) AS \"t!\"",
        new_email,
        guest.user_id
    )
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "email_taken",
            "another account uses this address",
        ));
    }
    let old_email = sqlx::query_scalar!(
        r#"SELECT email::text AS "e!" FROM users WHERE id = $1"#,
        guest.user_id
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE auth_tokens SET used_at = $2 WHERE id = $1",
        token.id,
        now
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE users SET email = $2::text::citext, email_verified_at = $3 WHERE id = $1",
        guest.user_id,
        new_email,
        now
    )
    .execute(&mut *tx)
    .await?;
    let mine = signin::link_customer(
        &mut tx,
        guest.user_id,
        &old_email,
        &crate::db::types::Locale::new("fr"),
    )
    .await?;
    // Guest bookings made with the new address join the account.
    if let Some(other) = sqlx::query_scalar!(
        "SELECT id FROM customers WHERE email = $1::text::citext AND id <> $2 AND user_id IS NULL AND deleted_at IS NULL",
        new_email,
        mine
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        sqlx::query!("UPDATE bookings SET customer_id = $2 WHERE customer_id = $1", other, mine)
            .execute(&mut *tx)
            .await?;
        sqlx::query!("UPDATE customers SET deleted_at = $2, email = NULL, internal_notes = concat_ws(E'\\n', internal_notes, 'Merged into ' || $3::text) WHERE id = $1", other, now, mine.to_string())
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query!(
        "UPDATE customers SET email = $2::text::citext WHERE id = $1",
        mine,
        new_email
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &client.actor(Actor::customer()),
        "auth.email_changed",
        "user",
        &guest.user_id.to_string(),
        json!({ "before": { "email": old_email }, "after": { "email": new_email } }),
    )
    .await?;
    outbox::enqueue(
        &mut tx,
        &Event::AuthEmailChanged {
            user_id: guest.user_id,
            old_email,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(account_view(&state, guest.user_id).await?))
}
