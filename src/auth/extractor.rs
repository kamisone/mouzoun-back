use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts},
};
use uuid::Uuid;

use super::{Role, jwt};
use crate::{error::AppError, state::AppState};

/// Any authenticated user (valid Bearer token).
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub id: Uuid,
    pub role: Role,
}

/// Authenticated user with admin or staff role.
#[derive(Debug, Clone, Copy)]
pub struct AdminUser(pub AuthUser);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(AppError::Unauthorized)?;
        let claims = jwt::verify(&state.config.jwt_secret, token)?;
        Ok(AuthUser {
            id: claims.sub,
            role: claims.role,
        })
    }
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if !user.role.is_staff() {
            return Err(AppError::Forbidden);
        }
        Ok(AdminUser(user))
    }
}
