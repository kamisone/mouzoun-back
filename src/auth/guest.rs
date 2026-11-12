//! Guest (customer account) authentication for the public API.
//!
//! The `guest_session` middleware runs on every public request: a valid access cookie (and an
//! active session) identifies the guest; when the access token expired, the refresh cookie is
//! rotated transparently and the new cookies are added to the response, so the website never
//! handles tokens. Handlers ask for `GuestUser` (401 when absent) or `Option<Extension<GuestUser>>`.

use axum::{
    extract::{FromRequestParts, Request, State},
    http::{HeaderValue, header, request::Parts},
    middleware::Next,
    response::Response,
};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use uuid::Uuid;

use super::jwt;
use crate::{
    error::AppError,
    services::accounts::sessions::{self, Refreshed},
    state::AppState,
};

#[derive(Debug, Clone, Copy)]
pub struct GuestUser {
    pub user_id: Uuid,
    pub session_id: Uuid,
}

impl FromRequestParts<AppState> for GuestUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _: &AppState) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<GuestUser>()
            .copied()
            .ok_or(AppError::Unauthorized)
    }
}

pub async fn guest_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let secure = state.config.cookie_secure;
    let jar = CookieJar::from_headers(request.headers());
    let access = jar
        .get(sessions::access_cookie_name(secure))
        .map(|c| c.value().to_string());
    let refresh = jar
        .get(sessions::refresh_cookie_name(secure))
        .map(|c| c.value().to_string());
    if access.is_none() && refresh.is_none() {
        return next.run(request).await;
    }
    let now = Utc::now();
    if let Some(token) = &access
        && let Ok(claims) = jwt::verify_guest(&state.config.jwt_secret, token)
        && let Some(sid) = claims.sid
        && sessions::is_active(&state.db, sid, now)
            .await
            .unwrap_or(false)
    {
        request.extensions_mut().insert(GuestUser {
            user_id: claims.sub,
            session_id: sid,
        });
        return next.run(request).await;
    }
    let mut set_cookies = Vec::new();
    match refresh {
        Some(token) => {
            match sessions::refresh(&state.db, &state.config.jwt_secret, &token, now).await {
                Ok(Refreshed::Rotated(issued)) => {
                    request.extensions_mut().insert(GuestUser {
                        user_id: issued.user_id,
                        session_id: issued.session_id,
                    });
                    set_cookies.extend(issued.cookies(secure));
                }
                Ok(Refreshed::Concurrent {
                    user_id,
                    session_id,
                }) => {
                    request.extensions_mut().insert(GuestUser {
                        user_id,
                        session_id,
                    });
                }
                Err(_) => set_cookies.extend(sessions::removal_cookies(secure)),
            }
        }
        None => set_cookies.extend(sessions::removal_cookies(secure)),
    }
    let mut response = next.run(request).await;
    // A handler that signs in or out sets its own cookies: do not override them.
    let handler_set = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|v| {
            v.to_str()
                .is_ok_and(|v| v.starts_with(sessions::access_cookie_name(secure)))
        });
    if !handler_set {
        for c in set_cookies {
            if let Ok(v) = HeaderValue::from_str(&c.to_string()) {
                response.headers_mut().append(header::SET_COOKIE, v);
            }
        }
    }
    response
}
