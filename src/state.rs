use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::PgPool;

use crate::{
    config::Config,
    infra::{cache::Cache, clock::Clock, storage::SharedStorage},
    modules::{media::MediaService, pages::SectionRegistry},
    services::{payments::Payments, shop::payments::ShopPayments},
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
    /// Shop payment providers (Stripe Checkout, the fake one in development).
    pub shop_payments: ShopPayments,
    /// Outgoing HTTP (payment providers, Turnstile).
    pub http: reqwest::Client,
    /// Last object-storage probe of the readiness endpoint (at most one call every 5 minutes).
    pub storage_probe: Arc<tokio::sync::Mutex<Option<(std::time::Instant, bool)>>>,
    /// Page builder section types (compiled once, from `section_types/*.json`).
    pub sections: Arc<SectionRegistry>,
    /// "Now" for time-dependent content (tests pin it).
    pub clock: Clock,
    /// Machine translation of admin content ("Générer" buttons).
    pub translator: crate::services::translate::Translator,
}

impl AppState {
    pub fn new(config: Config, db: PgPool, cache: Cache, buckets: SharedStorage) -> Self {
        let config = Arc::new(config);
        let media = MediaService::new(db.clone(), buckets, config.media.clone());
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .connect_timeout(std::time::Duration::from_secs(5))
            .user_agent("mouzoun-api")
            .build()
            .expect("HTTP client");
        let payments = Payments::from_config(&config.booking, http.clone());
        let shop_payments = ShopPayments::from_config(&config, http.clone());
        let translator =
            crate::services::translate::Translator::new(http.clone(), config.translate.clone());
        Self {
            config,
            db,
            cache,
            media,
            payments,
            shop_payments,
            translator,
            http,
            storage_probe: Arc::default(),
            sections: Arc::new(SectionRegistry::load()),
            clock: Clock::default(),
        }
    }
}

impl FromRef<AppState> for MediaService {
    fn from_ref(state: &AppState) -> Self {
        state.media.clone()
    }
}
