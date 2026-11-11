//! Keeps Redis read caches consistent: content changes publish an outbox event, this handler
//! deletes the cache entries built from that content.
//!
//! Cache keys (all under the `mouzoun:` namespace). Read paths that cache MUST use these
//! prefixes, so invalidation reaches them:
//! - `catalog:` — anything built from products (lists, product pages, prices);
//! - `settings:` — public settings;
//! - `menus:` — navigation menus;
//! - `pages:` — CMS pages.

use async_trait::async_trait;

use crate::{
    db::operations::OutboxEvent,
    infra::cache::Cache,
    services::outbox::types,
    worker::{HandlerError, OutboxHandler},
};

pub const CATALOG_PREFIX: &str = "catalog:";
pub const SETTINGS_PREFIX: &str = "settings:";
pub const MENUS_PREFIX: &str = "menus:";
pub const PAGES_PREFIX: &str = "pages:";

pub struct CacheInvalidationHandler {
    cache: Cache,
}

impl CacheInvalidationHandler {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }
}

/// Cache prefix invalidated by an event type.
pub fn prefix_for(event_type: &str) -> Option<&'static str> {
    match event_type {
        types::PRODUCT_UPDATED => Some(CATALOG_PREFIX),
        types::SETTINGS_UPDATED => Some(SETTINGS_PREFIX),
        types::MENU_UPDATED => Some(MENUS_PREFIX),
        types::PAGE_UPDATED => Some(PAGES_PREFIX),
        _ => None,
    }
}

#[async_trait]
impl OutboxHandler for CacheInvalidationHandler {
    fn name(&self) -> &'static str {
        "cache_invalidation"
    }

    fn event_types(&self) -> &'static [&'static str] {
        &[
            types::PRODUCT_UPDATED,
            types::SETTINGS_UPDATED,
            types::MENU_UPDATED,
            types::PAGE_UPDATED,
        ]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let Some(prefix) = prefix_for(&event.event_type) else {
            return Ok(());
        };
        let deleted = self
            .cache
            .delete_prefix(prefix)
            .await
            .map_err(|e| HandlerError::Retry(anyhow::anyhow!("redis: {e}")))?;
        tracing::debug!(event_id = event.id, prefix, deleted, "cache invalidated");
        Ok(())
    }
}
