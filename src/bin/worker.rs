//! Standalone outbox worker (`cargo run --bin worker`): dispatches outbox events (emails,
//! cache invalidation) and plans scheduled emails. Same configuration as the API; run the API
//! with OUTBOX_WORKER_ENABLED=false when this runs separately (running both is safe, just
//! redundant). Stops gracefully on SIGTERM/Ctrl+C after finishing the batch in progress.

use mouzoun_api::{
    app,
    config::Config,
    infra::{cache::Cache, db},
    services::email::{EmailSettings, Mailer, sender},
    worker,
};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::install_crypto_provider();
    dotenvy::dotenv().ok();
    let config = Config::from_env()?;
    app::init_tracing(config.log_json);

    let pool = db::connect(&config.database_url, 5).await?;
    let cache = Cache::connect(&config.redis_url, "mouzoun").await?;
    let mailer = Mailer::new(
        pool.clone(),
        sender::from_config(&config.email)?,
        EmailSettings::from_config(&config),
    );
    tracing::info!(email = ?config.email, "outbox worker starting");

    let shutdown = CancellationToken::new();
    let tasks = worker::spawn(
        pool.clone(),
        worker::registry(&pool, &cache, &mailer),
        config.worker.clone(),
        shutdown.clone(),
    );

    wait_for_signal().await;
    tracing::info!("shutdown requested: finishing the current batch");
    shutdown.cancel();
    for task in tasks {
        let _ = task.await;
    }
    tracing::info!("outbox worker stopped");
    Ok(())
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
