//! Route registry: the one place that knows every public URL of the site, which content serves
//! it, and whether that URL may be indexed. The sitemap, the `<head>` tags (canonical, hreflang,
//! robots), the redirect layer (live content wins, closest public parent) and the admin URL
//! tester all read it, so they never disagree.
//!
//! Paths follow the Nuxt routes (fixed in the code, not settings): no prefix for the default
//! language, `/en/...`, `/ar/...`; `/stays/{slug}`, `/activities/{slug}`, `/packages/{slug}`,
//! the shop under `/boutique` (`/en/shop`) with products at `/boutique/p/{slug}` and categories at
//! `/boutique/{slug}`, and builder pages at `/{slug}` (a slug may contain `/`: nested pages).
//! Extras, places, tags and other category scopes have no page.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::path::normalize;

pub const HOME: &str = "home";
pub const LISTING_STAYS: &str = "listing_stays";
pub const LISTING_ACTIVITIES: &str = "listing_activities";
pub const LISTING_SHOP: &str = "listing_shop";
pub const PAGE: &str = "page";
pub const ACCOMMODATION: &str = "accommodation";
pub const ACTIVITY: &str = "activity";
pub const PACKAGE: &str = "package";
pub const SHOP_PRODUCT: &str = "shop_product";
pub const SHOP_CATEGORY: &str = "shop_category";

/// Sitemap files, in the index order.
pub const SITEMAPS: &[&str] = &[
    "pages",
    "posts",
    "accommodations",
    "activities",
    "packages",
    "shop-categories",
    "shop-products",
];

/// Shared facts every rule needs.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub default_locale: String,
    /// Active locales, in their display order.
    pub locales: Vec<String>,
    pub now: DateTime<Utc>,
    /// `shop.status` is not `closed`: shop pages answer.
    pub shop_open: bool,
}

impl Ctx {
    pub async fn load(conn: &mut PgConnection, now: DateTime<Utc>) -> sqlx::Result<Self> {
        let rows = sqlx::query!(
            "SELECT code, is_default FROM locales WHERE is_active ORDER BY is_default DESC, sort_order, code"
        )
        .fetch_all(&mut *conn)
        .await?;
        let shop: Option<serde_json::Value> =
            sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'shop.status'")
                .fetch_optional(&mut *conn)
                .await?;
        // Publication dates are written with the database clock: never judge them by an earlier
        // application clock (a just-published item would look scheduled).
        let db_now: DateTime<Utc> = sqlx::query_scalar!(r#"SELECT now() AS "now!""#)
            .fetch_one(&mut *conn)
            .await?;
        Ok(Self {
            default_locale: rows
                .iter()
                .find(|r| r.is_default)
                .map_or_else(|| "fr".to_string(), |r| r.code.clone()),
            locales: rows.into_iter().map(|r| r.code).collect(),
            now: now.max(db_now),
            shop_open: shop.as_ref().and_then(|v| v.as_str()) != Some("closed"),
        })
    }

    fn prefix(&self, locale: &str) -> String {
        if locale == self.default_locale {
            String::new()
        } else {
            format!("/{locale}")
        }
    }

    pub fn home(&self, locale: &str) -> String {
        if locale == self.default_locale {
            "/".into()
        } else {
            format!("/{locale}")
        }
    }

    /// The shop's path segment in a language (as the Nuxt routes define it).
    pub fn shop_segment(locale: &str) -> &'static str {
        if locale == "en" { "shop" } else { "boutique" }
    }

    pub fn shop(&self, locale: &str) -> String {
        format!("{}/{}", self.prefix(locale), Self::shop_segment(locale))
    }

