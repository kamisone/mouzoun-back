//! Readiness reports every dependency; only the database and the cache gate traffic.

use crate::common::TestApp;
use axum::http::{Method, StatusCode};
use sqlx::PgPool;

#[sqlx::test(migrations = "./migrations")]
async fn readiness_reports_database_cache_storage_and_payments(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, body) = app.request(Method::GET, "/health/ready", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["database"], true);
    assert_eq!(body["cache"], true);
    assert_eq!(body["storage"], true);
    // The test configuration uses the simulated provider.
    assert_eq!(body["payments"]["online"], true);
    assert_eq!(body["payments"]["provider"], "fake");
    assert!(body["build"]["version"].is_string());

    let (status, body) = app.request(Method::GET, "/health/live", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["version"].is_string());
}
