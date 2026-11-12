use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::PgPool;

use crate::{
    config::Config,
    infra::{cache::Cache, storage::Buckets},
    modules::media::MediaService,
    services::payments::Payments,
};

/// Shared application state. Cheap to clone: everything inside is pooled or `Arc`ed.
/// Services are built once here (composition root) and extracted by handlers via `FromRef`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub cache: Cache,
    pub media: MediaService,
    /// Online payment providers (Stripe, PayPal, the fake one in development).
    pub payments: Payments,
    /// Outgoing HTTP (payment providers, Turnstile).
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(config: Config, db: PgPool, cache: Cache, buckets: Buckets) -> Self {
        let config = Arc::new(config);
        let media = MediaService::new(db.clone(), buckets, cache.clone(), config.media.clone());
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .connect_timeout(std::time::Duration::from_secs(5))
            .user_agent("mouzoun-api")
            .build()
            .expect("HTTP client");
        let payments = Payments::from_config(&config.booking, http.clone());
        Self {
            config,
            db,
            cache,
            media,
            payments,
            http,
        }
    }
}

impl FromRef<AppState> for MediaService {
    fn from_ref(state: &AppState) -> Self {
        state.media.clone()
    }
}
