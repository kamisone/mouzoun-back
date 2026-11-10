//! Redis-backed JSON cache.
//!
//! The cache is an optimisation, never a source of truth: every failure is logged
//! and treated as a miss, so a Redis outage degrades latency instead of breaking requests.

use std::time::Duration;

use redis::{AsyncCommands, aio::ConnectionManager};
use serde::{Serialize, de::DeserializeOwned};

#[derive(Clone)]
pub struct Cache {
    conn: ConnectionManager,
    /// Namespace prepended to every key (isolates environments / test runs sharing a Redis).
    prefix: String,
}

impl Cache {
    pub async fn connect(url: &str, prefix: impl Into<String>) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self {
            conn,
            prefix: prefix.into(),
        })
    }

    fn key(&self, key: &str) -> String {
        format!("{}:{key}", self.prefix)
    }

    pub async fn get_json<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let mut conn = self.conn.clone();
        match conn.get::<_, Option<String>>(self.key(key)).await {
            Ok(Some(raw)) => serde_json::from_str(&raw)
                .inspect_err(|e| tracing::warn!(key, error = %e, "cache: corrupt entry"))
                .ok(),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(key, error = %e, "cache: read failed");
                None
            }
        }
    }

    pub async fn set_json<T: Serialize>(&self, key: &str, value: &T, ttl: Duration) {
        let Ok(raw) = serde_json::to_string(value) else {
            return;
        };
        let mut conn = self.conn.clone();
        if let Err(e) = conn
            .set_ex::<_, _, ()>(self.key(key), raw, ttl.as_secs().max(1))
            .await
        {
            tracing::warn!(key, error = %e, "cache: write failed");
        }
    }

    pub async fn delete(&self, keys: &[&str]) {
        if keys.is_empty() {
            return;
        }
        let keys: Vec<String> = keys.iter().map(|k| self.key(k)).collect();
        let mut conn = self.conn.clone();
        if let Err(e) = conn.del::<_, ()>(keys).await {
            tracing::warn!(error = %e, "cache: delete failed");
        }
    }

    pub async fn ping(&self) -> bool {
        let mut conn = self.conn.clone();
        redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
            .is_ok()
    }
}
