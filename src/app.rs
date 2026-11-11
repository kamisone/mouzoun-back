//! Composition root: wires infrastructure, services, HTTP server and background jobs,
//! and coordinates graceful shutdown.

use std::net::SocketAddr;

use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    config::Config,
    infra::{cache::Cache, db, storage},
    jobs, seed,
    state::AppState,
    web,
};

pub async fn run(config: Config) -> anyhow::Result<()> {
    let pool = db::connect(&config.database_url, config.database_max_connections).await?;
    if config.run_migrations {
        db::migrate(&pool).await?;
        tracing::info!("database connected, migrations applied");
    } else {
        tracing::info!(
            "database connected (DATABASE_RUN_MIGRATIONS=false: migrations not applied)"
        );
    }

    let cache = Cache::connect(&config.redis_url, "mouzoun").await?;
    tracing::info!("redis connected");

    tracing::info!(storage = ?config.storage, "connecting object storage");
    let buckets = storage::gcs::connect(&config.storage).await?;
    // Network check is advisory: a transient GCS issue must not prevent the API from booting.
    match buckets.public.check().await {
        Ok(()) => tracing::info!("object storage reachable"),
        Err(e) => tracing::error!(error = %e, "object storage check failed (uploads will fail)"),
    }

    seed::ensure_admin(&pool, &config).await?;

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let state = AppState::new(config, pool, cache, buckets);

    let shutdown = CancellationToken::new();
    let workers = jobs::spawn_all(&state, shutdown.clone());

    let listener = TcpListener::bind(addr).await?;
    tracing::info!("mouzoun-api listening on http://{addr}");
    axum::serve(listener, web::router(state))
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await?;

    shutdown.cancel();
    for worker in workers {
        let _ = worker.await;
    }
    tracing::info!("shutdown complete");
    Ok(())
}

/// Two TLS crypto backends end up compiled in (`ring` via sqlx, `aws-lc-rs` via the Google SDK),
/// so rustls cannot pick one on its own and would panic on the first TLS handshake.
/// Must run once, before any TLS client is built.
pub fn install_crypto_provider() {
    // Err only means a provider is already installed, which is fine.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("mouzoun_api=info,tower_http=info"));
    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json())
            .init();
    } else {
        registry.with(tracing_subscriber::fmt::layer()).init();
    }
}

async fn shutdown_signal(token: CancellationToken) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = token.cancelled() => {},
    }
    tracing::info!("shutdown signal received, draining connections");
    token.cancel();
}
