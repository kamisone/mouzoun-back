//! The redirect map the Nuxt server serves from (Redis, versioned), and visit counting: the Nuxt
//! server batches redirect hits and 404s, the API adds them to Redis hashes (`HINCRBY`), a job
//! flushes them into Postgres every minute. Postgres is never written per request. Redis being
//! down degrades to building the map from Postgres and losing counts, never to an error.

use std::collections::BTreeMap;

use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use super::{REDIRECTS_MAP, REDIRECTS_VERSION, path};
use crate::infra::cache::Cache;

const HITS_REDIRECTS: &str = "seo:hits:redirects";
const HITS_404: &str = "seo:hits:404";
const HITS_404_REFERRER: &str = "seo:hits:404:referrer";
/// Most rows kept in `not_found_log`.
pub const NOT_FOUND_MAX: i64 = 5_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub to: String,
    pub code: i16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedirectMap {
    pub version: String,
    pub redirects: BTreeMap<String, Target>,
}

pub async fn bump_version(cache: &Cache) -> redis::RedisResult<()> {
    let mut conn = cache.connection();
    let _: i64 = conn.incr(cache.key(REDIRECTS_VERSION), 1).await?;
    let _: () = conn.del(cache.key(REDIRECTS_MAP)).await?;
    Ok(())
}

/// Fingerprint of the active redirects, when Redis cannot say.
async fn db_version(db: &PgPool) -> sqlx::Result<String> {
    sqlx::query_scalar!(
        r#"SELECT 'db-' || md5(COALESCE(string_agg(from_path || '>' || to_path || ':' || status_code, ',' ORDER BY from_path), '')) AS "v!"
           FROM redirects WHERE is_active"#
    )
    .fetch_one(db)
    .await
}

pub async fn version(cache: &Cache, db: &PgPool) -> sqlx::Result<String> {
    let mut conn = cache.connection();
    let key = cache.key(REDIRECTS_VERSION);
    match conn.get::<_, Option<i64>>(&key).await {
        Ok(Some(v)) => Ok(v.to_string()),
        Ok(None) => {
            let _: redis::RedisResult<bool> = conn.set_nx(&key, 1).await;
            Ok("1".into())
        }
        Err(e) => {
            tracing::warn!(error = %e, "seo: redirect version from redis failed, using the database");
            db_version(db).await
        }
    }
}

async fn load(db: &PgPool) -> sqlx::Result<BTreeMap<String, Target>> {
    Ok(
        sqlx::query!("SELECT from_path, to_path, status_code FROM redirects WHERE is_active")
            .fetch_all(db)
            .await?
            .into_iter()
            .map(|r| {
                (
                    r.from_path,
                    Target {
                        to: r.to_path,
                        code: r.status_code,
                    },
                )
            })
            .collect(),
    )
}

/// The active redirects with their version (cached in Redis until the next change).
pub async fn map(cache: &Cache, db: &PgPool) -> sqlx::Result<RedirectMap> {
    let version = version(cache, db).await?;
    if let Some(m) = cache.get_json::<RedirectMap>(REDIRECTS_MAP).await
        && m.version == version
    {
        return Ok(m);
    }
    let m = RedirectMap {
        version,
        redirects: load(db).await?,
    };
    cache
        .set_json(REDIRECTS_MAP, &m, std::time::Duration::from_secs(24 * 3600))
        .await;
    Ok(m)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct NotFoundHit {
    pub path: String,
    pub hits: i64,
    pub referrer: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Hits {
    /// `from_path` → visits.
    pub redirects: BTreeMap<String, i64>,
    pub not_found: Vec<NotFoundHit>,
}

/// Adds a batch from the Nuxt server to the Redis counters. Scanner noise is dropped.
pub async fn add(cache: &Cache, batch: &Hits) -> redis::RedisResult<()> {
    let mut conn = cache.connection();
    let mut pipe = redis::pipe();
    for (p, n) in batch.redirects.iter().take(1_000) {
        if *n > 0 {
            pipe.hincr(cache.key(HITS_REDIRECTS), p, (*n).min(1_000_000))
                .ignore();
        }
    }
    for h in batch.not_found.iter().take(1_000) {
        let Some(p) = path::normalize(&h.path).filter(|p| !path::is_noise(p)) else {
            continue;
        };
        pipe.hincr(cache.key(HITS_404), &p, h.hits.clamp(1, 1_000_000))
            .ignore();
        if let Some(r) = h.referrer.as_deref().filter(|r| !r.is_empty()) {
            pipe.hset(
                cache.key(HITS_404_REFERRER),
                &p,
                r.chars().take(500).collect::<String>(),
            )
            .ignore();
        }
    }
    pipe.query_async::<()>(&mut conn).await
}

/// Takes a hash atomically (RENAME then read), so counts added meanwhile go to the next flush.
async fn take(cache: &Cache, key: &str) -> redis::RedisResult<BTreeMap<String, String>> {
    let mut conn = cache.connection();
    let from = cache.key(key);
    let tmp = format!("{from}:flushing");
    match redis::cmd("RENAME")
        .arg(&from)
        .arg(&tmp)
        .query_async::<()>(&mut conn)
        .await
    {
        Ok(()) => {}
        // No such key: nothing to flush.
        Err(e) if e.to_string().contains("no such key") => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    }
    let all: BTreeMap<String, String> = conn.hgetall(&tmp).await?;
    let _: () = conn.del(&tmp).await?;
    Ok(all)
}

/// Writes the counted visits to Postgres (every minute) and keeps the 404 log bounded.
pub async fn flush(cache: &Cache, db: &PgPool) -> anyhow::Result<(usize, usize)> {
    let redirects = take(cache, HITS_REDIRECTS).await?;
    let missing = take(cache, HITS_404).await?;
    let referrers = take(cache, HITS_404_REFERRER).await?;
    for (p, n) in &redirects {
        let n: i64 = n.parse().unwrap_or(0);
        sqlx::query!(
            "UPDATE redirects SET hits = hits + $2, last_hit_at = now() WHERE from_path = $1",
            p,
            n
        )
        .execute(db)
        .await?;
    }
    for (p, n) in &missing {
        let n: i64 = n.parse().unwrap_or(0);
        sqlx::query!(
            "INSERT INTO not_found_log (path, hits, last_referrer) VALUES ($1, $2, $3)
             ON CONFLICT (path) DO UPDATE SET hits = not_found_log.hits + EXCLUDED.hits, last_seen_at = now(),
                 last_referrer = COALESCE(EXCLUDED.last_referrer, not_found_log.last_referrer)",
            p,
            n,
            referrers.get(p)
        )
        .execute(db)
        .await?;
    }
    if !missing.is_empty() {
        trim(db).await?;
    }
    Ok((redirects.len(), missing.len()))
}

/// At most `NOT_FOUND_MAX` rows: the least visited, oldest ones go.
pub async fn trim(db: &PgPool) -> sqlx::Result<u64> {
    Ok(sqlx::query!(
        "DELETE FROM not_found_log WHERE path IN (
             SELECT path FROM not_found_log ORDER BY hits DESC, last_seen_at DESC OFFSET $1)",
        NOT_FOUND_MAX
    )
    .execute(db)
    .await?
    .rows_affected())
}

/// Daily: 404s not seen for 90 days go.
pub async fn purge(db: &PgPool) -> sqlx::Result<u64> {
    Ok(
        sqlx::query!("DELETE FROM not_found_log WHERE last_seen_at < now() - interval '90 days'")
            .execute(db)
            .await?
            .rows_affected(),
    )
}
