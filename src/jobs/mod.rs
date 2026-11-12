//! Background workers. Each runs until the shutdown token is cancelled. All of them are safe to
//! run on several API instances at once (row locks with SKIP LOCKED, idempotent inserts).

mod booking_holds;
pub mod booking_ops;
mod inventory;
mod media_cleanup;

use std::{future::Future, time::Duration};

use tokio::{
    task::JoinHandle,
    time::{MissedTickBehavior, interval},
};
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

pub fn spawn_all(state: &AppState, shutdown: CancellationToken) -> Vec<JoinHandle<()>> {
    vec![
        every(
            "media cleanup",
            state.config.media.cleanup_interval,
            shutdown.clone(),
            {
                let media = state.media.clone();
                move || media_cleanup::run_once(media.clone())
            },
        ),
        every(
            "booking holds",
            Duration::from_secs(60),
            shutdown.clone(),
            {
                let db = state.db.clone();
                move || booking_holds::run_once(db.clone())
            },
        ),
        every(
            "booking operations",
            Duration::from_secs(3600),
            shutdown.clone(),
            {
                let (db, payments) = (state.db.clone(), state.payments.clone());
                move || booking_ops::run_once(db.clone(), payments.clone())
            },
        ),
        every("inventory", Duration::from_secs(24 * 3600), shutdown, {
            let db = state.db.clone();
            move || inventory::run_once(db.clone())
        }),
    ]
}

/// Runs `task` immediately, then every `period`, until shutdown. Errors are logged, never fatal.
fn every<F, Fut>(
    name: &'static str,
    period: Duration,
    shutdown: CancellationToken,
    task: F,
) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send,
{
    tokio::spawn(async move {
        let mut ticker = interval(period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(e) = task().await {
                        tracing::warn!(job = name, error = ?e, "background job failed");
                    }
                }
            }
        }
        tracing::debug!(job = name, "background job stopped");
    })
}