    pub fn path(&self, entity_type: &str, locale: &str, slug: &str) -> Option<String> {
        let p = self.prefix(locale);
        Some(match entity_type {
            HOME => self.home(locale),
            LISTING_STAYS => format!("{p}/stays"),
            LISTING_ACTIVITIES => format!("{p}/activities"),
            LISTING_SHOP => self.shop(locale),
            PAGE => format!("{p}/{slug}"),
            ACCOMMODATION => format!("{p}/stays/{slug}"),
            ACTIVITY => format!("{p}/activities/{slug}"),
            PACKAGE => format!("{p}/packages/{slug}"),
            SHOP_PRODUCT => format!("{}/p/{slug}", self.shop(locale)),
            SHOP_CATEGORY => format!("{}/{slug}", self.shop(locale)),
            _ => return None,
        })
    }
}

/// One language version of a piece of content.
#[derive(Debug, Clone, Serialize)]
pub struct Version {
    pub path: String,
    pub noindex: bool,
    /// Page excluded from the sitemap (`pages.in_sitemap = false`).
    pub excluded: bool,
    /// `canonical_url` of a page translation.
    pub canonical: Option<String>,
    pub lastmod: Option<DateTime<Utc>>,
}

/// A piece of public content and its URLs.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub entity_type: &'static str,
    pub entity_id: Option<Uuid>,
    pub sitemap: &'static str,
    /// Its pages answer 200 (published, not deleted, parents public, shop open...).
    pub served: bool,
    /// Why no language version may be indexed (even when served: an empty shop category).
    pub blocked: Option<&'static str>,
    pub versions: BTreeMap<String, Version>,
    pub lastmod: Option<DateTime<Utc>>,
    /// Cover then gallery (at most 11 media ids).
    pub media: Vec<Uuid>,
    /// Page type of a builder page (`page`, `post`, `landing`, `legal`, `home`).
    pub page_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum Indexability {
    Indexable,
    NotIndexable(&'static str),
}

impl Indexability {
    pub fn is_indexable(&self) -> bool {
        matches!(self, Self::Indexable)
    }
}

/// The one rule (sitemap, robots meta, URL tester). Reasons: `locale_inactive`, the entry's
/// `blocked` reason (`deleted`, `not_published`, `scheduled`, `parent_not_public`, `inactive`,
/// `no_published_products`, `shop_closed`), `translation_missing`, `noindex`, `not_in_sitemap`,
/// `canonical_elsewhere`.
pub fn judge(ctx: &Ctx, entry: &Entry, locale: &str, base_url: &str) -> Indexability {
    if !ctx.locales.iter().any(|l| l == locale) {
        return Indexability::NotIndexable("locale_inactive");
    }
    if let Some(reason) = entry.blocked {
        return Indexability::NotIndexable(reason);
    }
    let Some(v) = entry.versions.get(locale) else {
        return Indexability::NotIndexable("translation_missing");
    };
    if v.noindex {
        return Indexability::NotIndexable("noindex");
    }
    if v.excluded {
        return Indexability::NotIndexable("not_in_sitemap");
    }
    if let Some(c) = v
        .canonical
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        let own = super::path::absolute(base_url, &v.path);
        if c != v.path && c != own && c.trim_end_matches('/') != own.trim_end_matches('/') {
            return Indexability::NotIndexable("canonical_elsewhere");
        }
    }
    Indexability::Indexable
}

/// Which content a registry call is about.
#[derive(Debug, Clone, Copy)]
pub enum Filter<'a> {
    All,
    /// One sitemap file.
    Sitemap(&'a str),
    One(&'a str, Uuid),
}

fn latest(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    }
}

// ---- Loaders -------------------------------------------------------------------------------------

