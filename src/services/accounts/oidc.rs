//! "Continue with Google / Apple": OpenID Connect authorization code flow with PKCE.
//!
//! - `start`: random `state`, `nonce` and PKCE verifier kept in Redis for 10 minutes (single
//!   use), then a redirect to the provider;
//! - `callback`: the state is taken (GETDEL), the code exchanged with the verifier, the ID token
//!   checked against the provider's JWKS (signature, issuer, audience, expiry, nonce);
//! - `resolve`: a known (provider, subject) signs in its user; otherwise the identity is linked
//!   to the account of its email **only when the provider says that email is verified**, or to
//!   the signed-in user who started a "link" flow.

use std::time::Duration as StdDuration;

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, jwk::JwkSet};
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    signin::{self, SignedIn},
    tokens::{normalize_email, random_token, sha256},
};
use crate::{
    config::{AppleClient, BookingConfig, OidcClient},
    db::{identity::IdentityProvider, types::Locale},
    error::{AppError, AppResult},
    infra::cache::Cache,
    services::{
        audit::{self, Actor},
        outbox::{self, Event},
    },
};

const STATE_TTL_SECS: u64 = 600;
const JWKS_TTL: StdDuration = StdDuration::from_secs(3600);

pub struct ProviderInfo {
    pub authorize_url: &'static str,
    pub token_url: &'static str,
    pub jwks_url: &'static str,
    pub issuer: &'static str,
    pub scope: &'static str,
}

pub fn info(provider: IdentityProvider) -> Option<ProviderInfo> {
    match provider {
        IdentityProvider::Google => Some(ProviderInfo {
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
            token_url: "https://oauth2.googleapis.com/token",
            jwks_url: "https://www.googleapis.com/oauth2/v3/certs",
            issuer: "https://accounts.google.com",
            scope: "openid email profile",
        }),
        IdentityProvider::Apple => Some(ProviderInfo {
            authorize_url: "https://appleid.apple.com/auth/authorize",
            token_url: "https://appleid.apple.com/auth/token",
            jwks_url: "https://appleid.apple.com/auth/keys",
            issuer: "https://appleid.apple.com",
            scope: "name email",
        }),
        IdentityProvider::Facebook => None,
    }
}

/// Whether a provider is configured (buttons are hidden otherwise).
pub fn enabled(config: &BookingConfig, provider: IdentityProvider) -> bool {
    match provider {
        IdentityProvider::Google => config.google.is_some(),
        IdentityProvider::Apple => config.apple.is_some(),
        IdentityProvider::Facebook => false,
    }
}

pub fn redirect_uri(config: &BookingConfig, provider: IdentityProvider) -> String {
    format!(
        "{}/v1/account/oauth/{}/callback",
        config.api_public_url.trim_end_matches('/'),
        provider.as_str()
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAuth {
    pub provider: IdentityProvider,
    pub verifier: String,
    pub nonce: String,
    /// Relative path to go back to.
    pub return_to: Option<String>,
    pub persistent: bool,
    pub locale: String,
    /// Set when a signed-in user links a new provider from "Login & security".
    pub link_user: Option<Uuid>,
}

fn state_key(cache: &Cache, state: &str) -> String {
    cache.key(&format!("oidc:{}", super::tokens::sha256_hex(state)))
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(sha256(verifier))
}

/// Stores the pending flow and returns the provider URL to redirect to.
pub async fn start(
    cache: &Cache,
    config: &BookingConfig,
    pending: PendingAuth,
) -> AppResult<String> {
    let provider = pending.provider;
    let info = info(provider).ok_or(AppError::NotFound)?;
    let client_id = match provider {
        IdentityProvider::Google => config.google.as_ref().map(|c| c.client_id.clone()),
        IdentityProvider::Apple => config.apple.as_ref().map(|c| c.client_id.clone()),
        IdentityProvider::Facebook => None,
    }
    .ok_or(AppError::NotFound)?;
    let state = random_token();
    let challenge = pkce_challenge(&pending.verifier);
    let nonce = pending.nonce.clone();
    let mut conn = cache.connection();
    let _: () = conn
        .set_ex(
            state_key(cache, &state),
            serde_json::to_string(&pending).unwrap_or_default(),
            STATE_TTL_SECS,
        )
        .await?;
    let mut url =
        reqwest::Url::parse(info.authorize_url).map_err(|e| AppError::Internal(e.into()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri(config, provider))
            .append_pair("response_type", "code")
            .append_pair("scope", info.scope)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        match provider {
            // Apple sends the name and email only with a form POST.
            IdentityProvider::Apple => {
                q.append_pair("response_mode", "form_post");
            }
            _ => {
                q.append_pair("prompt", "select_account");
            }
        }
    }
    Ok(url.to_string())
}

/// Takes (single use) the pending flow of a callback.
pub async fn take(cache: &Cache, state: &str) -> AppResult<PendingAuth> {
    let mut conn = cache.connection();
    let raw: Option<String> = redis::cmd("GETDEL")
        .arg(state_key(cache, state))
        .query_async(&mut conn)
        .await?;
    raw.and_then(|r| serde_json::from_str(&r).ok())
        .ok_or_else(|| {
            AppError::domain(
                StatusCode::GONE,
                "oauth_expired",
                "sign-in expired: try again",
            )
        })
}

/// Apple's client secret: a short ES256 JWT signed with the team's key.
pub fn apple_client_secret(apple: &AppleClient, now: DateTime<Utc>) -> AppResult<String> {
    #[derive(Serialize)]
    struct C<'a> {
        iss: &'a str,
        iat: i64,
        exp: i64,
        aud: &'a str,
        sub: &'a str,
    }
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(apple.key_id.clone());
    let key = EncodingKey::from_ec_pem(apple.private_key.as_bytes())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("APPLE_PRIVATE_KEY: {e}")))?;
    jsonwebtoken::encode(
        &header,
        &C {
            iss: &apple.team_id,
            iat: now.timestamp(),
            exp: now.timestamp() + 300,
            aud: "https://appleid.apple.com",
            sub: &apple.client_id,
        },
        &key,
    )
    .map_err(|e| AppError::Internal(e.into()))
}

