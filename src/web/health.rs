//! Liveness / readiness probes (Docker, the deploy health gate, uptime monitors).

use std::time::{Duration, Instant};

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;

use crate::{services::payments::status::PaymentStatus, state::AppState};

/// Object storage is probed at most this often: each probe is a paid call to the provider.
const STORAGE_PROBE_EVERY: Duration = Duration::from_secs(300);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
}

#[derive(Serialize)]
struct Build {
    version: &'static str,
    /// Commit of the running image (`GIT_SHA` build argument), to tell an old build at a glance.
    commit: Option<&'static str>,
}

fn build() -> Build {
    Build {
        version: env!("CARGO_PKG_VERSION"),
        commit: option_env!("GIT_SHA").filter(|sha| !sha.is_empty()),
    }
}

/// The process is up. Never touches dependencies (a DB blip must not restart the container).
async fn live() -> Json<Build> {
    Json(build())
}

#[derive(Serialize)]
struct Readiness {
    /// `ok`: can serve traffic; `degraded`: database or cache unreachable (HTTP 503).
    status: &'static str,
    database: bool,
    cache: bool,
    /// Uploads and images (a failure does not stop traffic: pages still render).
    storage: bool,
    /// Online payment (not configured is a valid state: pay at the camp only).
    payments: PaymentSummary,
    build: Build,
}

/// The public part of the payment status: no counts or dates.
#[derive(Serialize)]
struct PaymentSummary {
    online: bool,
    provider: Option<&'static str>,
}

async fn storage_ok(state: &AppState) -> bool {
    let mut probe = state.storage_probe.lock().await;
    if let Some((at, ok)) = *probe
        && at.elapsed() < STORAGE_PROBE_EVERY
    {
        return ok;
    }
    let ok = state.media.check_storage().await;
    *probe = Some((Instant::now(), ok));
    ok
}

/// Ready when the database and the cache answer. Storage and payments are reported, not gating.
async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Readiness>) {
    let (database, cache, storage) = tokio::join!(
        async { sqlx::query("SELECT 1").execute(&state.db).await.is_ok() },
        state.cache.ping(),
        storage_ok(&state),
    );
    let payments = PaymentStatus::from_config(&state.config.booking);
    let ok = database && cache;
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(Readiness {
            status: if ok { "ok" } else { "degraded" },
            database,
            cache,
            storage,
            payments: PaymentSummary {
                online: payments.online,
                provider: payments.provider,
            },
            build: build(),
        }),
    )
}