async fn pages(conn: &mut PgConnection, ctx: &Ctx, id: Option<Uuid>) -> sqlx::Result<Vec<Entry>> {
    let rows = sqlx::query!(
        r#"WITH RECURSIVE up AS (
               SELECT p.id AS page_id, p.parent_id, 0 AS n, true AS ok
               FROM pages p WHERE ($1::uuid IS NULL OR p.id = $1)
               UNION ALL
               SELECT up.page_id, q.parent_id, up.n + 1,
                      q.deleted_at IS NULL AND q.status = 'published' AND (q.publish_at IS NULL OR q.publish_at <= $2)
                      AND EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = q.id AND r.status = 'published')
               FROM up JOIN pages q ON q.id = up.parent_id WHERE up.n < 10)
           SELECT p.id, p.page_type, p.status, p.publish_at, p.deleted_at, p.in_sitemap, p.cover_media_id, p.updated_at,
                  EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published') AS "has_revision!",
                  (SELECT max(r.published_at) FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published') AS published_at,
                  COALESCE((SELECT bool_and(ok) FROM up WHERE up.page_id = p.id AND up.n > 0), true) AS "parents_ok!"
           FROM pages p WHERE ($1::uuid IS NULL OR p.id = $1)"#,
        id,
        ctx.now
    )
    .fetch_all(&mut *conn)
    .await?;
    let texts = sqlx::query!(
        "SELECT page_id, locale, slug, noindex, canonical_url, updated_at FROM page_translations
         WHERE ($1::uuid IS NULL OR page_id = $1)",
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            let blocked = if p.deleted_at.is_some() {
                Some("deleted")
            } else if !p.has_revision || !["published", "scheduled"].contains(&p.status.as_str()) {
                Some("not_published")
            } else if p.status == "scheduled" || p.publish_at.is_some_and(|t| t > ctx.now) {
                Some("scheduled")
            } else if !p.parents_ok {
                Some("parent_not_public")
            } else {
                None
            };
            let lastmod = latest(Some(p.updated_at), p.published_at);
            let entity_type = if p.page_type == "home" { HOME } else { PAGE };
            let versions = texts
                .iter()
                .filter(|t| t.page_id == p.id)
                .filter_map(|t| {
                    Some((
                        t.locale.clone(),
                        Version {
                            path: ctx.path(entity_type, &t.locale, &t.slug)?,
                            noindex: t.noindex,
                            excluded: !p.in_sitemap,
                            canonical: t.canonical_url.clone(),
                            lastmod: latest(lastmod, Some(t.updated_at)),
                        },
                    ))
                })
                .collect();
            Entry {
                entity_type: PAGE,
                entity_id: Some(p.id),
                sitemap: if p.page_type == "post" {
                    "posts"
                } else {
                    "pages"
                },
                served: blocked.is_none(),
                blocked,
                versions,
                lastmod,
                media: p.cover_media_id.into_iter().collect(),
                page_type: Some(p.page_type),
            }
        })
        .collect())
}

/// Entity type of a product kind (`None`: no page of its own, e.g. extras).
pub fn product_entity(kind: &str) -> Option<&'static str> {
    product_sitemap(kind).map(|(t, _)| t)
}

fn product_sitemap(kind: &str) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        "accommodation" => (ACCOMMODATION, "accommodations"),
        "activity" => (ACTIVITY, "activities"),
        "package" => (PACKAGE, "packages"),
        "shop_product" => (SHOP_PRODUCT, "shop-products"),
        _ => return None,
    })
}

async fn products(
    conn: &mut PgConnection,
    ctx: &Ctx,
    kinds: &[&str],
    id: Option<Uuid>,
) -> sqlx::Result<Vec<Entry>> {
    let kinds: Vec<String> = kinds.iter().map(|k| (*k).to_string()).collect();
    let rows = sqlx::query!(
        r#"SELECT p.id, p.kind, p.status, p.published_at, p.deleted_at, p.updated_at, p.cover_media_id,
                  COALESCE(ARRAY(SELECT gi.media_id FROM gallery_items gi WHERE gi.gallery_id = p.gallery_id
                                 ORDER BY gi.sort_order LIMIT 10), '{}') AS "gallery!"
           FROM products p
           WHERE p.kind = ANY($1) AND ($2::uuid IS NULL OR p.id = $2)"#,
        &kinds,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let texts = sqlx::query!(
        "SELECT product_id, locale, slug, updated_at FROM product_translations WHERE product_id = ANY($1)",
        &ids
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|p| {
            let (entity_type, sitemap) = product_sitemap(&p.kind)?;
            let blocked = if p.deleted_at.is_some() {
                Some("deleted")
            } else if p.status != "published" {
                Some("not_published")
            } else if p.published_at.is_some_and(|t| t > ctx.now) {
                Some("scheduled")
            } else if entity_type == SHOP_PRODUCT && !ctx.shop_open {
                Some("shop_closed")
            } else {
                None
            };
            let versions = texts
                .iter()
                .filter(|t| t.product_id == p.id)
                .filter_map(|t| {
                    Some((
                        t.locale.clone(),
                        Version {
                            path: ctx.path(entity_type, &t.locale, &t.slug)?,
                            noindex: false,
                            excluded: false,
                            canonical: None,
                            lastmod: latest(Some(p.updated_at), Some(t.updated_at)),
                        },
                    ))
                })
                .collect();
            Some(Entry {
                entity_type,
                entity_id: Some(p.id),
                sitemap,
                served: blocked.is_none(),
                blocked,
                versions,
                lastmod: Some(p.updated_at),
                media: p.cover_media_id.into_iter().chain(p.gallery).collect(),
                page_type: None,
            })
        })
        .collect())
}

