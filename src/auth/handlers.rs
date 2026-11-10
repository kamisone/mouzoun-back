use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use validator::Validate;

use super::{AuthUser, Role, jwt, password};
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

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub user: UserDto,
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

pub async fn login(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<LoginRequest>,
) -> AppResult<Json<LoginResponse>> {
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
    let access_token = jwt::issue(&state.config.jwt_secret, user.id, user.role, ttl)?;

    Ok(Json(LoginResponse {
        access_token,
        token_type: "Bearer",
        expires_in: ttl * 60,
        user: UserDto {
            id: user.id,
            email: user.email,
            full_name: user.full_name,
            role: user.role,
            created_at: user.created_at,
        },
    }))
}

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
