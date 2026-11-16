//! Optional second factor of the back office: TOTP (authenticator app).
//!
//! Each staff member turns it on in the back-office settings (`/v1/admin/account/2fa`): `setup`
//! returns a new secret (kept in Redis for 15 minutes), `enable` stores it once a first code
//! proves the app is set up, `disable` needs a current code. When it is on, `login` returns a
//! short-lived `mfa_token` (Redis, 5 minutes, 5 tries) instead of the session, and
//! `POST /v1/auth/mfa` checks the code and opens the session. Codes cannot be replayed (last
//! used step kept in Redis).

use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use chrono::{DateTime, Utc};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    Staff,
    handlers::{SessionResponse, open_staff_session},
};
use crate::{
    error::{AppError, AppResult},
    services::{
        accounts::{
            tokens::{random_token, sha256_hex},
            totp,
        },
        audit,
        booking::register,
        identity,
    },
    state::AppState,
    web::limits::rate_limit,
};

/// Pending sign-in: the password was right, the code is still to come.
#[derive(Serialize, Deserialize)]
struct Ticket {
    user_id: Uuid,
    attempts: u32,
}

fn ticket_key(state: &AppState, token: &str) -> String {
    state.cache.key(&format!("mfa:{}", sha256_hex(token)))
}

fn setup_key(state: &AppState, user_id: Uuid) -> String {
    state.cache.key(&format!("mfa:setup:{user_id}"))
}

fn invalid_code() -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_code",
        "this code is not valid",
    )
}

/// The user's secret when two-factor sign-in is on.
async fn stored_secret(state: &AppState, user_id: Uuid) -> AppResult<Option<Vec<u8>>> {
    let enc = sqlx::query_scalar!(
        "SELECT totp_secret_enc FROM users WHERE id = $1 AND totp_enabled_at IS NOT NULL",
        user_id
    )
    .fetch_optional(&state.db)
    .await?
    .flatten();
    let Some(enc) = enc else { return Ok(None) };
    let secret = register::decrypt(&totp::key(&state.config.jwt_secret), &enc)
        .and_then(|text| totp::base32_decode(&text))
        .ok_or_else(|| AppError::Internal(anyhow::anyhow!("unreadable TOTP secret")))?;
    Ok(Some(secret))
}

/// Checks a code and burns its time step (a code works once).
async fn accept_code(
    state: &AppState,
    user_id: Uuid,
    secret: &[u8],
    code: &str,
) -> AppResult<bool> {
    let Some(step) = totp::verify(secret, code, Utc::now().timestamp()) else {
        return Ok(false);
    };
    let mut conn = state.cache.connection();
    let last_key = state.cache.key(&format!("mfa:last:{user_id}"));
    let last: Option<i64> = conn.get(&last_key).await.unwrap_or(None);
    if last.is_some_and(|l| step <= l) {
        return Ok(false);
    }
    let _: () = conn.set_ex(&last_key, step, 120).await?;
    Ok(true)
}

