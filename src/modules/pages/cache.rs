//! Invalidation of rendered pages (`pages:{page_id}:{revision}:{locale}`).
//!
//! - publishing changes the revision, hence the key: nothing to delete;
//! - `product.updated` / `content.changed` delete the pages whose published revision references
//!   that entity (`snapshot.refs`) or resolves that kind of data by rule (`snapshot.rule_sources`,
//!   e.g. "latest activities");
//! - `settings.updated` deletes every page (contact details, opening date, camp location).
//!
//! Entries also expire on their own at the next schedule boundary of the page (≤ 1 hour).

use async_trait::async_trait;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    db::operations::OutboxEvent,
    infra::cache::Cache,
    services::{cache_invalidation::PAGES_PREFIX, outbox::types},
    worker::{HandlerError, OutboxHandler},
};

pub struct PageCacheHandler {
    db: PgPool,
    cache: Cache,
}

impl PageCacheHandler {
    pub fn new(db: PgPool, cache: Cache) -> Self {
        Self { db, cache }
    }
}

/// Published pages affected by a change of `(ref_type, id)`; `rule_kind` is the data kind
/// that rule-based sections resolve live.
pub async fn affected_pages(
    db: &PgPool,
    ref_type: &str,
    id: Uuid,
    rule_kind: &str,
) -> sqlx::Result<Vec<Uuid>> {
    let needle = json!([{ "ref_type": ref_type, "ref_id": id }]);
    sqlx::query_scalar!(
        "SELECT page_id FROM page_revisions
         WHERE status = 'published' AND (snapshot -> 'refs' @> $1 OR snapshot -> 'rule_sources' ? $2)",
        needle,
        rule_kind
    )
    .fetch_all(db)
    .await
}

pub async fn invalidate_page(cache: &Cache, page_id: Uuid) -> redis::RedisResult<usize> {
    cache
        .delete_prefix(&format!("{PAGES_PREFIX}{page_id}:"))
        .await
}

#[async_trait]
impl OutboxHandler for PageCacheHandler {
    fn name(&self) -> &'static str {
        "page_cache"
    }

    fn event_types(&self) -> &'static [&'static str] {
        &[
            types::PRODUCT_UPDATED,
            types::CONTENT_CHANGED,
            types::SETTINGS_UPDATED,
        ]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let retry = |e: &dyn std::fmt::Display| HandlerError::Retry(anyhow::anyhow!("{e}"));
        let target = match event.event_type.as_str() {
            types::SETTINGS_UPDATED => {
                self.cache
                    .delete_prefix(PAGES_PREFIX)
                    .await
                    .map_err(|e| retry(&e))?;
                return Ok(());
            }
            types::PRODUCT_UPDATED => event.payload["product_id"]
                .as_str()
                .map(|id| ("product".to_string(), id.to_string())),
            _ => event.payload["ref_type"]
                .as_str()
                .zip(event.payload["ref_id"].as_str())
                .map(|(t, id)| (t.to_string(), id.to_string())),
        };
        let Some((ref_type, id)) = target else {
            return Err(HandlerError::Permanent(anyhow::anyhow!(
                "event without a target"
            )));
        };
        let id: Uuid = id
            .parse()
            .map_err(|e| HandlerError::Permanent(anyhow::anyhow!("bad id: {e}")))?;
        // Promotions change offers listed by rule; products change listings by rule.
        let rule_kind = match ref_type.as_str() {
            "farm_crop" => "crop",
            other => other,
        };
        for page in affected_pages(&self.db, &ref_type, id, rule_kind)
            .await
            .map_err(|e| retry(&e))?
        {
            let deleted = invalidate_page(&self.cache, page)
                .await
                .map_err(|e| retry(&e))?;
            tracing::debug!(event_id = event.id, %page, deleted, "page cache invalidated");
        }
        Ok(())
    }
}
