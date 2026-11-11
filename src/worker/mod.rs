//! Outbox worker: dispatches outbox events to their handlers (emails, cache invalidation) and
//! plans scheduled emails.
//!
//! Runs inside the API (`OUTBOX_WORKER_ENABLED=true`, default) or alone (`cargo run --bin
//! worker`); any number of instances can run at once. It wakes up instantly on
//! `NOTIFY outbox_events` (trigger on insert) and also polls every `OUTBOX_POLL_SECS`.
//! On shutdown it finishes the batch in progress, then exits.

pub mod dispatcher;
pub mod handler;
pub mod scheduler;

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use sqlx::{PgPool, postgres::PgListener};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use dispatcher::{BatchReport, process_batch};
pub use handler::{HandlerError, OutboxHandler, Registry};

use crate::{
    config::WorkerConfig,
    infra::cache::Cache,
    services::{
        cache_invalidation::CacheInvalidationHandler, email::Mailer, email::handler::EmailHandler,
    },
};

pub const CHANNEL: &str = "outbox_events";

/// The production handlers.
pub fn registry(pool: &PgPool, cache: &Cache, mailer: &Mailer) -> Registry {
    Registry::new()
        .register(Arc::new(EmailHandler::new(pool.clone(), mailer.clone())))
        .register(Arc::new(CacheInvalidationHandler::new(cache.clone())))
}

/// Starts the dispatcher and the scheduler; both stop when `shutdown` is cancelled.
pub fn spawn(
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
    shutdown: CancellationToken,
) -> Vec<JoinHandle<()>> {
    let scheduler = {
        let (pool, shutdown, period) = (pool.clone(), shutdown.clone(), config.scheduler_interval);
        tokio::spawn(async move { run_scheduler(pool, period, shutdown).await })
    };
    let dispatcher =
        tokio::spawn(
            async move { run_dispatcher(pool, Arc::new(registry), config, shutdown).await },
        );
    vec![dispatcher, scheduler]
}

async fn run_dispatcher(
    pool: PgPool,
    registry: Arc<Registry>,
    config: WorkerConfig,
    shutdown: CancellationToken,
) {
    let mut listener = match listen(&pool).await {
        Ok(l) => Some(l),
        Err(e) => {
            tracing::warn!(error = %e, "outbox: LISTEN failed, polling only");
            None
        }
    };
    tracing::info!(
        batch = config.batch_size,
        max_attempts = config.max_attempts,
        "outbox dispatcher started"
    );
    loop {
        // Drain everything due; shutdown is only checked between batches, so a batch in progress
        // always completes.
        loop {
            match process_batch(
                &pool,
                &registry,
                config.batch_size,
                config.max_attempts,
                Utc::now(),
            )
            .await
            {
                Ok(report)
                    if report.claimed as i64 >= config.batch_size && !shutdown.is_cancelled() =>
                {
                    continue;
                }
                Ok(_) => break,
                Err(e) => {
                    tracing::error!(error = %e, "outbox: batch failed");
                    break;
                }
            }
        }
        let woke = async {
            match listener.as_mut() {
                Some(l) => {
                    if let Err(e) = l.recv().await {
                        tracing::warn!(error = %e, "outbox: notification channel lost, reconnecting");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = woke => {}
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
    tracing::info!("outbox dispatcher stopped");
}

async fn listen(pool: &PgPool) -> sqlx::Result<PgListener> {
    let mut listener = PgListener::connect_with(pool).await?;
    listener.listen(CHANNEL).await?;
    Ok(listener)
}

async fn run_scheduler(pool: PgPool, period: Duration, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {
                if let Err(e) = scheduler::plan(&pool, Utc::now()).await {
                    tracing::warn!(error = %e, "email scheduler failed");
                }
            }
        }
    }
}
