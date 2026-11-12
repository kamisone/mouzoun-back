use axum::{Json, extract::State, http::StatusCode};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use validator::Validate;

use super::{AuthUser, jwt, password, session};
use crate::{
    db::identity::UserStatus,
    error::{AppError, AppResult},
    services::identity::{self, UserProfile},
    state::AppState,
    web::extract::ApiJson,
};

/// Login only bounds the input size: format or strength rules here would reject valid accounts
/// (e.g. an admin created with a short password) with a 422. Wrong input is simply a 401.
#[derive(Debug, Deserialize, Validate)]
pub struct LoginRequest {
    #[validate(length(min = 1, max = 254))]
    pub email: String,
    #[validate(length(min = 1, max = 1024))]
    pub password: String,
}

/// The token itself is only ever sent as an `HttpOnly` cookie, never in the body.
#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub user: UserProfile,
    /// Session lifetime in seconds.
    pub expires_in: i64,
}

/// POST /v1/auth/login — back office only (staff). With two-factor required (default), answers
/// `{mfa_required, mfa_token}` (or `{mfa_enrollment_required, mfa_token, otpauth_uri, secret}`
/// the first time) and sets the session cookie only after `POST /v1/auth/mfa`.
pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    client: crate::web::client::ClientInfo,
    ApiJson(body): ApiJson<LoginRequest>,
) -> AppResult<(CookieJar, Json<serde_json::Value>)> {
    body.validate()?;
    crate::web::limits::rate_limit(
        &state.cache,
        "staff_login_ip",
        &client.key(),
        20,
        std::time::Duration::from_secs(900),
    )
    .await?;
    crate::web::limits::rate_limit(
        &state.cache,
        "staff_login_email",
        &crate::services::accounts::tokens::sha256_hex(&body.email.trim().to_lowercase()),
        10,
        std::time::Duration::from_secs(900),
    )
    .await?;
    let candidate = identity::find_login(&state.db, &body.email)
        .await?
        .ok_or(AppError::Unauthorized)?;
    // Invited (no password yet) and suspended accounts cannot log in.
    let hash = match (candidate.status, candidate.password_hash) {
        (UserStatus::Active, Some(hash)) => hash,
        _ => return Err(AppError::Unauthorized),
    };
    if !password::verify(body.password, hash).await? {
        return Err(AppError::Unauthorized);
    }
    let user = identity::profile(&state.db, candidate.id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    // A guest account can never enter the back office.
    if !user.is_staff {
        return Err(AppError::Unauthorized);
    }
    if super::mfa::required(&state).await? {
        return Ok((
            jar,
            Json(super::mfa::challenge(&state, user.id, &user.email).await?),
        ));
    }
    let (jar, session) = open_staff_session(&state, jar, user).await?;
    Ok((jar, Json(serde_json::to_value(session).unwrap_or_default())))
}

/// Issues the back-office session cookie (after the password, and the second factor).
pub(crate) async fn open_staff_session(
    state: &AppState,
    jar: CookieJar,
    user: UserProfile,
) -> AppResult<(CookieJar, SessionResponse)> {
    identity::record_login(&state.db, user.id, Utc::now()).await?;
    let ttl = state.config.jwt_ttl_minutes;
    let token = jwt::issue(&state.config.jwt_secret, user.id, user.is_staff, ttl)?;
    let jar = jar.add(session::session_cookie(
        token,
        ttl,
        state.config.cookie_secure,
    ));
    Ok((
        jar,
        SessionResponse {
            user,
            expires_in: ttl * 60,
        },
    ))
}

/// POST /v1/auth/logout — expires the session cookie.
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> (CookieJar, StatusCode) {
    (
        jar.add(session::removal_cookie(state.config.cookie_secure)),
        StatusCode::NO_CONTENT,
    )
}

/// GET /v1/auth/me — the current user, with roles and permissions.
pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<UserProfile>> {
    identity::profile(&state.db, user.id)
        .await?
        .map(Json)
        .ok_or(AppError::Unauthorized)
}
