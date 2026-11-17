//! The "Générer" translations endpoint against a local mock of the translation service.

use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{Method, StatusCode},
    routing::get,
};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::common::TestApp;

type Calls = Arc<Mutex<Vec<(String, String)>>>;

/// Answers `[[["<tl>:<q>", q]]]`; Arabic fails with 429 when `fail_ar`.
async fn mock(fail_ar: bool) -> (String, Calls) {
    let calls: Calls = Arc::default();
    async fn handle(
        State((calls, fail_ar)): State<(Calls, bool)>,
        Query(q): Query<std::collections::HashMap<String, String>>,
    ) -> Result<Json<Value>, StatusCode> {
        let (tl, text) = (q["tl"].clone(), q["q"].clone());
        calls.lock().unwrap().push((tl.clone(), text.clone()));
        if fail_ar && tl == "ar" {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
        // Like the real service: translated text, tokens kept, outer spaces trimmed.
        Ok(Json(json!([
            [[format!("{tl}:{}", text.trim()), text, null]],
            null,
            q["sl"]
        ])))
    }
    let router = Router::new()
        .route("/translate_a/single", get(handle))
        .with_state((calls.clone(), fail_ar));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), calls)
}

#[sqlx::test(migrations = "./migrations")]
async fn generate_fills_every_other_locale_from_french(db: PgPool) {
    let (base, calls) = mock(false).await;
    let app = TestApp::with_config(db, |c| c.translate.api_base = base).await;
    let (status, out) = app
        .admin(
            Method::POST,
            "/v1/admin/translations/generate",
            Some(json!({
                "fields": { "name": "Dattes Medjool", "description": "Moelleuses.\n\nDe notre palmeraie.", "subtitle": "" },
                "html": ["body"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["errors"], json!({}));
    assert_eq!(out["result"]["en"]["name"], "en:Dattes Medjool");
    assert_eq!(out["result"]["ar"]["name"], "ar:Dattes Medjool");
    // Paragraphs translated one by one, blank lines kept; empty sources stay empty.
    assert_eq!(
        out["result"]["en"]["description"],
        "en:Moelleuses.\n\nen:De notre palmeraie."
    );
    assert_eq!(out["result"]["en"]["subtitle"], "");
    assert!(
        out["result"].get("fr").is_none(),
        "the source locale is not a target"
    );
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .all(|(tl, _)| tl == "en" || tl == "ar")
    );

    // HTML: block by block, inline tags as numeric tokens.
    let (status, out) = app
        .admin(
            Method::POST,
            "/v1/admin/translations/generate",
            Some(json!({ "fields": { "body": "<p>Une <strong>datte</strong> douce</p><ul><li>Bio</li></ul>" }, "html": ["body"], "targets": ["en"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["result"]["en"]["body"],
        "<p>en:Une <strong>datte</strong> douce</p><ul><li>en:Bio</li></ul>"
    );
    assert!(out["result"].get("ar").is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_failing_language_is_reported_without_losing_the_others(db: PgPool) {
    let (base, _) = mock(true).await;
    let app = TestApp::with_config(db, |c| c.translate.api_base = base).await;
    let (status, out) = app
        .admin(
            Method::POST,
            "/v1/admin/translations/generate",
            Some(json!({ "fields": { "name": "Huile d'olive" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["result"]["en"]["name"], "en:Huile d'olive");
    assert_eq!(out["result"]["ar"]["name"], "");
    assert!(
        out["errors"]["ar"]
            .as_str()
            .unwrap()
            .contains("rate limited")
    );

    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/translations/generate",
            Some(json!({ "fields": { "name": "  " } })),
        )
        .await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("source_empty"))
    );
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/admin/translations/generate",
            Some(json!({ "fields": { "name": "x" } })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "staff only");
}
