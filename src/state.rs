use std::sync::Arc;

use axum::extract::FromRef;
use sqlx::PgPool;

use crate::{
    config::Config,
    infra::{cache::Cache, storage::Buckets},
    modules::media::MediaService,
};

/// Shared application state. Cheap to clone: everything inside is pooled or `Arc`ed.
/// Services are built once here (composition root) and extracted by handlers via `FromRef`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: PgPool,
    pub cache: Cache,
    pub media: MediaService,
}

impl AppState {
    pub fn new(config: Config, db: PgPool, cache: Cache, buckets: Buckets) -> Self {
        let config = Arc::new(config);
        let media = MediaService::new(db.clone(), buckets, cache.clone(), config.media.clone());
        Self {
            config,
            db,
            cache,
            media,
        }
    }
}

impl FromRef<AppState> for MediaService {
    fn from_ref(state: &AppState) -> Self {
        state.media.clone()
    }
}
