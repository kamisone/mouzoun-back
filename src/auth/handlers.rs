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

/// POST /v1/auth/login — sets the session cookie.
pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    ApiJson(body): ApiJson<LoginRequest>,
) -> AppResult<(CookieJar, Json<SessionResponse>)> {
    body.validate()?;
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
        Json(SessionResponse {
            user,
            expires_in: ttl * 60,
        }),
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
