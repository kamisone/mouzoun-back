//! Multilingual sitemaps built from the route registry: an index (`/sitemap.xml`) and one file
//! per content type (`/sitemaps/{name}.xml`, split past 10,000 URLs as `{name}-2.xml`...). One
//! `<url>` per indexable language version with `xhtml:link` alternates to every indexable version
//! plus `x-default` (the default language), `<lastmod>` from the content, up to 11 public images.
//! Files are cached in Redis (`seo:sitemap:*`, 24 h) and dropped by `seo.changed` events.

use std::{collections::HashMap, fmt::Write as _, time::Duration};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    path::absolute,
    routes::{self, Ctx, Entry, Filter, SITEMAPS, judge},
};
use crate::{error::AppResult, state::AppState};

pub const CACHE_PREFIX: &str = "seo:sitemap:";
const TTL: Duration = Duration::from_secs(24 * 3600);
pub const MAX_URLS: usize = 10_000;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

fn w3c(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn base_url(state: &AppState) -> String {
    state
        .config
        .email
        .public_site_url
        .trim_end_matches('/')
        .to_string()
}

/// One `<url>` of the sitemap (also what the admin status counts).
#[derive(Debug, Clone, Serialize)]
pub struct UrlEntry {
    pub loc: String,
    pub path: String,
    pub locale: String,
    pub lastmod: Option<DateTime<Utc>>,
    /// `(hreflang, href)`, `x-default` last.
    pub alternates: Vec<(String, String)>,
    pub images: Vec<String>,
}

/// Absolute public URLs of media (private or deleted media are skipped).
async fn media_urls(
    state: &AppState,
    conn: &mut PgConnection,
    ids: &[Uuid],
    base: &str,
) -> sqlx::Result<HashMap<Uuid, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        "SELECT id, storage_provider, object_key FROM media_assets
         WHERE id = ANY($1) AND deleted_at IS NULL AND visibility = 'public' AND kind = 'image'",
        ids
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let url = if r.storage_provider == "site" {
                absolute(base, &r.object_key)
            } else {
                state.media.public_url(&r.object_key)
            };
            (r.id, url)
        })
        .collect())
}

/// The URLs of one content type, every indexable language version.
pub async fn urls(
    state: &AppState,
    conn: &mut PgConnection,
    ctx: &Ctx,
    name: &str,
) -> sqlx::Result<Vec<UrlEntry>> {
    let base = base_url(state);
    let entries = routes::entries(conn, ctx, Filter::Sitemap(name)).await?;
    let media: Vec<Uuid> = entries
        .iter()
        .flat_map(|e| e.media.iter().copied())
        .collect();
    let images = media_urls(state, conn, &media, &base).await?;
    Ok(entries
        .iter()
        .flat_map(|e| entry_urls(ctx, e, &base, &images))
        .collect())
}

pub fn entry_urls(
    ctx: &Ctx,
    e: &Entry,
    base: &str,
    images: &HashMap<Uuid, String>,
) -> Vec<UrlEntry> {
    let indexable: Vec<(&String, &routes::Version)> = ctx
        .locales
        .iter()
        .filter(|l| judge(ctx, e, l, base).is_indexable())
        .filter_map(|l| e.versions.get_key_value(l))
        .collect();
    let mut alternates: Vec<(String, String)> = indexable
        .iter()
        .map(|(l, v)| ((*l).clone(), absolute(base, &v.path)))
        .collect();
    if let Some((_, v)) = indexable.iter().find(|(l, _)| **l == ctx.default_locale) {
        alternates.push(("x-default".into(), absolute(base, &v.path)));
    }
    let pictures: Vec<String> = e
        .media
        .iter()
        .filter_map(|m| images.get(m).cloned())
        .take(11)
        .collect();
    indexable
        .iter()
        .map(|(l, v)| UrlEntry {
            loc: absolute(base, &v.path),
            path: v.path.clone(),
            locale: (*l).clone(),
            lastmod: v.lastmod.or(e.lastmod),
            alternates: if alternates.len() > 2 {
                alternates.clone()
            } else {
                vec![]
            },
            images: pictures.clone(),
        })
        .collect()
}

pub fn render_urlset(urls: &[UrlEntry]) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\" \
         xmlns:xhtml=\"http://www.w3.org/1999/xhtml\" xmlns:image=\"http://www.google.com/schemas/sitemap-image/1.1\">\n",
    );
    for u in urls {
        let _ = write!(x, "  <url>\n    <loc>{}</loc>\n", escape(&u.loc));
        if let Some(t) = u.lastmod {
            let _ = writeln!(x, "    <lastmod>{}</lastmod>", w3c(t));
        }
        for (lang, href) in &u.alternates {
            let _ = writeln!(
                x,
                "    <xhtml:link rel=\"alternate\" hreflang=\"{}\" href=\"{}\"/>",
                escape(lang),
                escape(href)
            );
        }
        for img in &u.images {
            let _ = writeln!(
                x,
                "    <image:image><image:loc>{}</image:loc></image:image>",
                escape(img)
            );
        }
        x.push_str("  </url>\n");
    }
    x.push_str("</urlset>\n");
    x
}