async fn shop_categories(
    conn: &mut PgConnection,
    ctx: &Ctx,
    id: Option<Uuid>,
) -> sqlx::Result<Vec<Entry>> {
    let rows = sqlx::query!(
        r#"WITH vis AS (SELECT * FROM shop_visible_categories())
           SELECT c.id, c.is_active, c.cover_media_id, c.updated_at,
                  EXISTS (SELECT 1 FROM vis WHERE vis.id = c.id) AS "visible!",
                  (SELECT count(DISTINCT pc.product_id) FROM vis x JOIN product_categories pc ON pc.category_id = x.id
                   JOIN products p ON p.id = pc.product_id AND p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL
                   WHERE c.id = ANY(x.path)) AS "published!"
           FROM categories c WHERE c.scope = 'shop' AND ($1::uuid IS NULL OR c.id = $1)"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let texts = sqlx::query!(
        "SELECT t.category_id, t.locale, t.slug, t.updated_at FROM category_translations t
         JOIN categories c ON c.id = t.category_id AND c.scope = 'shop' WHERE ($1::uuid IS NULL OR c.id = $1)",
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|c| {
            let served = ctx.shop_open && c.visible;
            let blocked = if !ctx.shop_open {
                Some("shop_closed")
            } else if !c.is_active {
                Some("inactive")
            } else if !c.visible {
                Some("parent_not_public")
            } else if c.published == 0 {
                Some("no_published_products")
            } else {
                None
            };
            let versions = texts
                .iter()
                .filter(|t| t.category_id == c.id)
                .filter_map(|t| {
                    Some((
                        t.locale.clone(),
                        Version {
                            path: ctx.path(SHOP_CATEGORY, &t.locale, &t.slug)?,
                            noindex: false,
                            excluded: false,
                            canonical: None,
                            lastmod: latest(Some(c.updated_at), Some(t.updated_at)),
                        },
                    ))
                })
                .collect();
            Entry {
                entity_type: SHOP_CATEGORY,
                entity_id: Some(c.id),
                sitemap: "shop-categories",
                served,
                blocked,
                versions,
                lastmod: Some(c.updated_at),
                media: c.cover_media_id.into_iter().collect(),
                page_type: None,
            }
        })
        .collect())
}

/// Home (when no builder home page is live) and the listing pages, in every active language.
async fn listings(conn: &mut PgConnection, ctx: &Ctx, with_home: bool) -> sqlx::Result<Vec<Entry>> {
    let latest_of = sqlx::query!(
        r#"SELECT kind AS "kind!", max(updated_at) AS updated FROM products
           WHERE status = 'published' AND deleted_at IS NULL GROUP BY kind"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let updated = |kind: &str| {
        latest_of
            .iter()
            .find(|r| r.kind == kind)
            .and_then(|r| r.updated)
    };
    let mut out = vec![];
    let mut push = |entity_type: &'static str, served: bool, lastmod: Option<DateTime<Utc>>| {
        out.push(Entry {
            entity_type,
            entity_id: None,
            sitemap: "pages",
            served,
            blocked: (!served).then_some("shop_closed"),
            versions: ctx
                .locales
                .iter()
                .filter_map(|l| {
                    Some((
                        l.clone(),
                        Version {
                            path: ctx.path(entity_type, l, "")?,
                            noindex: false,
                            excluded: false,
                            canonical: None,
                            lastmod,
                        },
                    ))
                })
                .collect(),
            lastmod,
            media: vec![],
            page_type: None,
        });
    };
    if with_home {
        push(HOME, true, None);
    }
    push(LISTING_STAYS, true, updated("accommodation"));
    push(LISTING_ACTIVITIES, true, updated("activity"));
    push(LISTING_SHOP, ctx.shop_open, updated("shop_product"));
    Ok(out)
}

