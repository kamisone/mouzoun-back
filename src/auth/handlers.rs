use axum::{Json, extract::State, http::StatusCode};
use axum_extra::extract::cookie::CookieJar;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

use super::{AuthUser, Role, jwt, password, session};
use crate::{
    error::{AppError, AppResult},
    state::AppState,
    web::extract::ApiJson,
};

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct UserDto {
    pub id: Uuid,
    pub email: String,
    pub full_name: String,
    pub role: Role,
    pub created_at: DateTime<Utc>,
}

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
    pub user: UserDto,
    /// Session lifetime in seconds.
    pub expires_in: i64,
}

#[derive(sqlx::FromRow)]
struct UserWithHash {
    id: Uuid,
    email: String,
    full_name: String,
    role: Role,
    created_at: DateTime<Utc>,
    password_hash: String,
}

/// POST /v1/auth/login — sets the session cookie.
pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    ApiJson(body): ApiJson<LoginRequest>,
) -> AppResult<(CookieJar, Json<SessionResponse>)> {
    body.validate()?;

    let user = sqlx::query_as::<_, UserWithHash>(
        "SELECT id, email, full_name, role, created_at, password_hash
         FROM users WHERE email = $1 AND is_active = TRUE",
    )
    .bind(body.email.trim().to_lowercase())
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Unauthorized)?;

    if !password::verify(body.password, user.password_hash).await? {
        return Err(AppError::Unauthorized);
    }

    let ttl = state.config.jwt_ttl_minutes;
    let token = jwt::issue(&state.config.jwt_secret, user.id, user.role, ttl)?;
    let jar = jar.add(session::session_cookie(
        token,
        ttl,
        state.config.cookie_secure,
    ));

    let response = SessionResponse {
        expires_in: ttl * 60,
        user: UserDto {
            id: user.id,
            email: user.email,
            full_name: user.full_name,
            role: user.role,
            created_at: user.created_at,
        },
    };
    Ok((jar, Json(response)))
}

/// POST /v1/auth/logout — expires the session cookie.
pub async fn logout(State(state): State<AppState>, jar: CookieJar) -> (CookieJar, StatusCode) {
    (
        jar.add(session::removal_cookie(state.config.cookie_secure)),
        StatusCode::NO_CONTENT,
    )
}

/// GET /v1/auth/me
pub async fn me(State(state): State<AppState>, user: AuthUser) -> AppResult<Json<UserDto>> {
    let dto = sqlx::query_as::<_, UserDto>(
        "SELECT id, email, full_name, role, created_at FROM users WHERE id = $1 AND is_active = TRUE",
    )
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Unauthorized)?;
    Ok(Json(dto))
}
