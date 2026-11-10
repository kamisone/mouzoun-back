//! Liveness / readiness probes (Docker, load balancers, uptime monitors).

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/health", get(ready))
}

/// The process is up. Never touches dependencies (a DB blip must not restart the container).
async fn live() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// The instance can serve traffic: database and cache reachable.
/// Storage is checked at startup only: probing GCS every few seconds costs money and latency.
async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let (db_ok, cache_ok) = tokio::join!(
        async { sqlx::query("SELECT 1").execute(&state.db).await.is_ok() },
        state.cache.ping(),
    );
    let ok = db_ok && cache_ok;
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(json!({
            "status": if ok { "ok" } else { "degraded" },
            "database": db_ok,
            "cache": cache_ok,
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}
