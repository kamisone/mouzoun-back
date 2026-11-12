//! Cloudflare Turnstile (invisible bot check) on checkout, sign-in and "find my booking".
//! Disabled when `TURNSTILE_SECRET_KEY` / `TURNSTILE_SITE_KEY` are not set (local development,
//! tests); the site gets the site key from `/v1/booking/config`.

use std::net::IpAddr;

use axum::http::StatusCode;
use serde_json::Value;

use crate::error::AppError;

const VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

pub async fn verify(
    http: &reqwest::Client,
    secret: Option<&str>,
    token: Option<&str>,
    ip: Option<IpAddr>,
) -> Result<(), AppError> {
    let Some(secret) = secret else { return Ok(()) };
    let failed = || {
        AppError::domain(
            StatusCode::FORBIDDEN,
            "captcha_failed",
            "the security check failed, try again",
        )
    };
    let token = token
        .filter(|t| !t.is_empty() && t.len() < 4096)
        .ok_or_else(failed)?;
    let mut form = vec![
        ("secret", secret.to_string()),
        ("response", token.to_string()),
    ];
    if let Some(ip) = ip {
        form.push(("remoteip", ip.to_string()));
    }
    let response = http.post(VERIFY_URL).form(&form).send().await;
    match response {
        Ok(r) => {
            let body: Value = r.json().await.unwrap_or_default();
            if body["success"] == true {
                Ok(())
            } else {
                tracing::info!(errors = %body["error-codes"], "turnstile refused");
                Err(failed())
            }
        }
        Err(e) => {
            // Cloudflare unreachable: let the guest through (rate limits still apply).
            tracing::warn!(error = %e, "turnstile unreachable, allowing");
            Ok(())
        }
    }
}
