//! Outbox dispatcher: exactly-once processing across concurrent workers, retries with backoff,
//! dead events, unknown types, deferral and secret scrubbing.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::common::fixtures::now;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use mouzoun_api::{
    db::operations::OutboxEvent,
    services::email::admin,
    worker::{HandlerError, OutboxHandler, Registry, process_batch},
};
use serde_json::json;
use sqlx::PgPool;

const PING: &str = "test.ping";

/// Records how many times each event was handled; fails or defers on demand.
#[derive(Default)]
struct Probe {
    seen: Mutex<HashMap<i64, usize>>,
    behaviour: Behaviour,
}

#[derive(Default, Clone, Copy)]
enum Behaviour {
    #[default]
    Ok,
    Transient,
    Permanent,
    DeferTo(DateTime<Utc>),
}

#[async_trait]
impl OutboxHandler for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn event_types(&self) -> &'static [&'static str] {
        &[PING]
    }
    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        *self.seen.lock().unwrap().entry(event.id).or_default() += 1;
        tokio::task::yield_now().await;
        match self.behaviour {
            Behaviour::Ok => Ok(()),
            Behaviour::Transient => Err(HandlerError::Retry(anyhow::anyhow!("smtp timeout"))),
            Behaviour::Permanent => Err(HandlerError::Permanent(anyhow::anyhow!("bad template"))),
            Behaviour::DeferTo(at) => Err(HandlerError::Defer(at)),
        }
    }
}

fn registry(probe: &Arc<Probe>) -> Registry {
    Registry::new().register(probe.clone())
}

