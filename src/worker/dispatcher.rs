//! Claims due outbox events and runs their handlers.
//!
//! Events are claimed `FOR NO KEY UPDATE SKIP LOCKED` inside one transaction per batch, so any
//! number of dispatchers (API instances, `worker` binaries) can run at once: a locked event is
//! skipped by the others and each event is handled by one dispatcher at a time.
//!
//! `NO KEY` matters: handlers write rows that reference the event (`email_log.outbox_event_id`)
//! on other connections, and those foreign keys take `FOR KEY SHARE`, which plain `FOR UPDATE`
//! would block until this transaction ends: a deadlock.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::Rng;
use sqlx::{PgConnection, PgPool};

use super::handler::{HandlerError, Registry};
use crate::{db::operations::OutboxEvent, services::outbox::SECRET_KEY};

/// Delay before attempt n+1, after n failed attempts (n = 1, 2, ...); the last value repeats.
pub const BACKOFF: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(6 * 60 * 60),
];
/// ±10 % random jitter, so failures after an outage do not all retry at the same second.
pub const JITTER: f64 = 0.10;

/// What one batch did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BatchReport {
    pub claimed: usize,
    pub processed: usize,
    pub retried: usize,
    pub dead: usize,
    pub deferred: usize,
}

pub fn backoff(failed_attempts: i32) -> Duration {
    let index = usize::try_from(failed_attempts.max(1) - 1).unwrap_or(0);
    BACKOFF[index.min(BACKOFF.len() - 1)]
}

fn jittered(delay: Duration) -> Duration {
    let factor = rand::rng().random_range((1.0 - JITTER)..=(1.0 + JITTER));
    delay.mul_f64(factor)
}

/// Handles up to `batch` due events. Returns what happened; `claimed < batch` means the queue
/// is (momentarily) drained.
pub async fn process_batch(
    pool: &PgPool,
    registry: &Registry,
    batch: i64,
    max_attempts: i32,
    now: DateTime<Utc>,
) -> sqlx::Result<BatchReport> {
    let mut tx = pool.begin().await?;
    let events = sqlx::query_as!(
        OutboxEvent,
        "SELECT id, aggregate_type, aggregate_id, event_type, payload, available_at, attempts,
                last_error, processed_at, failed_at, dedupe_key, created_at
         FROM outbox_events
         WHERE processed_at IS NULL AND failed_at IS NULL AND available_at <= $1
         ORDER BY id
         LIMIT $2
         FOR NO KEY UPDATE SKIP LOCKED",
        now,
        batch
    )
    .fetch_all(&mut *tx)
    .await?;

    let mut report = BatchReport {
        claimed: events.len(),
        ..BatchReport::default()
    };
    for event in &events {
        let handlers = registry.handlers(&event.event_type);
        if handlers.is_empty() {
            tracing::warn!(event_id = event.id, event_type = %event.event_type, "outbox: no handler for this event type, marked processed");
            mark_processed(&mut tx, event.id, now).await?;
            report.processed += 1;
            continue;
        }

        let mut outcome: Option<HandlerError> = None;
        for handler in handlers {
            if let Err(e) = handler.handle(event).await {
                tracing::debug!(event_id = event.id, handler = handler.name(), error = %e, "outbox: handler did not complete");
                outcome = Some(merge(outcome, e));
            }
        }

        match outcome {
            None => {
                mark_processed(&mut tx, event.id, now).await?;
                report.processed += 1;
            }
            Some(HandlerError::Defer(until)) => {
                sqlx::query!(
                    "UPDATE outbox_events SET available_at = $2 WHERE id = $1",
                    event.id,
                    until
                )
                .execute(&mut *tx)
                .await?;
                report.deferred += 1;
            }
            Some(HandlerError::Permanent(e)) => {
                mark_dead(&mut tx, event, &format!("{e:#}"), now).await?;
                report.dead += 1;
            }
            Some(HandlerError::Retry(e)) => {
                let attempts = event.attempts + 1;
                let message = format!("{e:#}");
                if attempts >= max_attempts {
                    mark_dead(&mut tx, event, &message, now).await?;
                    report.dead += 1;
                } else {
                    let delay = jittered(backoff(attempts));
                    let at = now + chrono::Duration::from_std(delay).unwrap_or_default();
                    sqlx::query!(
                        "UPDATE outbox_events SET attempts = $2, last_error = $3, available_at = $4 WHERE id = $1",
                        event.id,
                        attempts,
                        message,
                        at
                    )
                    .execute(&mut *tx)
                    .await?;
                    tracing::warn!(event_id = event.id, event_type = %event.event_type, attempts, retry_at = %at, error = %message, "outbox: event failed, will retry");
                    report.retried += 1;
                }
            }
        }
    }
    tx.commit().await?;
    Ok(report)
}

/// The most severe outcome wins: Permanent > Retry > Defer (earliest).
fn merge(current: Option<HandlerError>, new: HandlerError) -> HandlerError {
    use HandlerError::*;
    match (current, new) {
        (None, n) => n,
        (Some(Permanent(e)), _) | (_, Permanent(e)) => Permanent(e),
        (Some(Retry(e)), _) | (_, Retry(e)) => Retry(e),
        (Some(Defer(a)), Defer(b)) => Defer(a.min(b)),
    }
}

async fn mark_processed(tx: &mut PgConnection, id: i64, now: DateTime<Utc>) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE outbox_events SET processed_at = $2, last_error = NULL, payload = payload - $3::text WHERE id = $1",
        id,
        now,
        SECRET_KEY
    )
    .execute(tx)
    .await?;
    Ok(())
}

async fn mark_dead(
    tx: &mut PgConnection,
    event: &OutboxEvent,
    error: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE outbox_events
         SET failed_at = $2, attempts = attempts + 1, last_error = $3, payload = payload - $4::text
         WHERE id = $1",
        event.id,
        now,
        error,
        SECRET_KEY
    )
    .execute(tx)
    .await?;
    // `alert` lets log-based alerting pick dead events up.
    tracing::error!(alert = true, event_id = event.id, event_type = %event.event_type, error, "outbox: event is dead (replay with `email retry <id>` once fixed)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_follows_the_schedule_then_stays_at_six_hours() {
        let secs: Vec<u64> = (1..=7).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [30, 120, 600, 3600, 21_600, 21_600, 21_600]);
        for _ in 0..200 {
            let d = jittered(Duration::from_secs(100)).as_secs_f64();
            assert!((90.0..=110.0).contains(&d), "{d}");
        }
    }
}