/// What we keep from a verified ID token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIdentity {
    pub provider: IdentityProvider,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    sub: String,
    nonce: Option<String>,
    email: Option<String>,
    /// Google: bool; Apple: bool or "true".
    #[serde(default)]
    email_verified: Option<serde_json::Value>,
    given_name: Option<String>,
    family_name: Option<String>,
}

async fn jwks(state_http: &reqwest::Client, cache: &Cache, url: &str) -> AppResult<JwkSet> {
    let key = format!("oidc:jwks:{}", super::tokens::sha256_hex(url));
    if let Some(set) = cache.get_json::<JwkSet>(&key).await {
        return Ok(set);
    }
    let set: JwkSet = state_http
        .get(url)
        .timeout(StdDuration::from_secs(10))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| AppError::Internal(e.into()))?
        .json()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    cache.set_json(&key, &set, JWKS_TTL).await;
    Ok(set)
}

/// Checks an ID token against a key set (signature, issuer, audience, expiry, nonce).
pub fn verify_id_token(
    provider: IdentityProvider,
    id_token: &str,
    keys: &JwkSet,
    issuer: &str,
    audience: &str,
    nonce: &str,
) -> AppResult<VerifiedIdentity> {
    let bad = || {
        AppError::domain(
            StatusCode::UNAUTHORIZED,
            "oauth_invalid",
            "sign-in could not be verified",
        )
    };
    let header = jsonwebtoken::decode_header(id_token).map_err(|_| bad())?;
    let jwk = header
        .kid
        .as_deref()
        .and_then(|kid| keys.find(kid))
        .ok_or_else(bad)?;
    let key = DecodingKey::from_jwk(jwk).map_err(|_| bad())?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer, issuer.trim_start_matches("https://")]);
    validation.set_audience(&[audience]);
    validation.leeway = 60;
    let data = jsonwebtoken::decode::<IdClaims>(id_token, &key, &validation).map_err(|_| bad())?;
    let c = data.claims;
    if !c
        .nonce
        .as_deref()
        .is_some_and(|n| super::tokens::constant_time_eq(n.as_bytes(), nonce.as_bytes()))
    {
        return Err(bad());
    }
    let email_verified = match c.email_verified {
        Some(serde_json::Value::Bool(b)) => b,
        Some(serde_json::Value::String(s)) => s == "true",
        _ => false,
    };
    Ok(VerifiedIdentity {
        provider,
        subject: c.sub,
        email: c.email.map(|e| normalize_email(&e)),
        email_verified,
        given_name: c.given_name,
        family_name: c.family_name,
    })
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// Exchanges the authorization code and verifies the ID token it returns.
pub async fn exchange(
    http: &reqwest::Client,
    cache: &Cache,
    config: &BookingConfig,
    pending: &PendingAuth,
    code: &str,
    now: DateTime<Utc>,
) -> AppResult<VerifiedIdentity> {
    let provider = pending.provider;
    let info = info(provider).ok_or(AppError::NotFound)?;
    let (client_id, client_secret) = match provider {
        IdentityProvider::Google => {
            let OidcClient {
                client_id,
                client_secret,
            } = config.google.clone().ok_or(AppError::NotFound)?;
            (client_id, client_secret)
        }
        IdentityProvider::Apple => {
            let apple = config.apple.as_ref().ok_or(AppError::NotFound)?;
            (apple.client_id.clone(), apple_client_secret(apple, now)?)
        }
        IdentityProvider::Facebook => return Err(AppError::NotFound),
    };
    let redirect = redirect_uri(config, provider);
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect.as_str()),
        ("client_id", client_id.as_str()),
        ("client_secret", client_secret.as_str()),
        ("code_verifier", pending.verifier.as_str()),
    ];
    let response = http
        .post(info.token_url)
        .timeout(StdDuration::from_secs(10))
        .form(&form)
        .send()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if !response.status().is_success() {
        tracing::warn!(provider = provider.as_str(), status = %response.status(), "oauth code exchange refused");
        return Err(AppError::domain(
            StatusCode::UNAUTHORIZED,
            "oauth_invalid",
            "sign-in could not be verified",
        ));
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    let keys = jwks(http, cache, info.jwks_url).await?;
    verify_id_token(
        provider,
        &tokens.id_token,
        &keys,
        info.issuer,
        &client_id,
        &pending.nonce,
    )
}