/// Every entry (or one sitemap file's, or one entity).
pub async fn entries(
    conn: &mut PgConnection,
    ctx: &Ctx,
    filter: Filter<'_>,
) -> sqlx::Result<Vec<Entry>> {
    match filter {
        Filter::One(entity_type, id) => match entity_type {
            PAGE | HOME => pages(conn, ctx, Some(id)).await,
            ACCOMMODATION | ACTIVITY | PACKAGE | SHOP_PRODUCT => {
                products(
                    conn,
                    ctx,
                    &["accommodation", "activity", "package", "shop_product"],
                    Some(id),
                )
                .await
            }
            SHOP_CATEGORY => shop_categories(conn, ctx, Some(id)).await,
            _ => Ok(vec![]),
        },
        Filter::Sitemap(name) => {
            let mut out = match name {
                "pages" | "posts" => {
                    let mut p: Vec<Entry> = pages(conn, ctx, None).await?;
                    p.retain(|e| e.sitemap == name);
                    p
                }
                "accommodations" => products(conn, ctx, &["accommodation"], None).await?,
                "activities" => products(conn, ctx, &["activity"], None).await?,
                "packages" => products(conn, ctx, &["package"], None).await?,
                "shop-products" => products(conn, ctx, &["shop_product"], None).await?,
                "shop-categories" => shop_categories(conn, ctx, None).await?,
                _ => vec![],
            };
            if name == "pages" {
                let home_live = out
                    .iter()
                    .any(|e| e.page_type.as_deref() == Some("home") && e.served);
                out.splice(0..0, listings(conn, ctx, !home_live).await?);
            }
            Ok(out)
        }
        Filter::All => {
            let mut out = vec![];
            for name in SITEMAPS {
                out.extend(Box::pin(entries(conn, ctx, Filter::Sitemap(name))).await?);
            }
            Ok(out)
        }
    }
}

/// The localized path of a piece of content (`None` without a translation in that language).
pub async fn path_for(
    conn: &mut PgConnection,
    ctx: &Ctx,
    entity_type: &str,
    entity_id: Uuid,
    locale: &str,
) -> sqlx::Result<Option<String>> {
    Ok(paths_for(conn, ctx, entity_type, entity_id)
        .await?
        .remove(locale))
}

/// Every language's path of a piece of content.
pub async fn paths_for(
    conn: &mut PgConnection,
    ctx: &Ctx,
    entity_type: &str,
    entity_id: Uuid,
) -> sqlx::Result<BTreeMap<String, String>> {
    Ok(entries(conn, ctx, Filter::One(entity_type, entity_id))
        .await?
        .into_iter()
        .next()
        .map(|e| e.versions.into_iter().map(|(l, v)| (l, v.path)).collect())
        .unwrap_or_default())
}

/// Indexability of one language version of a piece of content.
pub async fn indexability(
    conn: &mut PgConnection,
    ctx: &Ctx,
    entity_type: &str,
    entity_id: Uuid,
    locale: &str,
    base_url: &str,
) -> sqlx::Result<Indexability> {
    Ok(
        match entries(conn, ctx, Filter::One(entity_type, entity_id))
            .await?
            .first()
        {
            Some(e) => judge(ctx, e, locale, base_url),
            None => Indexability::NotIndexable("deleted"),
        },
    )
}

