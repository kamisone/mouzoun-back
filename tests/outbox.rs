//! Outbox dispatch: published events are marked processed, failures are retried later.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};

use common::fixtures::now;
use mouzoun_api::{
    db::operations::OutboxEvent,
    services::outbox::{EventPublisher, dispatch_batch, enqueue},
};
use serde_json::json;
use sqlx::PgPool;

struct Recorder(AtomicUsize);
impl EventPublisher for Recorder {
    async fn publish(&self, _: &OutboxEvent) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Down;
impl EventPublisher for Down {
    async fn publish(&self, _: &OutboxEvent) -> anyhow::Result<()> {
        anyhow::bail!("redis unreachable")
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn events_are_published_once_and_failures_are_retried_with_backoff(db: PgPool) {
    let mut conn = db.acquire().await.unwrap();
    for n in 0..3 {
        enqueue(
            &mut conn,
            "booking",
            &n.to_string(),
            "booking.confirmed",
            json!({ "n": n }),
        )
        .await
        .unwrap();
    }
    drop(conn);

    assert_eq!(dispatch_batch(&db, &Down, 10, now()).await.unwrap(), 0);
    let (attempts, error, delayed): (i32, String, bool) = sqlx::query_as(
        "SELECT attempts, last_error, available_at > $1 FROM outbox_events ORDER BY id LIMIT 1",
    )
    .bind(now())
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (attempts, error.as_str(), delayed),
        (1, "redis unreachable", true)
    );
    assert_eq!(
        dispatch_batch(&db, &Recorder(AtomicUsize::new(0)), 10, now())
            .await
            .unwrap(),
        0,
        "not due yet"
    );

    let recorder = Recorder(AtomicUsize::new(0));
    let later = now() + chrono::Duration::minutes(5);
    assert_eq!(dispatch_batch(&db, &recorder, 10, later).await.unwrap(), 3);
    assert_eq!(
        dispatch_batch(&db, &recorder, 10, later).await.unwrap(),
        0,
        "never published twice"
    );
    assert_eq!(recorder.0.load(Ordering::SeqCst), 3);
}
