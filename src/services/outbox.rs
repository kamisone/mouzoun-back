//! Transactional outbox.
//!
//! State changes write an `outbox_events` row **in their own transaction** (`enqueue`), so an
//! event exists if and only if the change committed. A background job (`dispatch_batch`) then
//! publishes pending events to a Redis stream, where consumers (emails, notifications...) read
//! them; failures are retried with exponential backoff. Delivery is at-least-once: consumers
//! must be idempotent (the event id is part of every message).

use std::future::Future;

use chrono::{DateTime, Utc};
use redis::aio::ConnectionManager;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

use crate::db::operations::OutboxEvent;

pub const EVENTS_STREAM: &str = "mouzoun:events";

pub async fn enqueue(
    conn: &mut PgConnection,
    aggregate_type: &str,
    aggregate_id: &str,
    event_type: &str,
    payload: Value,
) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        "INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload)
         VALUES ($1, $2, $3, $4) RETURNING id",
        aggregate_type,
        aggregate_id,
        event_type,
        payload
    )
    .fetch_one(conn)
    .await
}

/// Where outbox events are published.
pub trait EventPublisher: Send + Sync {
    fn publish(&self, event: &OutboxEvent) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Publishes to a Redis stream (`XADD mouzoun:events MAXLEN ~ 100000 * ...`).
#[derive(Clone)]
pub struct RedisStreamPublisher {
    conn: ConnectionManager,
}

impl RedisStreamPublisher {
    pub fn new(conn: ConnectionManager) -> Self {
        Self { conn }
    }
}

impl EventPublisher for RedisStreamPublisher {
    async fn publish(&self, event: &OutboxEvent) -> anyhow::Result<()> {
        let mut conn = self.conn.clone();
        redis::cmd("XADD")
            .arg(EVENTS_STREAM)
            .arg("MAXLEN")
            .arg("~")
            .arg(100_000)
            .arg("*")
            .arg("id")
            .arg(event.id)
            .arg("type")
            .arg(&event.event_type)
            .arg("aggregate_type")
            .arg(&event.aggregate_type)
            .arg("aggregate_id")
            .arg(&event.aggregate_id)
            .arg("payload")
            .arg(event.payload.to_string())
            .query_async::<String>(&mut conn)
            .await?;
        Ok(())
    }
}

/// Publishes up to `batch` due events. `SKIP LOCKED` lets several instances dispatch at once
/// without publishing the same event twice concurrently.
pub async fn dispatch_batch(
    pool: &PgPool,
    publisher: &impl EventPublisher,
    batch: i64,
    now: DateTime<Utc>,
) -> sqlx::Result<usize> {
    let mut tx = pool.begin().await?;
    let events = sqlx::query_as!(
        OutboxEvent,
        "SELECT id, aggregate_type, aggregate_id, event_type, payload, available_at, attempts,
                last_error, processed_at, created_at
         FROM outbox_events
         WHERE processed_at IS NULL AND available_at <= $1
         ORDER BY id
         LIMIT $2
         FOR UPDATE SKIP LOCKED",
        now,
        batch
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut published = 0;
    for event in &events {
        match publisher.publish(event).await {
            Ok(()) => {
                sqlx::query!(
                    "UPDATE outbox_events SET processed_at = $2 WHERE id = $1",
                    event.id,
                    now
                )
                .execute(&mut *tx)
                .await?;
                published += 1;
            }
            Err(e) => {
                // 2^attempts seconds, capped at one hour.
                let delay_secs = 2_f64.powi(event.attempts.min(12)).min(3600.0);
                sqlx::query!(
                    "UPDATE outbox_events
                     SET attempts = attempts + 1, last_error = $2,
                         available_at = $3::timestamptz + make_interval(secs => $4)
                     WHERE id = $1",
                    event.id,
                    e.to_string(),
                    now,
                    delay_secs
                )
                .execute(&mut *tx)
                .await?;
                tracing::warn!(event_id = event.id, error = %e, "outbox: publish failed, will retry");
            }
        }
    }
    tx.commit().await?;
    Ok(published)
}