/// What serves a path (served or not), in which language.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub entry: Entry,
    pub locale: String,
    pub path: String,
}

/// The content a path points to, whether or not it is served (the tester explains why not).
pub async fn lookup(conn: &mut PgConnection, ctx: &Ctx, raw: &str) -> sqlx::Result<Option<Found>> {
    let Some(path) = normalize(raw) else {
        return Ok(None);
    };
    let segs: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let (locale, rest): (String, Vec<&str>) = match segs.first() {
        Some(first) if *first != ctx.default_locale && ctx.locales.iter().any(|l| l == first) => {
            ((*first).to_string(), segs[1..].to_vec())
        }
        _ => (ctx.default_locale.clone(), segs),
    };
    let found = |entry: Entry| {
        Some(Found {
            entry,
            locale: locale.clone(),
            path: path.clone(),
        })
    };
    let one = |e: Vec<Entry>| e.into_iter().next();
    let shop = Ctx::shop_segment(&locale);
    let slug_of = |n: usize| rest[n..].join("/");
    let entry = match rest.as_slice() {
        [] => {
            let home = sqlx::query_scalar!(
                "SELECT p.id FROM pages p WHERE p.page_type = 'home' AND p.deleted_at IS NULL AND p.status = 'published'
                 AND EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published') LIMIT 1"
            )
            .fetch_optional(&mut *conn)
            .await?;
            match home {
                Some(id) => one(pages(conn, ctx, Some(id)).await?),
                None => listings(conn, ctx, true)
                    .await?
                    .into_iter()
                    .find(|e| e.entity_type == HOME),
            }
        }
        ["stays"] => listings(conn, ctx, false)
            .await?
            .into_iter()
            .find(|e| e.entity_type == LISTING_STAYS),
        ["activities"] => listings(conn, ctx, false)
            .await?
            .into_iter()
            .find(|e| e.entity_type == LISTING_ACTIVITIES),
        [s] if *s == shop => listings(conn, ctx, false)
            .await?
            .into_iter()
            .find(|e| e.entity_type == LISTING_SHOP),
        [kind @ ("stays" | "activities" | "packages"), slug] => {
            let k = match *kind {
                "stays" => "accommodation",
                "activities" => "activity",
                _ => "package",
            };
            match product_by_slug(conn, k, slug, &locale).await? {
                Some(id) => one(products(conn, ctx, &[k], Some(id)).await?),
                None => None,
            }
        }
        [s, "p", slug] if *s == shop => {
            match product_by_slug(conn, "shop_product", slug, &locale).await? {
                Some(id) => one(products(conn, ctx, &["shop_product"], Some(id)).await?),
                None => None,
            }
        }
        [s, slug] if *s == shop => {
            let id = sqlx::query_scalar!(
                "SELECT c.id FROM categories c JOIN category_translations t ON t.category_id = c.id
                 WHERE c.scope = 'shop' AND t.slug = $1 ORDER BY (t.locale = $2) DESC LIMIT 1",
                slug,
                locale
            )
            .fetch_optional(&mut *conn)
            .await?;
            match id {
                Some(id) => one(shop_categories(conn, ctx, Some(id)).await?),
                None => None,
            }
        }
        _ => {
            let slug = slug_of(0);
            let id = sqlx::query_scalar!(
                "SELECT page_id FROM page_translations t JOIN pages p ON p.id = t.page_id
                 WHERE t.slug = $1 AND p.page_type <> 'home' ORDER BY (p.deleted_at IS NULL) DESC, (t.locale = $2) DESC LIMIT 1",
                slug,
                locale
            )
            .fetch_optional(&mut *conn)
            .await?;
            match id {
                Some(id) => one(pages(conn, ctx, Some(id)).await?),
                None => None,
            }
        }
    };
    Ok(entry.and_then(found))
}

