//! CSRF defence in depth for cookie-authenticated requests.
//!
//! The session cookie is `SameSite=Lax`, so browsers do not send it with cross-site POST/PUT/
//! PATCH/DELETE requests. On top of that, any state-changing request whose `Origin` header names
//! a site outside the allow-list (`CORS_ORIGINS`) is refused. Requests without an `Origin`
//! (non-browser clients authenticating with a Bearer token) are not affected.

use axum::{
    extract::{Request, State},
    http::{Method, header},
    middleware::Next,
    response::Response,
};

use crate::{error::AppError, state::AppState};

pub async fn reject_foreign_origin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if !safe && let Some(origin) = request.headers().get(header::ORIGIN) {
        let allowed = origin
            .to_str()
            .is_ok_and(|o| state.config.cors_origins.iter().any(|a| a == o));
        if !allowed {
            tracing::warn!(?origin, method = %request.method(), uri = %request.uri(), "rejected cross-origin request");
            return Err(AppError::Forbidden);
        }
    }
    Ok(next.run(request).await)
}