/// File name of a page of a sitemap (`activities.xml`, `activities-2.xml`).
pub fn file_name(name: &str, page: usize) -> String {
    if page <= 1 {
        format!("{name}.xml")
    } else {
        format!("{name}-{page}.xml")
    }
}

/// `activities-2` → (`activities`, 2).
pub fn parse_name(file: &str) -> Option<(&'static str, usize)> {
    let stem = file.strip_suffix(".xml").unwrap_or(file);
    for name in SITEMAPS {
        if stem == *name {
            return Some((name, 1));
        }
        if let Some(n) = stem
            .strip_prefix(name)
            .and_then(|r| r.strip_prefix('-'))
            .and_then(|n| n.parse().ok())
        {
            return (n >= 2).then_some((name, n));
        }
    }
    None
}

async fn record_build(state: &AppState) {
    let _ = sqlx::query!(
        "UPDATE settings SET value = to_jsonb(now()), updated_at = now() WHERE key = 'seo.sitemap_built_at'"
    )
    .execute(&state.db)
    .await;
}

/// One sitemap file (cached). `None`: no such file (unknown, empty, or past the last page).
pub async fn file(state: &AppState, name: &str, page: usize) -> AppResult<Option<String>> {
    let key = format!("{CACHE_PREFIX}{name}:{page}");
    if let Some(xml) = state.cache.get_json::<String>(&key).await {
        return Ok(Some(xml));
    }
    let mut conn = state.db.acquire().await?;
    let ctx = Ctx::load(&mut conn, state.clock.now()).await?;
    let all = urls(state, &mut conn, &ctx, name).await?;
    let Some(chunk) = all.chunks(MAX_URLS).nth(page.saturating_sub(1)) else {
        return Ok(None);
    };
    let xml = render_urlset(chunk);
    state.cache.set_json(&key, &xml, TTL).await;
    record_build(state).await;
    Ok(Some(xml))
}

/// Pages of every non-empty file with their latest `lastmod`.
pub async fn files(
    state: &AppState,
) -> AppResult<Vec<(String, Option<DateTime<Utc>>, usize, HashMap<String, usize>)>> {
    let mut conn = state.db.acquire().await?;
    let ctx = Ctx::load(&mut conn, state.clock.now()).await?;
    let mut out = vec![];
    for name in SITEMAPS {
        let all = urls(state, &mut conn, &ctx, name).await?;
        for (i, chunk) in all.chunks(MAX_URLS).enumerate() {
            let mut per_locale = HashMap::new();
            for u in chunk {
                *per_locale.entry(u.locale.clone()).or_insert(0) += 1;
            }
            out.push((
                file_name(name, i + 1),
                chunk.iter().filter_map(|u| u.lastmod).max(),
                chunk.len(),
                per_locale,
            ));
        }
    }
    Ok(out)
}

/// The sitemap index (cached).
pub async fn index(state: &AppState) -> AppResult<String> {
    let key = format!("{CACHE_PREFIX}index");
    if let Some(xml) = state.cache.get_json::<String>(&key).await {
        return Ok(xml);
    }
    let base = base_url(state);
    let mut x = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    for (file, lastmod, _, _) in files(state).await? {
        let _ = write!(
            x,
            "  <sitemap>\n    <loc>{}</loc>\n",
            escape(&format!("{base}/sitemaps/{file}"))
        );
        if let Some(t) = lastmod {
            let _ = writeln!(x, "    <lastmod>{}</lastmod>", w3c(t));
        }
        x.push_str("  </sitemap>\n");
    }
    x.push_str("</sitemapindex>\n");
    state.cache.set_json(&key, &x, TTL).await;
    record_build(state).await;
    Ok(x)
}

/// Drops cached files (all when `names` is empty) and the index.
pub async fn invalidate(
    cache: &crate::infra::cache::Cache,
    names: &[String],
) -> redis::RedisResult<()> {
    if names.is_empty() {
        cache.delete_prefix(CACHE_PREFIX).await?;
    } else {
        for n in names {
            cache.delete_prefix(&format!("{CACHE_PREFIX}{n}:")).await?;
        }
        cache.delete([format!("{CACHE_PREFIX}index")]).await;
    }
    // The head tags (canonical, hreflang, robots) are built from the same registry.
    cache.delete_prefix(super::HEAD_CACHE_PREFIX).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_escaping() {
        assert_eq!(parse_name("activities.xml"), Some(("activities", 1)));
        assert_eq!(
            parse_name("shop-products-3.xml"),
            Some(("shop-products", 3))
        );
        assert_eq!(parse_name("activities-1.xml"), None);
        assert_eq!(parse_name("secrets.xml"), None);
        assert_eq!(file_name("pages", 2), "pages-2.xml");
        assert_eq!(escape("a&b<\"c\">'"), "a&amp;b&lt;&quot;c&quot;&gt;&apos;");
    }
}
