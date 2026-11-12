//! "Continue with Google / Apple" endpoints and linked sign-in methods.
//!
//! The browser goes to `GET /v1/account/oauth/{provider}/start` (full-page navigation) and comes
//! back to `/callback` (GET for Google, form POST for Apple); the API sets the session cookies
//! and redirects to the site (`/account/signin/complete`, or `/account/signin?error=...`).

use axum::{
    Extension, Form, Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use axum_extra::extract::cookie::CookieJar;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::GuestUser,
    db::{identity::IdentityProvider, types::Locale},
    error::{AppError, AppResult},
    services::{
        accounts::{
            oidc::{self, PendingAuth},
            sessions,
            tokens::random_token,
        },
        audit::{self, Actor},
    },
    state::AppState,
    web::client::ClientInfo,
};

fn provider(raw: &str) -> AppResult<IdentityProvider> {
    match raw {
        "google" => Ok(IdentityProvider::Google),
        "apple" => Ok(IdentityProvider::Apple),
        _ => Err(AppError::NotFound),
    }
}

/// A relative path on the site, or nothing (no open redirects).
pub fn safe_return_to(raw: Option<&str>) -> Option<String> {
    raw.filter(|r| {
        r.starts_with('/') && !r.starts_with("//") && !r.contains('\\') && r.len() <= 500
    })
    .map(str::to_string)
}

fn site_page(state: &AppState, locale: &str, path: &str) -> String {
    let origin = state.config.booking.webauthn_origin.trim_end_matches('/');
    if locale == "fr" {
        format!("{origin}{path}")
    } else {
        format!("{origin}/{locale}{path}")
    }
}

pub async fn providers(State(state): State<AppState>) -> Json<Value> {
    let b = &state.config.booking;
    Json(json!({
        "google": oidc::enabled(b, IdentityProvider::Google),
        "apple": oidc::enabled(b, IdentityProvider::Apple),
    }))
}

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    #[serde(default)]
    pub return_to: Option<String>,
    #[serde(default)]
    pub shared_device: bool,
    #[serde(default)]
    pub locale: Option<String>,
    /// `link`: add this provider to the signed-in account.
    #[serde(default)]
    pub intent: Option<String>,
}

pub async fn start(
    State(state): State<AppState>,
    Path(raw): Path<String>,
    Query(q): Query<StartQuery>,
    guest: Option<Extension<GuestUser>>,
) -> AppResult<Redirect> {
    let provider = provider(&raw)?;
    if !oidc::enabled(&state.config.booking, provider) {
        return Err(AppError::NotFound);
    }
    let link_user = match q.intent.as_deref() {
        Some("link") => {
            let g = guest.ok_or(AppError::Unauthorized)?;
            sessions::require_recent_auth(&state.db, g.session_id, Utc::now()).await?;
            Some(g.user_id)
        }
        _ => None,
    };
    let locale = q
        .locale
        .filter(|l| ["fr", "en", "ar"].contains(&l.as_str()))
        .unwrap_or_else(|| "fr".into());
    let url = oidc::start(
        &state.cache,
        &state.config.booking,
        PendingAuth {
            provider,
            verifier: random_token() + &random_token(),
            nonce: random_token(),
            return_to: safe_return_to(q.return_to.as_deref()),
            persistent: !q.shared_device,
            locale,
            link_user,
        },
    )
    .await?;
    Ok(Redirect::to(&url))
}

#[derive(Debug, Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

pub async fn callback_get(
    State(state): State<AppState>,
    jar: CookieJar,
    client: ClientInfo,
    Path(raw): Path<String>,
    Query(p): Query<CallbackParams>,
) -> Response {
    complete(state, jar, client, raw, p).await
}

pub async fn callback_post(
    State(state): State<AppState>,
    jar: CookieJar,
    client: ClientInfo,
    Path(raw): Path<String>,
    Form(p): Form<CallbackParams>,
) -> Response {
    complete(state, jar, client, raw, p).await
}

async fn complete(
    state: AppState,
    jar: CookieJar,
    client: ClientInfo,
    raw: String,
    p: CallbackParams,
) -> Response {
    let Some(state_param) = p.state.as_deref() else {
        return Redirect::to(&site_page(
            &state,
            "fr",
            "/account/signin?error=oauth_expired",
        ))
        .into_response();
    };
    let pending = match oidc::take(&state.cache, state_param).await {
        Ok(p) => p,
        Err(_) => {
            return Redirect::to(&site_page(
                &state,
                "fr",
                "/account/signin?error=oauth_expired",
            ))
            .into_response();
        }
    };
    let locale = pending.locale.clone();
    let fail = |code: &str| {
        let back = if pending.link_user.is_some() {
            "/account/security"
        } else {
            "/account/signin"
        };
        Redirect::to(&site_page(&state, &locale, &format!("{back}?error={code}"))).into_response()
    };
    if provider(&raw).ok() != Some(pending.provider) {
        return fail("oauth_invalid");
    }
    if p.error.is_some() {
        return fail("oauth_cancelled");
    }
    let Some(code) = p.code.as_deref() else {
        return fail("oauth_invalid");
    };
    let now = Utc::now();
    let result = async {
        let identity = oidc::exchange(
            &state.http,
            &state.cache,
            &state.config.booking,
            &pending,
            code,
            now,
        )
        .await?;
        let actor = client.actor(Actor::customer());
        let signed = oidc::resolve(
            &state.db,
            &identity,
            pending.link_user,
            &Locale::new(&pending.locale),
            &actor,
            now,
        )
        .await?;
        if pending.link_user.is_some() {
            return Ok::<_, AppError>((jar.clone(), None));
        }
        let (jar, body) = super::auth::finish(
            &state,
            jar.clone(),
            &client,
            signed,
            pending.persistent,
            pending.provider.as_str(),
        )
        .await?;
        Ok((jar, Some(body.0)))
    }
    .await;
    match result {
        Ok((jar, None)) => (
            jar,
            Redirect::to(&site_page(&state, &locale, "/account/security?linked=1")),
        )
            .into_response(),
        Ok((jar, Some(body))) => {
            let mut target = format!(
                "/account/signin/complete?new={}",
                body["needs_profile"].as_bool().unwrap_or(false)
            );
            if let Some(r) = pending.return_to.as_deref() {
                target.push_str("&return_to=");
                target.push_str(&url_encode(r));
            }
            (jar, Redirect::to(&site_page(&state, &locale, &target))).into_response()
        }
        Err(AppError::Domain { code, .. }) => fail(code),
        Err(e) => {
            tracing::warn!(error = %e, "oauth sign-in failed");
            fail("oauth_failed")
        }
    }
}

fn url_encode(s: &str) -> String {
    reqwest::Url::parse_with_params("x:", &[("v", s)])
        .ok()
        .and_then(|u| u.query().map(|q| q.trim_start_matches("v=").to_string()))
        .unwrap_or_default()
}

/// Unlinks a provider. Email codes always work, so the account keeps a way in.
pub async fn unlink(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Path(raw): Path<String>,
) -> AppResult<StatusCode> {
    let provider = provider(&raw)?;
    sessions::require_recent_auth(&state.db, guest.session_id, Utc::now()).await?;
    let deleted = sqlx::query!(
        "DELETE FROM user_identities WHERE user_id = $1 AND provider = $2",
        guest.user_id,
        provider.as_str()
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "account.identity_unlinked",
        "user",
        &guest.user_id.to_string(),
        json!({ "provider": provider.as_str() }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
