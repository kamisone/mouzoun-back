//! SEO: the route registry (every public URL, one indexability rule), site-wide redirects (auto
//! and manual, served by the Nuxt server from a cached map), the 404 log, sitemaps and the head
//! tags. See back/docs/seo/README.md.
//!
//! | area | read | write |
//! |---|---|---|
//! | redirects, 404 log, sitemap, URL tester (`/v1/admin/seo`) | seo.manage or content.read | seo.manage |
//! | map, hits (`/v1/seo`, hits need the internal token), sitemaps, head tags | public | — |

pub mod handlers;
pub mod hits;
pub mod path;
pub mod redirects;
pub mod routes;
pub mod sitemap;

use async_trait::async_trait;
use axum::{
    Router,
    routing::{get, patch, post},
};
use sqlx::PgPool;

use crate::{
    db::operations::OutboxEvent,
    infra::cache::Cache,
    services::outbox::types,
    state::AppState,
    worker::{HandlerError, OutboxHandler},
};

/// Redis keys (in the cache namespace).
pub const REDIRECTS_VERSION: &str = "seo:redirects:version";
pub const REDIRECTS_MAP: &str = "seo:redirects";
pub const HEAD_CACHE_PREFIX: &str = "seo:head:";

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/seo/redirects/version", get(handlers::redirects_version))
        .route("/seo/redirects/map", get(handlers::redirects_map))
        .route("/seo/hits", post(handlers::hits))
        .route("/seo/sitemap.xml", get(handlers::sitemap_index))
        .route("/seo/sitemaps/{file}", get(handlers::sitemap_file))
        .route("/seo/head", get(handlers::head))
        .route("/seo/robots", get(handlers::robots))
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/seo/redirects",
            get(handlers::admin_redirects).post(handlers::admin_create_redirect),
        )
        .route("/seo/redirects/bulk", post(handlers::admin_bulk_redirects))
        .route(
            "/seo/redirects/{id}",
            patch(handlers::admin_update_redirect).delete(handlers::admin_delete_redirect),
        )
        .route(
            "/seo/not-found",
            get(handlers::admin_not_found).delete(handlers::admin_clear_not_found),
        )
        .route(
            "/seo/not-found/{*path}",
            patch(handlers::admin_ignore_not_found),
        )
        .route("/seo/test", get(handlers::admin_test))
        .route("/seo/live-paths", get(handlers::admin_live_paths))
        .route("/seo/sitemap/status", get(handlers::admin_sitemap_status))
        .route(
            "/seo/sitemap/rebuild",
            post(handlers::admin_sitemap_rebuild),
        )
}

/// Keeps the SEO caches in step with the content: `seo.changed` bumps the redirect map version
/// and drops the listed sitemap files; content events drop the sitemap file of their type.
pub struct SeoCacheHandler {
    pool: PgPool,
    cache: Cache,
}

impl SeoCacheHandler {
    pub fn new(pool: PgPool, cache: Cache) -> Self {
        Self { pool, cache }
    }

    /// Sitemap files a content event touches (empty = all).
    async fn files_for(&self, event: &OutboxEvent) -> Result<Option<Vec<String>>, HandlerError> {
        let p = &event.payload;
        let id = |k: &str| {
            p.get(k)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<uuid::Uuid>().ok())
        };
        let one = |s: &str| Some(vec![s.to_string()]);
        Ok(match event.event_type.as_str() {
            types::SEO_CHANGED => Some(
                p.get("sitemaps")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            types::PRODUCT_UPDATED => {
                let kind = match id("product_id") {
                    Some(pid) => {
                        sqlx::query_scalar!("SELECT kind FROM products WHERE id = $1", pid)
                            .fetch_optional(&self.pool)
                            .await
                            .map_err(|e| HandlerError::Retry(e.into()))?
                    }
                    None => None,
                };
                match kind.as_deref() {
                    Some("accommodation") => one("accommodations"),
                    Some("activity") => one("activities"),
                    Some("package") => one("packages"),
                    // A shop product can make a category (non-)indexable.
                    Some("shop_product") => {
                        Some(vec!["shop-products".into(), "shop-categories".into()])
                    }
                    Some(_) => Some(vec![]),
                    None => None,
                }
            }
            types::PAGE_UPDATED => {
                let kind = match id("page_id") {
                    Some(pid) => {
                        sqlx::query_scalar!("SELECT page_type FROM pages WHERE id = $1", pid)
                            .fetch_optional(&self.pool)
                            .await
                            .map_err(|e| HandlerError::Retry(e.into()))?
                    }
                    None => None,
                };
                match kind.as_deref() {
                    Some("post") => one("posts"),
                    Some(_) => one("pages"),
                    None => None,
                }
            }
            types::CONTENT_CHANGED => match p.get("ref_type").and_then(|v| v.as_str()) {
                Some("category") => one("shop-categories"),
                _ => Some(vec![]),
            },
            // Locales, shop status: everything.
            types::SETTINGS_UPDATED => None,
            _ => Some(vec![]),
        })
    }
}

#[async_trait]
impl OutboxHandler for SeoCacheHandler {
    fn name(&self) -> &'static str {
        "seo_cache"
    }

    fn event_types(&self) -> &'static [&'static str] {
        &[
            types::SEO_CHANGED,
            types::PRODUCT_UPDATED,
            types::PAGE_UPDATED,
            types::CONTENT_CHANGED,
            types::SETTINGS_UPDATED,
        ]
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let retry = |e: redis::RedisError| HandlerError::Retry(anyhow::anyhow!("redis: {e}"));
        if event.event_type == types::SEO_CHANGED
            && event.payload.get("redirects").and_then(|v| v.as_bool()) == Some(true)
        {
            hits::bump_version(&self.cache).await.map_err(retry)?;
        }
        match self.files_for(event).await? {
            // Nothing that a sitemap shows.
            Some(files) if files.is_empty() && event.event_type != types::SETTINGS_UPDATED => {}
            Some(files) => sitemap::invalidate(&self.cache, &files)
                .await
                .map_err(retry)?,
            None => sitemap::invalidate(&self.cache, &[]).await.map_err(retry)?,
        }
        Ok(())
    }
}
