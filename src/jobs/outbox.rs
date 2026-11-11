//! Publishes outbox events to the Redis stream `mouzoun:events`.

use chrono::Utc;
use redis::aio::ConnectionManager;
use sqlx::PgPool;

use crate::services::outbox::{RedisStreamPublisher, dispatch_batch};

pub async fn run_once(db: PgPool, redis: ConnectionManager) -> anyhow::Result<()> {
    dispatch_batch(&db, &RedisStreamPublisher::new(redis), 100, Utc::now()).await?;
    Ok(())
}
