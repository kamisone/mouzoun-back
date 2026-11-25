//! Invalidation of rendered pages (`pages:{page_id}:{revision}:{locale}`) and of the photo pages
//! of their gallery sections (`gallery:{block_id}:{revision}:...`).
//!
//! - publishing changes the revision, hence the key: nothing to delete;
//! - `product.updated` / `content.changed` delete the pages whose published revision references
//!   that entity (`snapshot.refs`) or resolves that kind of data by rule (`snapshot.rule_sources`,
//!   e.g. "latest activities");
//! - `settings.updated` deletes every page (contact details, opening date, camp location);
//! - shop sections (`rule_sources: shop_catalog`) follow any shop product: `product.updated` and
//!   `variant.updated` of a shop product, `promotion.updated` of a shop (or all-scope) promotion,
//!   and `stock.changed` only when the product's availability status changed from the one last
//!   shown (memo `shop_shown:{product}`, written by the section resolver);
//! - a `content.changed` of a media also reaches the pages showing a gallery that contains it.
//!
//! Entries also expire on their own at the next schedule boundary of the page (≤ 1 hour).

use async_trait::async_trait;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    db::operations::OutboxEvent,
    infra::cache::Cache,
    services::{cache_invalidation::PAGES_PREFIX, outbox::types, shop::events::types as shop},
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

impl PageCacheHandler {
    /// Pages with a shop section a shop event reaches (`None`: not a shop event).
    async fn shop_pages(&self, event: &OutboxEvent) -> anyhow::Result<Option<Vec<Uuid>>> {
        let id = |k: &str| {
            event.payload[k]
                .as_str()
                .and_then(|s| s.parse::<Uuid>().ok())
        };
        let shop_rule = || affected_pages(&self.db, "shop_catalog", Uuid::nil(), "shop_catalog");
        match event.event_type.as_str() {
            types::PRODUCT_UPDATED | shop::VARIANT_UPDATED => {
                let Some(product) = id("product_id") else {
                    return Ok(None);
                };
                let kind: Option<String> =
                    sqlx::query_scalar!("SELECT kind FROM products WHERE id = $1", product)
                        .fetch_optional(&self.db)
                        .await?;
                if kind.as_deref() != Some("shop_product") {
                    return Ok(None);
                }
                let mut pages = shop_rule().await?;
                pages.extend(affected_pages(&self.db, "product", product, "shop_catalog").await?);
                Ok(Some(pages))
            }
            shop::STOCK_CHANGED => {
                let Some(product) = id("product_id") else {
                    return Ok(Some(vec![]));
                };
                let now =
                    crate::services::shop::catalog::availability_status(&self.db, product).await?;
                let key = format!("{}{product}", super::public::SHOWN_AVAILABILITY_PREFIX);
                let shown = self
                    .cache
                    .get_json::<crate::services::shop::inventory::Availability>(&key)
                    .await;
                if shown.is_some() && shown == now {
                    return Ok(Some(vec![]));
                }
                if let Some(a) = now {
                    self.cache
                        .set_json(&key, &a, std::time::Duration::from_secs(30 * 24 * 3600))
                        .await;
                }
                Ok(Some(shop_rule().await?))
            }
            shop::PROMOTION_UPDATED => {
                let Some(promotion) = id("promotion_id") else {
                    return Ok(Some(vec![]));
                };
                let scope: Option<String> =
                    sqlx::query_scalar!("SELECT scope FROM promotions WHERE id = $1", promotion)
                        .fetch_optional(&self.db)
                        .await?;
                Ok(Some(match scope.as_deref() {
                    Some("shop" | "all") | None => shop_rule().await?,
                    _ => vec![],
                }))
            }
            _ => Ok(None),
        }
    }
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
            shop::VARIANT_UPDATED,
            shop::STOCK_CHANGED,
            shop::PROMOTION_UPDATED,
        ]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let retry = |e: &dyn std::fmt::Display| HandlerError::Retry(anyhow::anyhow!("{e}"));
        if let Some(pages) = self.shop_pages(event).await.map_err(|e| retry(&e))? {
            for page in pages {
                invalidate_page(&self.cache, page)
                    .await
                    .map_err(|e| retry(&e))?;
            }
            if event.event_type != types::PRODUCT_UPDATED {
                return Ok(());
            }
        }
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
        let mut pages = affected_pages(&self.db, &ref_type, id, rule_kind)
            .await
            .map_err(|e| retry(&e))?;
        // A photo shown inside a gallery section is reached through its galleries.
        if ref_type == "media" {
            let galleries = sqlx::query_scalar!(
                "SELECT gallery_id FROM gallery_items WHERE media_id = $1",
                id
            )
            .fetch_all(&self.db)
            .await
            .map_err(|e| retry(&e))?;
            for g in galleries {
                pages.extend(
                    affected_pages(&self.db, "gallery", g, "gallery")
                        .await
                        .map_err(|e| retry(&e))?,
                );
            }
        }
        pages.sort_unstable();
        pages.dedup();
        for page in pages {
            let deleted = invalidate_page(&self.cache, page)
                .await
                .map_err(|e| retry(&e))?;
            let photos = super::gallery::invalidate_page_galleries(&self.db, &self.cache, page)
                .await
                .map_err(|e| retry(&e))?;
            tracing::debug!(event_id = event.id, %page, deleted, photos, "page cache invalidated");
        }
        Ok(())
    }
}