/// Signs in (or links) a verified provider identity.
pub async fn resolve(
    pool: &PgPool,
    identity: &VerifiedIdentity,
    link_user: Option<Uuid>,
    locale: &Locale,
    actor: &Actor,
    now: DateTime<Utc>,
) -> AppResult<SignedIn> {
    let known = sqlx::query!(
        r#"SELECT i.user_id, u.status FROM user_identities i JOIN users u ON u.id = i.user_id
           WHERE i.provider = $1 AND i.provider_subject = $2 AND u.deleted_at IS NULL"#,
        identity.provider.as_str(),
        identity.subject
    )
    .fetch_optional(pool)
    .await?;
    if let Some(k) = known {
        if link_user.is_some_and(|u| u != k.user_id) {
            return Err(AppError::domain(
                StatusCode::CONFLICT,
                "identity_in_use",
                "this account is already linked to another Mouzoun account",
            ));
        }
        if k.status == "suspended" {
            return Err(AppError::domain(
                StatusCode::FORBIDDEN,
                "account_suspended",
                "this account is suspended: contact the camp",
            ));
        }
        sqlx::query!(
            "UPDATE user_identities SET last_used_at = $3, email = COALESCE($4::text::citext, email)
             WHERE provider = $1 AND provider_subject = $2",
            identity.provider.as_str(),
            identity.subject,
            now,
            identity.email
        )
        .execute(pool)
        .await?;
        return Ok(SignedIn {
            user_id: k.user_id,
            created: false,
            needs_profile: false,
        });
    }
    let signed = match link_user {
        Some(user_id) => SignedIn {
            user_id,
            created: false,
            needs_profile: false,
        },
        None => {
            // Never trust an address the provider has not verified: an attacker could otherwise
            // take over the account of that address.
            let email = identity
                .email
                .as_deref()
                .filter(|_| identity.email_verified)
                .ok_or_else(|| {
                    AppError::domain(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "email_unverified",
                        "this account has no verified email: sign in with your email instead",
                    )
                })?;
            signin::user_for_verified_email(pool, email, locale, actor, now).await?
        }
    };
    let mut tx = pool.begin().await?;
    sqlx::query!(
        "INSERT INTO user_identities (user_id, provider, provider_subject, email, email_verified, last_used_at)
         VALUES ($1, $2, $3, $4::text::citext, $5, $6)",
        signed.user_id,
        identity.provider.as_str(),
        identity.subject,
        identity.email,
        identity.email_verified,
        now
    )
    .execute(&mut *tx)
    .await?;
    // Names from the provider fill a new account.
    if let (Some(first), true) = (identity.given_name.as_deref(), signed.needs_profile) {
        sqlx::query!(
            "UPDATE customers SET first_name = $2, last_name = COALESCE(NULLIF(btrim(last_name), ''), $3)
             WHERE user_id = $1 AND btrim(first_name) = ''",
            signed.user_id,
            first,
            identity.family_name
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE users SET full_name = btrim($2 || ' ' || COALESCE($3, '')) WHERE id = $1 AND full_name = ''",
            signed.user_id,
            first,
            identity.family_name
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        actor,
        "account.identity_linked",
        "user",
        &signed.user_id.to_string(),
        serde_json::json!({ "provider": identity.provider.as_str() }),
    )
    .await?;
    if !signed.created {
        outbox::enqueue(
            &mut tx,
            &Event::AuthIdentityLinked {
                user_id: signed.user_id,
                provider: identity.provider.as_str().to_string(),
            },
        )
        .await?;
    }
    tx.commit().await?;
    let needs_profile = signed.needs_profile && identity.given_name.is_none();
    Ok(SignedIn {
        needs_profile,
        ..signed
    })
}
