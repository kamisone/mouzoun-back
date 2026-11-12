//! Second factor of the back office: TOTP (authenticator app). After the password, `login`
//! returns a short-lived `mfa_token` (Redis, 5 minutes, 5 tries); `POST /v1/auth/mfa` checks the
//! code (and, the first time, stores the new secret) and opens the session. Codes cannot be
//! replayed (last used step kept in Redis).

use axum::{Json, extract::State, http::StatusCode};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::handlers::{SessionResponse, open_staff_session};
use crate::{
    error::{AppError, AppResult},
    services::{
        accounts::{tokens::random_token, totp},
        audit::{self, Actor},
        booking::register,
        identity,
    },
    state::AppState,
};

#[derive(Serialize, Deserialize)]
struct Ticket {
    user_id: Uuid,
    /// Present while enrolling: the new secret, confirmed by the first code.
    new_secret: Option<Vec<u8>>,
    attempts: u32,
}

pub async fn required(state: &AppState) -> AppResult<bool> {
    Ok(
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'auth.staff_2fa_required'")
            .fetch_optional(&state.db)
            .await?
            .is_none_or(|v| v != serde_json::Value::Bool(false)),
    )
}

fn ticket_key(state: &AppState, token: &str) -> String {
    state.cache.key(&format!(
        "mfa:{}",
        crate::services::accounts::tokens::sha256_hex(token)
    ))
}

pub async fn challenge(state: &AppState, user_id: Uuid, email: &str) -> AppResult<Value> {
    let enrolled = sqlx::query_scalar!(
        "SELECT totp_enabled_at IS NOT NULL AND totp_secret_enc IS NOT NULL AS \"e!\" FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db)
    .await?;
    let token = random_token();
    let new_secret = (!enrolled).then(totp::new_secret);
    let ticket = Ticket {
        user_id,
        new_secret: new_secret.clone(),
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
    Ok(match new_secret {
        None => json!({ "mfa_required": true, "mfa_token": token }),
        Some(secret) => json!({
            "mfa_enrollment_required": true,
            "mfa_token": token,
            "secret": totp::base32(&secret),
            "otpauth_uri": totp::otpauth_uri(&secret, email, "Mouzoun"),
        }),
    })
}

#[derive(Debug, Deserialize)]
pub struct MfaBody {
    pub mfa_token: String,
    pub code: String,
}

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
    let totp_key = totp::key(&state.config.jwt_secret);
    let secret = match &ticket.new_secret {
        Some(s) => s.clone(),
        None => {
            let enc = sqlx::query_scalar!(
                "SELECT totp_secret_enc FROM users WHERE id = $1",
                ticket.user_id
            )
            .fetch_one(&state.db)
            .await?
            .ok_or(AppError::Unauthorized)?;
            register::decrypt(&totp_key, &enc)
                .and_then(|text| totp::base32_decode(&text))
                .ok_or(AppError::Unauthorized)?
        }
    };
    let now = Utc::now();
    let step = totp::verify(&secret, &body.code, now.timestamp());
    let last_key = state.cache.key(&format!("mfa:last:{}", ticket.user_id));
    let last: Option<i64> = conn.get(&last_key).await.unwrap_or(None);
    let Some(step) = step.filter(|s| last.is_none_or(|l| *s > l)) else {
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
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_code",
            "this code is not valid",
        ));
    };
    let _: () = conn.set_ex(&last_key, step, 120).await?;
    let _: () = conn.del(&key).await?;
    if ticket.new_secret.is_some() {
        // Stored as base32 text inside the AES envelope.
        let enc = register::encrypt(&totp_key, &totp::base32(&secret));
        sqlx::query!(
            "UPDATE users SET totp_secret_enc = $2, totp_enabled_at = $3 WHERE id = $1",
            ticket.user_id,
            enc,
            now
        )
        .execute(&state.db)
        .await?;
        let mut db = state.db.acquire().await?;
        audit::record(
            &mut db,
            &Actor::staff(ticket.user_id),
            "auth.totp_enrolled",
            "user",
            &ticket.user_id.to_string(),
            json!({}),
        )
        .await?;
    }
    let user = identity::profile(&state.db, ticket.user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let (jar, session) = open_staff_session(&state, jar, user).await?;
    Ok((jar, Json(session)))
}
