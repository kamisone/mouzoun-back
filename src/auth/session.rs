//! Browser session cookie carrying the JWT.
//!
//! `HttpOnly` (unreadable by JavaScript, so an XSS bug cannot steal it), `SameSite=Lax` (not sent
//! on cross-site POST/PUT/DELETE, the main CSRF defence) and, in production, `Secure` with the
//! `__Host-` prefix (HTTPS only, bound to the exact host: a sub-domain cannot set or overwrite it).

use axum_extra::extract::cookie::{Cookie, SameSite};
use time::Duration;

/// Cookie name. The `__Host-` prefix requires `Secure`, so plain-HTTP local development uses
/// the unprefixed name.
pub fn cookie_name(secure: bool) -> &'static str {
    if secure {
        "__Host-mouzoun_session"
    } else {
        "mouzoun_session"
    }
}

pub fn session_cookie(token: String, ttl_minutes: i64, secure: bool) -> Cookie<'static> {
    Cookie::build((cookie_name(secure), token))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(Duration::minutes(ttl_minutes))
        .build()
}

/// Expires the session cookie (logout).
pub fn removal_cookie(secure: bool) -> Cookie<'static> {
    Cookie::build((cookie_name(secure), ""))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(Duration::ZERO)
        .build()
}
