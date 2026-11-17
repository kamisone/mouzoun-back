//! `POST /v1/admin/translations/generate` (staff): the "Générer" buttons of the back office.
//! The admin writes the source language (default: the site's default locale, French); the
//! response holds every other active locale, or a reason per locale that failed.

use std::{collections::BTreeMap, time::Duration};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde::Deserialize;

use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::translate::Outcome,
    state::AppState,
    web::limits::rate_limit,
};

pub fn admin_routes() -> Router<AppState> {
    Router::new().route("/translations/generate", post(generate))
}

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    /// Source locale; the default locale when absent.
    pub source_locale: Option<String>,
    /// Field key → source text (at most 30 fields, 20,000 characters in all).
    pub fields: BTreeMap<String, String>,
    /// Keys holding HTML (translated block by block, inline tags kept).
    #[serde(default)]
    pub html: Vec<String>,
    /// Target locales; every other active locale when absent.
    pub targets: Option<Vec<String>>,
}

pub async fn generate(
    State(state): State<AppState>,
    staff: Staff,
    Json(b): Json<GenerateBody>,
) -> AppResult<Json<Outcome>> {
    if !state.translator.enabled() {
        return Err(AppError::domain(
            StatusCode::SERVICE_UNAVAILABLE,
            "translation_disabled",
            "machine translation is turned off (TRANSLATE_ENABLED=false)",
        ));
    }
    let size: usize = b.fields.values().map(|v| v.chars().count()).sum();
    if b.fields.is_empty() || b.fields.len() > 30 || size > 20_000 {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "1 to 30 fields, 20,000 characters at most",
        ));
    }
    if b.fields.values().all(|v| v.trim().is_empty()) {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "source_empty",
            "write the source text first",
        ));
    }
    // The free endpoint rate-limits: keep each staff member to a reasonable pace.
    rate_limit(
        &state.cache,
        "translate",
        &staff.id.to_string(),
        30,
        Duration::from_secs(60),
    )
    .await?;
    let locales = sqlx::query!(
        "SELECT code, is_default FROM locales WHERE is_active ORDER BY sort_order, code"
    )
    .fetch_all(&state.db)
    .await?;
    let source = b
        .source_locale
        .clone()
        .or_else(|| {
            locales
                .iter()
                .find(|l| l.is_default)
                .map(|l| l.code.clone())
        })
        .unwrap_or_else(|| "fr".into());
    if !locales.iter().any(|l| l.code == source) {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "unknown source locale",
        ));
    }
    let targets: Vec<String> = locales
        .iter()
        .map(|l| l.code.clone())
        .filter(|c| *c != source)
        .filter(|c| b.targets.as_ref().is_none_or(|t| t.contains(c)))
        .collect();
    let outcome = state
        .translator
        .fields(&b.fields, &b.html, &source, &targets)
        .await;
    Ok(Json(outcome))
}
