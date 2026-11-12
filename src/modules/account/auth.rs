//! Guest sign-in: "Sign in or create an account" by email code / magic link, password, refresh,
//! sign out (this device or all), re-authentication before sensitive actions.

use std::time::Duration as StdDuration;

use axum::{Json, extract::State, http::StatusCode};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use super::profile::account_view;
use crate::{
    auth::{GuestUser, password},
    error::{AppError, AppResult},
    modules::booking::cart,
    services::{
        accounts::{
            sessions::{self, Refreshed},
            signin,
            tokens::{mask_email, normalize_email, sha256_hex},
        },
        audit::{self, Actor},
        turnstile,
    },
    state::AppState,
    web::{
        client::{ClientInfo, RequestLocale},
        limits::rate_limit,
    },
};

fn valid_email(email: &str) -> bool {
    let e = email.trim();
    e.len() <= 254
        && !e.contains(char::is_whitespace)
        && e.split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.ends_with('.'))
}

#[derive(Debug, Deserialize)]
pub struct IdentifyBody {
    pub email: String,
    #[serde(default)]
    pub return_to: Option<String>,
    #[serde(default)]
    pub captcha_token: Option<String>,
}

/// Sends a code + magic link. Same answer whether or not the address has an account.
pub async fn identify(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    Json(body): Json<IdentifyBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    if !valid_email(&body.email) {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_email",
            "enter a valid email address",
        ));
    }
    rate_limit(
        &state.cache,
        "acct_identify_ip",
        &client.key(),
        20,
        StdDuration::from_secs(900),
    )
    .await?;
    turnstile::verify(
        &state.http,
        state.config.booking.turnstile_secret.as_deref(),
        body.captcha_token.as_deref(),
        client.ip,
    )
    .await?;
    let email = normalize_email(&body.email);
    // Per address: at most 5 emails per 15 minutes. Beyond, nothing is sent but the answer is
    // the same (the address owner is not spammed, an attacker learns nothing).
    let per_email = rate_limit(
        &state.cache,
        "acct_identify_email",
        &sha256_hex(&email),
        5,
        StdDuration::from_secs(900),
    )
    .await;
    let return_to = body
        .return_to
        .as_deref()
        .filter(|r| r.starts_with('/') && !r.starts_with("//"));
    if per_email.is_ok() {
        signin::send_code(
            &state.db,
            &state.cache,
            &email,
            &locale,
            return_to,
            Utc::now(),
        )
        .await?;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "sent": true,
            "masked_email": mask_email(&email),
            "code_minutes": signin::CODE_MINUTES,
            "resend_after_seconds": 30,
        })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct CodeBody {
    pub email: String,
    pub code: String,
    #[serde(default)]
    pub shared_device: bool,
}

pub async fn verify_code(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    jar: CookieJar,
    Json(body): Json<CodeBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    rate_limit(
        &state.cache,
        "acct_code_ip",
        &client.key(),
        30,
        StdDuration::from_secs(900),
    )
    .await?;
    signin::check_code(&state.cache, &body.email, &body.code).await?;
    let signed = signin::user_for_verified_email(
        &state.db,
        &body.email,
        &locale,
        &client.actor(Actor::customer()),
        Utc::now(),
    )
    .await?;
    finish(&state, jar, &client, signed, !body.shared_device, "code").await
}

#[derive(Debug, Deserialize)]
pub struct LinkBody {
    pub email: String,
    pub token: String,
    #[serde(default)]
    pub shared_device: bool,
}