async fn insert(db: &PgPool, event_type: &str, payload: serde_json::Value) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO outbox_events (aggregate_type, aggregate_id, event_type, payload) VALUES ('test', '1', $1, $2) RETURNING id",
    )
    .bind(event_type)
    .bind(payload)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn state(
    db: &PgPool,
    id: i64,
) -> (
    i32,
    Option<String>,
    DateTime<Utc>,
    bool,
    bool,
    serde_json::Value,
) {
    sqlx::query_as(
        "SELECT attempts, last_error, available_at, processed_at IS NOT NULL, failed_at IS NOT NULL, payload
         FROM outbox_events WHERE id = $1",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn two_concurrent_workers_process_each_of_100_events_exactly_once(db: PgPool) {
    for n in 0..100 {
        insert(&db, PING, json!({ "n": n })).await;
    }
    let probe = Arc::new(Probe::default());
    let registry = Arc::new(registry(&probe));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let (db, registry) = (db.clone(), registry.clone());
            tokio::spawn(async move {
                let mut handled = 0;
                loop {
                    let report = process_batch(&db, &registry, 7, 8, now()).await.unwrap();
                    if report.claimed == 0 {
                        return handled;
                    }
                    handled += report.processed;
                }
            })
        })
        .collect();
    let mut per_worker = Vec::new();
    for w in workers {
        per_worker.push(w.await.unwrap());
    }

    let seen = probe.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 100, "every event handled");
    assert!(seen.values().all(|&n| n == 1), "no event handled twice");
    assert_eq!(per_worker.iter().sum::<usize>(), 100);
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE processed_at IS NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(pending, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn transient_failures_back_off_then_the_event_dies_after_max_attempts(db: PgPool) {
    let id = insert(&db, PING, json!({})).await;
    let probe = Arc::new(Probe {
        behaviour: Behaviour::Transient,
        ..Probe::default()
    });
    let registry = registry(&probe);

    // Expected delays after 1, 2, 3 failures: 30 s, 2 min, 10 min (±10 % jitter).
    let mut t = now();
    for (attempt, nominal) in [(1, 30), (2, 120), (3, 600)] {
        let report = process_batch(&db, &registry, 10, 4, t).await.unwrap();
        assert_eq!(report.retried, 1);
        let (attempts, error, available_at, processed, dead, _) = state(&db, id).await;
        assert_eq!((attempts, processed, dead), (attempt, false, false));
        assert_eq!(error.as_deref(), Some("smtp timeout"));
        let delay = (available_at - t).num_milliseconds() as f64 / 1000.0;
        assert!(
            (nominal as f64 * 0.9 - 0.01..=nominal as f64 * 1.1 + 0.01).contains(&delay),
            "attempt {attempt}: delay {delay}s, expected {nominal}s ±10%"
        );
        // Not claimed before it is due.
        let early = process_batch(&db, &registry, 10, 4, available_at - Duration::seconds(1))
            .await
            .unwrap();
        assert_eq!(early.claimed, 0);
        t = available_at;
    }
    // 4th failure with OUTBOX_MAX_ATTEMPTS = 4: dead, never claimed again.
    let report = process_batch(&db, &registry, 10, 4, t).await.unwrap();
    assert_eq!(report.dead, 1);
    let (attempts, _, _, processed, dead, _) = state(&db, id).await;
    assert_eq!((attempts, processed, dead), (4, false, true));
    let later = process_batch(&db, &registry, 10, 4, t + Duration::days(30))
        .await
        .unwrap();
    assert_eq!(later.claimed, 0, "dead events are not retried");

    // An operator replays it.
    assert_eq!(admin::list_dead(&db, 10).await.unwrap().len(), 1);
    assert!(admin::retry_event(&db, id).await.unwrap());
    assert!(
        !admin::retry_event(&db, id).await.unwrap(),
        "only dead events can be replayed"
    );
    let (attempts, error, _, _, dead, _) = state(&db, id).await;
    assert_eq!((attempts, error, dead), (0, None, false));
}

#[sqlx::test(migrations = "./migrations")]
async fn permanent_errors_kill_the_event_at_once_and_secrets_are_scrubbed(db: PgPool) {
    let id = insert(
        &db,
        PING,
        json!({ "user_id": "u", "secret": { "token": "raw-token" } }),
    )
    .await;
    let probe = Arc::new(Probe {
        behaviour: Behaviour::Permanent,
        ..Probe::default()
    });
    let report = process_batch(&db, &registry(&probe), 10, 8, now())
        .await
        .unwrap();
    assert_eq!(report.dead, 1);
    let (attempts, error, _, _, dead, payload) = state(&db, id).await;
    assert_eq!((attempts, dead), (1, true));
    assert_eq!(error.as_deref(), Some("bad template"));
    assert_eq!(
        payload,
        json!({ "user_id": "u" }),
        "the raw token never outlives the event"
    );

    // Processed events are scrubbed too.
    let ok = insert(&db, PING, json!({ "secret": { "token": "t" } })).await;
    let probe = Arc::new(Probe::default());
    process_batch(&db, &registry(&probe), 10, 8, now())
        .await
        .unwrap();
    let (_, _, _, processed, _, payload) = state(&db, ok).await;
    assert!(processed);
    assert_eq!(payload, json!({}));
}

#[sqlx::test(migrations = "./migrations")]
async fn unknown_event_types_are_marked_processed_not_retried(db: PgPool) {
    let id = insert(&db, "nobody.listens", json!({})).await;
    let probe = Arc::new(Probe::default());
    let report = process_batch(&db, &registry(&probe), 10, 8, now())
        .await
        .unwrap();
    assert_eq!((report.claimed, report.processed), (1, 1));
    let (attempts, _, _, processed, dead, _) = state(&db, id).await;
    assert_eq!((attempts, processed, dead), (0, true, false));
}

#[sqlx::test(migrations = "./migrations")]
async fn deferred_events_wait_without_counting_an_attempt(db: PgPool) {
    let id = insert(&db, PING, json!({})).await;
    let later = now() + Duration::days(2);
    let probe = Arc::new(Probe {
        behaviour: Behaviour::DeferTo(later),
        ..Probe::default()
    });
    let report = process_batch(&db, &registry(&probe), 10, 8, now())
        .await
        .unwrap();
    assert_eq!(report.deferred, 1);
    let (attempts, _, available_at, processed, dead, _) = state(&db, id).await;
    assert_eq!((attempts, processed, dead), (0, false, false));
    assert_eq!(available_at, later);
}

#[sqlx::test(migrations = "./migrations")]
async fn content_events_invalidate_their_cache_prefix_only(db: PgPool) {
    use mouzoun_api::{
        infra::cache::Cache, services::cache_invalidation::CacheInvalidationHandler,
    };
    dotenvy::dotenv().ok();
    let namespace = format!("test-{}", uuid::Uuid::now_v7());
    let cache = Cache::connect(&std::env::var("REDIS_URL").unwrap(), namespace)
        .await
        .unwrap();
    let ttl = std::time::Duration::from_secs(60);
    for key in [
        "catalog:product:tente-nomade:fr",
        "catalog:list:fr",
        "menus:header:fr",
    ] {
        cache.set_json(key, &json!(1), ttl).await;
    }
    let mut conn = db.acquire().await.unwrap();
    mouzoun_api::services::outbox::enqueue(
        &mut conn,
        &mouzoun_api::services::outbox::Event::ProductUpdated {
            product_id: uuid::Uuid::now_v7(),
        },
    )
    .await
    .unwrap();
    drop(conn);

    let registry = Registry::new().register(Arc::new(CacheInvalidationHandler::new(cache.clone())));
    assert_eq!(
        process_batch(&db, &registry, 10, 8, now())
            .await
            .unwrap()
            .processed,
        1
    );
    assert_eq!(
        cache
            .get_json::<i32>("catalog:product:tente-nomade:fr")
            .await,
        None
    );
    assert_eq!(cache.get_json::<i32>("catalog:list:fr").await, None);
    assert_eq!(
        cache.get_json::<i32>("menus:header:fr").await,
        Some(1),
        "other prefixes untouched"
    );
    cache.delete(["menus:header:fr"]).await;
}
