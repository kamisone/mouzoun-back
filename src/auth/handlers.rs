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

#[derive(Debug, Deserialize, Validate)]
pub struct LoginRequest {
    #[validate(email)]
    pub email: String,
    #[validate(length(min = 8, max = 128))]
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
    .bind(body.email.to_lowercase())
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
