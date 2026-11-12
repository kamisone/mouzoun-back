//! Keeps Redis read caches consistent: content changes publish an outbox event, this handler
//! deletes the cache entries built from that content.
//!
//! Cache keys (all under the `mouzoun:` namespace). Read paths that cache MUST use these
//! prefixes, so invalidation reaches them:
//! - `catalog:` — anything built from products (lists, product pages, prices);
//! - `settings:` — public settings;
//! - `menus:` — navigation menus;
//! - `pages:` — CMS pages;
//! - `availability:` — availability calendars and searches (every booking change).

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
pub const AVAILABILITY_PREFIX: &str = "availability:";

pub struct CacheInvalidationHandler {
    cache: Cache,
}

impl CacheInvalidationHandler {
    pub fn new(cache: Cache) -> Self {
        Self { cache }
    }
}

/// Cache prefixes invalidated by an event type.
pub fn prefixes_for(event_type: &str) -> &'static [&'static str] {
    match event_type {
        types::PRODUCT_UPDATED => &[CATALOG_PREFIX, AVAILABILITY_PREFIX],
        types::SETTINGS_UPDATED => &[SETTINGS_PREFIX, AVAILABILITY_PREFIX],
        types::MENU_UPDATED => &[MENUS_PREFIX],
        types::PAGE_UPDATED => &[PAGES_PREFIX],
        types::BOOKING_CREATED
        | types::BOOKING_CONFIRMED
        | types::BOOKING_MODIFIED
        | types::BOOKING_CANCELLED
        | types::BOOKING_EXPIRED
        | types::BOOKING_DECLINED => &[AVAILABILITY_PREFIX],
        _ => &[],
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
            types::BOOKING_CREATED,
            types::BOOKING_CONFIRMED,
            types::BOOKING_MODIFIED,
            types::BOOKING_CANCELLED,
            types::BOOKING_EXPIRED,
            types::BOOKING_DECLINED,
        ]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        for prefix in prefixes_for(&event.event_type) {
            let deleted = self
                .cache
                .delete_prefix(prefix)
                .await
                .map_err(|e| HandlerError::Retry(anyhow::anyhow!("redis: {e}")))?;
            tracing::debug!(event_id = event.id, prefix, deleted, "cache invalidated");
        }
        Ok(())
    }
}