pub async fn verify_link(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    jar: CookieJar,
    Json(body): Json<LinkBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    rate_limit(
        &state.cache,
        "acct_code_ip",
        &client.key(),
        30,
        StdDuration::from_secs(900),
    )
    .await?;
    signin::check_link(&state.cache, &body.email, &body.token).await?;
    let signed = signin::user_for_verified_email(
        &state.db,
        &body.email,
        &locale,
        &client.actor(Actor::customer()),
        Utc::now(),
    )
    .await?;
    finish(
        &state,
        jar,
        &client,
        signed,
        !body.shared_device,
        "magic_link",
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct PasswordBody {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub shared_device: bool,
}

fn bad_credentials() -> AppError {
    AppError::domain(
        StatusCode::UNAUTHORIZED,
        "invalid_credentials",
        "wrong email or password",
    )
}

pub async fn password_login(
    State(state): State<AppState>,
    client: ClientInfo,
    jar: CookieJar,
    Json(body): Json<PasswordBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let email = normalize_email(&body.email);
    rate_limit(
        &state.cache,
        "acct_password_ip",
        &client.key(),
        30,
        StdDuration::from_secs(900),
    )
    .await?;
    // Per address: 10 tries per 15 minutes, then wait (never a permanent lockout).
    rate_limit(
        &state.cache,
        "acct_password_email",
        &sha256_hex(&email),
        10,
        StdDuration::from_secs(900),
    )
    .await?;
    let user = sqlx::query!(
        "SELECT id, password_hash, status FROM users WHERE email = $1::text::citext AND deleted_at IS NULL",
        email
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(user) = user else {
        // Same work as a real check: no timing difference for unknown addresses.
        let _ = password::hash("timing-equaliser".into()).await;
        return Err(bad_credentials());
    };
    let Some(hash) = user.password_hash.filter(|_| user.status == "active") else {
        return Err(bad_credentials());
    };
    if body.password.len() > 1024 || !password::verify(body.password, hash).await? {
        return Err(bad_credentials());
    }
    let signed = signin::SignedIn {
        user_id: user.id,
        created: false,
        needs_profile: false,
    };
    finish(
        &state,
        jar,
        &client,
        signed,
        !body.shared_device,
        "password",
    )
    .await
}

/// Opens the session, sends the "new device" notice when relevant, merges the basket.
pub(crate) async fn finish(
    state: &AppState,
    jar: CookieJar,
    client: &ClientInfo,
    signed: signin::SignedIn,
    persistent: bool,
    method: &str,
) -> AppResult<(CookieJar, Json<Value>)> {
    let now = Utc::now();
    let issued = sessions::create(
        &state.db,
        &state.config.jwt_secret,
        signed.user_id,
        persistent,
        client.user_agent.as_deref(),
        client.ip_network(),
        now,
    )
    .await?;
    sqlx::query!(
        "UPDATE users SET last_login_at = $2 WHERE id = $1",
        signed.user_id,
        now
    )
    .execute(&state.db)
    .await?;
    signin::notice_new_device(
        &state.db,
        signed.user_id,
        issued.session_id,
        client.user_agent.as_deref(),
    )
    .await?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "auth.signed_in",
        "user",
        &signed.user_id.to_string(),
        json!({ "method": method, "session_id": issued.session_id, "persistent": persistent }),
    )
    .await?;
    drop(conn);
    let mut jar = cart::merge_on_sign_in(state, jar, signed.user_id).await;
    for c in issued.cookies(state.config.cookie_secure) {
        jar = jar.add(c);
    }
    let account = account_view(state, signed.user_id).await?;
    Ok((
        jar,
        Json(
            json!({ "account": account, "created": signed.created, "needs_profile": signed.needs_profile }),
        ),
    ))
}

/// Explicit refresh (the middleware also refreshes transparently).
pub async fn refresh(
    State(state): State<AppState>,
    jar: CookieJar,
) -> AppResult<(CookieJar, StatusCode)> {
    let secure = state.config.cookie_secure;
    let token = jar
        .get(sessions::refresh_cookie_name(secure))
        .map(|c| c.value().to_string())
        .ok_or(AppError::Unauthorized)?;
    match sessions::refresh(&state.db, &state.config.jwt_secret, &token, Utc::now()).await? {
        Refreshed::Rotated(issued) => {
            let mut jar = jar;
            for c in issued.cookies(secure) {
                jar = jar.add(c);
            }
            Ok((jar, StatusCode::NO_CONTENT))
        }
        Refreshed::Concurrent { .. } => Ok((jar, StatusCode::NO_CONTENT)),
    }
}

pub async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
    guest: Option<axum::Extension<GuestUser>>,
) -> AppResult<(CookieJar, StatusCode)> {
    if let Some(axum::Extension(g)) = guest {
        sessions::revoke(&state.db, g.session_id, "logout", Utc::now()).await?;
    }
    let mut jar = jar;
    for c in sessions::removal_cookies(state.config.cookie_secure) {
        jar = jar.add(c);
    }
    Ok((jar, StatusCode::NO_CONTENT))
}

/// "Sign out of all devices", this one included.
pub async fn logout_all(
    State(state): State<AppState>,
    jar: CookieJar,
    client: ClientInfo,
    guest: GuestUser,
) -> AppResult<(CookieJar, Json<Value>)> {
    let revoked =
        sessions::revoke_all(&state.db, guest.user_id, None, "logout_all", Utc::now()).await?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "auth.logout_all",
        "user",
        &guest.user_id.to_string(),
        json!({ "sessions": revoked }),
    )
    .await?;
    let mut jar = jar;
    for c in sessions::removal_cookies(state.config.cookie_secure) {
        jar = jar.add(c);
    }
    Ok((jar, Json(json!({ "revoked": revoked }))))
}

/// Sends a code to the account's own address, to confirm it is you before a sensitive action.
pub async fn reauth_send(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    guest: GuestUser,
) -> AppResult<(StatusCode, Json<Value>)> {
    rate_limit(
        &state.cache,
        "acct_reauth",
        &guest.user_id.to_string(),
        5,
        StdDuration::from_secs(900),
    )
    .await?;
    let email = sqlx::query_scalar!(
        r#"SELECT email::text AS "email!" FROM users WHERE id = $1"#,
        guest.user_id
    )
    .fetch_one(&state.db)
    .await?;
    signin::send_code(&state.db, &state.cache, &email, &locale, None, Utc::now()).await?;
    let _ = client;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "masked_email": mask_email(&email) })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct ReauthBody {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

pub async fn reauth(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<ReauthBody>,
) -> AppResult<StatusCode> {
    rate_limit(
        &state.cache,
        "acct_reauth_check",
        &guest.user_id.to_string(),
        10,
        StdDuration::from_secs(900),
    )
    .await?;
    let user = sqlx::query!(
        r#"SELECT email::text AS "email!", password_hash FROM users WHERE id = $1"#,
        guest.user_id
    )
    .fetch_one(&state.db)
    .await?;
    match (body.code, body.password) {
        (Some(code), _) => signin::check_code(&state.cache, &user.email, &code).await?,
        (None, Some(pass)) => {
            let hash = user.password_hash.ok_or_else(bad_credentials)?;
            if !password::verify(pass, hash).await? {
                return Err(bad_credentials());
            }
        }
        _ => return Err(AppError::BadRequest("code or password required".into())),
    }
    sessions::mark_reauthenticated(&state.db, guest.session_id, Utc::now()).await?;
    let _ = client;
    Ok(StatusCode::NO_CONTENT)
}
