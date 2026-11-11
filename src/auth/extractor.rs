use axum::{
    extract::{FromRequestParts, Request},
    http::{header, request::Parts},
    middleware::Next,
    response::Response,
};
use axum_extra::extract::cookie::CookieJar;
use uuid::Uuid;

use super::{Role, jwt, session};
use crate::{error::AppError, state::AppState};

/// Any authenticated user.
///
/// Browsers authenticate with the `HttpOnly` session cookie; non-browser clients (scripts,
/// a future mobile app) may send the same JWT as `Authorization: Bearer <token>`.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub id: Uuid,
    pub role: Role,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let cookie_token = jar
            .get(session::cookie_name(state.config.cookie_secure))
            .map(|c| c.value().to_owned());
        let token = match cookie_token {
            Some(token) => token,
            None => parts
                .headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned)
                .ok_or(AppError::Unauthorized)?,
        };
        let claims = jwt::verify(&state.config.jwt_secret, &token)?;
        Ok(AuthUser {
            id: claims.sub,
            role: claims.role,
        })
    }
}

/// Guard for the whole back-office router (`/v1/admin/...`): admin or staff only.
///
/// Applied once with `route_layer`, so every admin route, present or future, is protected by
/// construction. The verified user is stored in the request extensions for handlers that need
/// it (`Extension<AuthUser>`).
pub async fn require_staff(
    user: AuthUser,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if !user.role.is_staff() {
        return Err(AppError::Forbidden);
    }
    request.extensions_mut().insert(user);
    Ok(next.run(request).await)
}
