//! Optional account passwords: at least 10 characters, no composition rules, refused when
//! known in breaches (Have I Been Pwned "range" API, k-anonymity: only the first 5 hex chars of
//! the SHA-1 leave the server). The check fails open when HIBP is unreachable.

use axum::http::StatusCode;
use sha1::{Digest, Sha1};

use crate::error::{AppError, AppResult};

pub const MIN_LENGTH: usize = 10;

pub async fn validate_new(http: &reqwest::Client, enabled: bool, password: &str) -> AppResult<()> {
    if password.chars().count() < MIN_LENGTH {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "password_too_short",
            "at least 10 characters",
        )
        .with_details(serde_json::json!({ "min": MIN_LENGTH })));
    }
    if password.len() > 1024 {
        return Err(AppError::BadRequest("password too long".into()));
    }
    if enabled && breached(http, password).await {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "password_breached",
            "this password appeared in a data breach: choose another one",
        ));
    }
    Ok(())
}

async fn breached(http: &reqwest::Client, password: &str) -> bool {
    let hex: String = Sha1::digest(password.as_bytes())
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect();
    let (prefix, suffix) = hex.split_at(5);
    let Ok(response) = http
        .get(format!("https://api.pwnedpasswords.com/range/{prefix}"))
        .header("Add-Padding", "true")
        .send()
        .await
    else {
        tracing::warn!("HIBP unreachable: password accepted without breach check");
        return false;
    };
    let body = response.text().await.unwrap_or_default();
    body.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(s, count)| s.eq_ignore_ascii_case(suffix) && count.trim() != "0")
    })
}