async fn product_by_slug(
    conn: &mut PgConnection,
    kind: &str,
    slug: &str,
    locale: &str,
) -> sqlx::Result<Option<Uuid>> {
    sqlx::query_scalar!(
        "SELECT p.id FROM products p JOIN product_translations t ON t.product_id = p.id
         WHERE p.kind = $1 AND t.slug = $2 ORDER BY (p.deleted_at IS NULL) DESC, (t.locale = $3) DESC LIMIT 1",
        kind,
        slug,
        locale
    )
    .fetch_optional(conn)
    .await
}

/// Is this path live content (it answers 200 with content)? `(entity_type, entity_id, locale)`.
pub async fn resolve(
    conn: &mut PgConnection,
    ctx: &Ctx,
    raw: &str,
) -> sqlx::Result<Option<(&'static str, Option<Uuid>, String)>> {
    Ok(lookup(conn, ctx, raw)
        .await?
        .filter(|f| f.entry.served)
        .map(|f| (f.entry.entity_type, f.entry.entity_id, f.locale)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            default_locale: "fr".into(),
            locales: vec!["fr".into(), "en".into(), "ar".into()],
            now: Utc::now(),
            shop_open: true,
        }
    }

    fn entry(versions: &[(&str, Version)]) -> Entry {
        Entry {
            entity_type: PAGE,
            entity_id: Some(Uuid::nil()),
            sitemap: "pages",
            served: true,
            blocked: None,
            versions: versions
                .iter()
                .map(|(l, v)| ((*l).to_string(), v.clone()))
                .collect(),
            lastmod: None,
            media: vec![],
            page_type: Some("page".into()),
        }
    }

    fn v(path: &str) -> Version {
        Version {
            path: path.into(),
            noindex: false,
            excluded: false,
            canonical: None,
            lastmod: None,
        }
    }

    #[test]
    fn paths_follow_the_nuxt_routes() {
        let c = ctx();
        assert_eq!(c.path(HOME, "fr", "").unwrap(), "/");
        assert_eq!(c.path(HOME, "ar", "").unwrap(), "/ar");
        assert_eq!(
            c.path(ACTIVITY, "en", "quad").unwrap(),
            "/en/activities/quad"
        );
        assert_eq!(
            c.path(SHOP_PRODUCT, "fr", "dattes").unwrap(),
            "/boutique/p/dattes"
        );
        assert_eq!(
            c.path(SHOP_CATEGORY, "en", "dates").unwrap(),
            "/en/shop/dates"
        );
        assert_eq!(c.path(PAGE, "ar", "a/b").unwrap(), "/ar/a/b");
        assert_eq!(c.path("extra", "fr", "x"), None);
    }

    #[test]
    fn one_rule_with_its_reasons() {
        let c = ctx();
        let base = "https://mouzoun.com";
        let ok = entry(&[("fr", v("/a"))]);
        assert_eq!(judge(&c, &ok, "fr", base), Indexability::Indexable);
        assert_eq!(
            judge(&c, &ok, "en", base),
            Indexability::NotIndexable("translation_missing")
        );
        assert_eq!(
            judge(&c, &ok, "de", base),
            Indexability::NotIndexable("locale_inactive")
        );
        let mut blocked = ok.clone();
        blocked.blocked = Some("not_published");
        assert_eq!(
            judge(&c, &blocked, "fr", base),
            Indexability::NotIndexable("not_published")
        );
        let mut no = v("/a");
        no.noindex = true;
        assert_eq!(
            judge(&c, &entry(&[("fr", no)]), "fr", base),
            Indexability::NotIndexable("noindex")
        );
        let mut ex = v("/a");
        ex.excluded = true;
        assert_eq!(
            judge(&c, &entry(&[("fr", ex)]), "fr", base),
            Indexability::NotIndexable("not_in_sitemap")
        );
        let mut can = v("/a");
        can.canonical = Some("https://mouzoun.com/b".into());
        assert_eq!(
            judge(&c, &entry(&[("fr", can)]), "fr", base),
            Indexability::NotIndexable("canonical_elsewhere")
        );
        let mut own = v("/a");
        own.canonical = Some("https://mouzoun.com/a".into());
        assert_eq!(
            judge(&c, &entry(&[("fr", own)]), "fr", base),
            Indexability::Indexable
        );
    }
}
