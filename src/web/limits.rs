//! Redis-backed protections: fixed-window rate limits and `Idempotency-Key` replay (24 h).
//!
//! Both degrade open when Redis is down (logged): an outage must not block bookings, and the
//! database constraints remain the real guarantee against double processing.

use std::{future::Future, time::Duration};

use axum::http::{HeaderMap, StatusCode};
use redis::AsyncCommands;
use serde_json::Value;

use crate::{error::AppError, infra::cache::Cache};

/// Allows `max` calls per `window` for `bucket` + `key` (an IP, an email...).
pub async fn rate_limit(
    cache: &Cache,
    bucket: &str,
    key: &str,
    max: u64,
    window: Duration,
) -> Result<(), AppError> {
    let redis_key = cache.key(&format!("rl:{bucket}:{key}"));
    let mut conn = cache.connection();
    let count: redis::RedisResult<u64> = conn.incr(&redis_key, 1u64).await;
    match count {
        Ok(1) => {
            let _: redis::RedisResult<()> = conn
                .expire(&redis_key, window.as_secs().max(1) as i64)
                .await;
            Ok(())
        }
        Ok(n) if n > max => {
            tracing::info!(bucket, "rate limit reached");
            Err(AppError::too_many_requests())
        }
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, "rate limit unavailable (redis), allowing");
            Ok(())
        }
    }
}

const PENDING: &str = "__pending__";
const TTL_SECS: u64 = 24 * 3600;

/// The `Idempotency-Key` header, when present and sane.
pub fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= 200 && k.chars().all(|c| c.is_ascii_graphic()))
        .map(str::to_string)
}

/// Runs `work` once per key: a retry with the same key gets the stored response; a concurrent
/// one gets `409 request_in_progress`. Failures are not stored (the guest may retry).
pub async fn idempotent<F, Fut>(
    cache: &Cache,
    scope: &str,
    key: Option<String>,
    work: F,
) -> Result<Value, AppError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Value, AppError>>,
{
    let Some(key) = key else { return work().await };
    let redis_key = cache.key(&format!("idem:{scope}:{key}"));
    let mut conn = cache.connection();
    let claimed: redis::RedisResult<bool> = redis::cmd("SET")
        .arg(&redis_key)
        .arg(PENDING)
        .arg("NX")
        .arg("EX")
        .arg(TTL_SECS)
        .query_async::<Option<String>>(&mut conn)
        .await
        .map(|r| r.is_some());
    match claimed {
        Ok(true) => {}
        Ok(false) => {
            let stored: Option<String> = conn.get(&redis_key).await.unwrap_or(None);
            return match stored.as_deref() {
                Some(PENDING) | None => Err(AppError::domain(
                    StatusCode::CONFLICT,
                    "request_in_progress",
                    "the same request is already being processed",
                )),
                Some(raw) => serde_json::from_str(raw).map_err(|e| {
                    AppError::Internal(anyhow::anyhow!("stored idempotent response: {e}"))
                }),
            };
        }
        Err(e) => {
            tracing::warn!(error = %e, "idempotency unavailable (redis), running once");
            return work().await;
        }
    }
    let result = work().await;
    match &result {
        Ok(value) => {
            let _: redis::RedisResult<()> =
                conn.set_ex(&redis_key, value.to_string(), TTL_SECS).await;
        }
        Err(_) => {
            let _: redis::RedisResult<()> = conn.del(&redis_key).await;
        }
    }
    result
}