/// After the password: `Some(challenge)` when the account has two-factor sign-in on.
pub async fn challenge(state: &AppState, user_id: Uuid) -> AppResult<Option<Value>> {
    let enrolled = sqlx::query_scalar!(
        "SELECT totp_enabled_at IS NOT NULL AND totp_secret_enc IS NOT NULL AS \"e!\" FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db)
    .await?;
    if !enrolled {
        return Ok(None);
    }
    let token = random_token();
    let ticket = Ticket {
        user_id,
        attempts: 0,
    };
    let mut conn = state.cache.connection();
    let _: () = conn
        .set_ex(
            ticket_key(state, &token),
            serde_json::to_string(&ticket).unwrap_or_default(),
            300,
        )
        .await?;
    Ok(Some(json!({ "mfa_required": true, "mfa_token": token })))
}

#[derive(Debug, Deserialize)]
pub struct MfaBody {
    pub mfa_token: String,
    pub code: String,
}

/// POST /v1/auth/mfa — second step of the sign-in.
pub async fn verify(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(body): Json<MfaBody>,
) -> AppResult<(CookieJar, Json<SessionResponse>)> {
    let key = ticket_key(&state, &body.mfa_token);
    let mut conn = state.cache.connection();
    let raw: Option<String> = conn.get(&key).await?;
    let mut ticket: Ticket = raw
        .and_then(|r| serde_json::from_str(&r).ok())
        .ok_or_else(|| AppError::domain(StatusCode::GONE, "mfa_expired", "sign in again"))?;
    // Turned off since the password step: sign in again.
    let secret = stored_secret(&state, ticket.user_id)
        .await?
        .ok_or_else(|| AppError::domain(StatusCode::GONE, "mfa_expired", "sign in again"))?;
    if !accept_code(&state, ticket.user_id, &secret, &body.code).await? {
        ticket.attempts += 1;
        if ticket.attempts >= 5 {
            let _: () = conn.del(&key).await?;
        } else {
            let _: () = conn
                .set_ex(
                    &key,
                    serde_json::to_string(&ticket).unwrap_or_default(),
                    300,
                )
                .await?;
        }
        return Err(invalid_code());
    }
    let _: () = conn.del(&key).await?;
    let user = identity::profile(&state.db, ticket.user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let (jar, session) = open_staff_session(&state, jar, user).await?;
    Ok((jar, Json(session)))
}

// --- Settings (back office, the signed-in staff member's own account) ---

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/account/2fa", get(status))
        .route("/account/2fa/setup", post(setup))
        .route("/account/2fa/enable", post(enable))
        .route("/account/2fa/disable", post(disable))
}

#[derive(Debug, Serialize)]
pub struct MfaStatus {
    pub enabled: bool,
    pub enabled_at: Option<DateTime<Utc>>,
}

async fn current_status(state: &AppState, user_id: Uuid) -> AppResult<MfaStatus> {
    let enabled_at = sqlx::query_scalar!(
        "SELECT totp_enabled_at FROM users WHERE id = $1 AND totp_secret_enc IS NOT NULL",
        user_id
    )
    .fetch_optional(&state.db)
    .await?
    .flatten();
    Ok(MfaStatus {
        enabled: enabled_at.is_some(),
        enabled_at,
    })
}

/// GET /v1/admin/account/2fa
pub async fn status(State(state): State<AppState>, staff: Staff) -> AppResult<Json<MfaStatus>> {
    Ok(Json(current_status(&state, staff.id).await?))
}

/// POST /v1/admin/account/2fa/setup — a new secret to scan (not active until `enable`).
pub async fn setup(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    if current_status(&state, staff.id).await?.enabled {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "mfa_already_enabled",
            "two-factor sign-in is already on",
        ));
    }
    let email = identity::profile(&state.db, staff.id)
        .await?
        .ok_or(AppError::Unauthorized)?
        .email;
    let secret = totp::new_secret();
    let mut conn = state.cache.connection();
    let _: () = conn
        .set_ex(setup_key(&state, staff.id), totp::base32(&secret), 900)
        .await?;
    Ok(Json(json!({
        "secret": totp::base32(&secret),
        "otpauth_uri": totp::otpauth_uri(&secret, &email, "Mouzoun"),
    })))
}

#[derive(Debug, Deserialize)]
pub struct CodeBody {
    pub code: String,
}

async fn limit_attempts(state: &AppState, user_id: Uuid) -> AppResult<()> {
    rate_limit(
        &state.cache,
        "mfa_settings",
        &user_id.to_string(),
        10,
        Duration::from_secs(900),
    )
    .await
}

/// POST /v1/admin/account/2fa/enable — the first code from the app turns it on.
pub async fn enable(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<CodeBody>,
) -> AppResult<Json<MfaStatus>> {
    limit_attempts(&state, staff.id).await?;
    let mut conn = state.cache.connection();
    let pending: Option<String> = conn.get(setup_key(&state, staff.id)).await?;
    let secret = pending
        .as_deref()
        .and_then(totp::base32_decode)
        .ok_or_else(|| {
            AppError::domain(
                StatusCode::GONE,
                "mfa_setup_expired",
                "start the setup again",
            )
        })?;
    if !accept_code(&state, staff.id, &secret, &body.code).await? {
        return Err(invalid_code());
    }
    let enc = register::encrypt(&totp::key(&state.config.jwt_secret), &totp::base32(&secret))?;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "UPDATE users SET totp_secret_enc = $2, totp_enabled_at = $3 WHERE id = $1",
        staff.id,
        enc,
        Utc::now()
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "auth.totp_enabled",
        "user",
        &staff.id.to_string(),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    let _: () = conn.del(setup_key(&state, staff.id)).await?;
    Ok(Json(current_status(&state, staff.id).await?))
}

/// POST /v1/admin/account/2fa/disable — needs a current code from the app.
pub async fn disable(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<CodeBody>,
) -> AppResult<Json<MfaStatus>> {
    limit_attempts(&state, staff.id).await?;
    let Some(secret) = stored_secret(&state, staff.id).await? else {
        return Ok(Json(current_status(&state, staff.id).await?));
    };
    if !accept_code(&state, staff.id, &secret, &body.code).await? {
        return Err(invalid_code());
    }
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "UPDATE users SET totp_secret_enc = NULL, totp_enabled_at = NULL WHERE id = $1",
        staff.id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "auth.totp_disabled",
        "user",
        &staff.id.to_string(),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(current_status(&state, staff.id).await?))
}
