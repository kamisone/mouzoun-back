//! Who is calling: client IP (behind our nginx), user agent, request id, and the language of
//! the response (`?locale=`, then `Accept-Language`, among the active locales).

use std::net::IpAddr;

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::{header, request::Parts},
};
use ipnetwork::IpNetwork;

use crate::{
    db::types::Locale,
    error::AppError,
    services::{audit::Actor, locale::resolve_locale},
    state::AppState,
};

#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
    pub request_id: Option<String>,
}

impl ClientInfo {
    pub fn ip_network(&self) -> Option<IpNetwork> {
        self.ip.map(IpNetwork::from)
    }

    /// Key for per-client rate limits.
    pub fn key(&self) -> String {
        self.ip
            .map_or_else(|| "unknown".into(), |ip| ip.to_string())
    }

    pub fn actor(&self, base: Actor) -> Actor {
        base.with_request(
            self.ip_network(),
            self.user_agent.clone(),
            self.request_id.clone(),
        )
    }
}

impl FromRequestParts<AppState> for ClientInfo {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        // Our nginx appends the peer address to X-Forwarded-For: the last entry is the one we
        // can trust (earlier ones are whatever the client sent).
        let forwarded = state
            .config
            .booking
            .trust_proxy
            .then(|| header("x-forwarded-for"))
            .flatten()
            .and_then(|v| v.rsplit(',').next().and_then(|ip| ip.trim().parse().ok()));
        let peer = parts
            .extensions
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip());
        Ok(Self {
            ip: forwarded.or(peer),
            user_agent: header(header::USER_AGENT.as_str())
                .map(|ua| ua.chars().take(400).collect()),
            request_id: header("x-request-id"),
        })
    }
}

/// The language of the response.
#[derive(Debug, Clone)]
pub struct RequestLocale(pub Locale);

impl FromRequestParts<AppState> for RequestLocale {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let query = parts.uri.query().and_then(|q| {
            q.split('&')
                .find_map(|kv| kv.strip_prefix("locale="))
                .map(str::to_string)
        });
        let accept = parts
            .headers
            .get(header::ACCEPT_LANGUAGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let rows = sqlx::query!(
            "SELECT code, is_default FROM locales WHERE is_active ORDER BY sort_order"
        )
        .fetch_all(&state.db)
        .await?;
        let available: Vec<Locale> = rows.iter().map(|r| Locale::new(r.code.clone())).collect();
        let default = rows
            .iter()
            .find(|r| r.is_default)
            .map(|r| Locale::new(r.code.clone()))
            .or_else(|| available.first().cloned())
            .unwrap_or_else(|| Locale::new("fr"));
        Ok(Self(resolve_locale(
            query.as_deref(),
            accept.as_deref(),
            &available,
            &default,
        )))
    }
}
