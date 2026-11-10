mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{TestApp, png_bytes};
use serde_json::{Value, json};
use sqlx::PgPool;

fn object_key_of(url: &str) -> String {
    // memory://upload/<key>
    url.trim_start_matches("memory://upload/").to_string()
}

async fn start_upload(app: &TestApp, size: usize) -> (String, String, Value) {
    let (status, body) = app
        .admin(
            Method::POST,
            "/v1/media/uploads",
            Some(
                json!({ "filename": "dune.png", "content_type": "image/png", "size_bytes": size }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["media"]["id"].as_str().unwrap().to_string();
    let key = object_key_of(body["upload"]["url"].as_str().unwrap());
    (id, key, body)
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_flow_publishes_a_verified_image(db: PgPool) {
    let app = TestApp::new(db).await;
    let (id, key, ticket) = start_upload(&app, 2048).await;

    // The signature pins type, exact size and immutable caching.
    let headers = &ticket["upload"]["headers"];
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-goog-content-length-range"], "2048,2048");
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(ticket["media"]["status"], "pending");
    assert!(ticket["media"]["url"].is_null());
    assert!(key.starts_with("media/") && key.ends_with(".png"));

    // Completing before the upload landed is refused.
    let (status, _) = app
        .admin(Method::POST, &format!("/v1/media/{id}/complete"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    app.storage.put(&key, png_bytes(2048), "image/png");
    let (status, media) = app
        .admin(Method::POST, &format!("/v1/media/{id}/complete"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{media}");
    assert_eq!(media["status"], "ready");
    assert_eq!(media["url"], format!("https://cdn.test/{key}"));

    // Idempotent.
    let (status, again) = app
        .admin(Method::POST, &format!("/v1/media/{id}/complete"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["url"], media["url"]);

    let (status, list) = app.admin(Method::GET, "/v1/media", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn spoofed_content_is_rejected_and_removed(db: PgPool) {
    let app = TestApp::new(db).await;
    let (id, key, _) = start_upload(&app, 64).await;

    // Declared PNG, actually HTML.
    let mut html = b"<html><script>alert(1)</script>".to_vec();
    html.resize(64, b' ');
    app.storage.put(&key, html, "image/png");

    let (status, body) = app
        .admin(Method::POST, &format!("/v1/media/{id}/complete"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        !app.storage.contains(&key),
        "rejected object must be deleted"
    );
    let (status, _) = app
        .admin(Method::GET, &format!("/v1/media/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "./migrations")]
async fn size_mismatch_is_rejected(db: PgPool) {
    let app = TestApp::new(db).await;
    let (id, key, _) = start_upload(&app, 100).await;
    app.storage.put(&key, png_bytes(90), "image/png");
    let (status, _) = app
        .admin(Method::POST, &format!("/v1/media/{id}/complete"), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_requests_are_validated(db: PgPool) {
    let app = TestApp::new(db).await;
    let cases = [
        (
            json!({ "filename": "x.svg", "content_type": "image/svg+xml", "size_bytes": 10 }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "filename": "x.png", "content_type": "image/png", "size_bytes": 10 * 1024 * 1024 }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "filename": "x.png", "content_type": "image/png", "size_bytes": 0 }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            json!({ "filename": "", "content_type": "image/png", "size_bytes": 10 }),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        // No private bucket configured in this app.
        (
            json!({ "filename": "x.pdf", "content_type": "application/pdf", "size_bytes": 10, "visibility": "private" }),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (body, expected) in cases {
        let (status, res) = app
            .admin(Method::POST, "/v1/media/uploads", Some(body.clone()))
            .await;
        assert_eq!(status, expected, "{body} -> {res}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn private_media_get_signed_urls(db: PgPool) {
    let app = TestApp::with_private_bucket(db, true).await;
    let (status, ticket) = app
        .admin(
            Method::POST,
            "/v1/media/uploads",
            Some(json!({ "filename": "facture.pdf", "content_type": "application/pdf", "size_bytes": 10, "visibility": "private" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ticket}");
    assert!(
        ticket["upload"]["headers"].get("cache-control").is_none(),
        "private files are not CDN-cached"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn media_endpoints_require_staff(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, _) = app.request(Method::GET, "/v1/media", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let customer_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, password_hash, full_name, role) VALUES ('c@test.ma','x','C','customer') RETURNING id",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    let token = mouzoun_api::auth::jwt::issue(
        common::JWT_SECRET,
        customer_id,
        mouzoun_api::auth::Role::Customer,
        60,
    )
    .unwrap();
    let (status, _) = app
        .request(Method::GET, "/v1/media", None, Some(&token))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn abandoned_uploads_are_purged(db: PgPool) {
    let app = TestApp::new(db).await;
    let (old_id, old_key, _) = start_upload(&app, 32).await;
    let (fresh_id, _, _) = start_upload(&app, 32).await;
    app.storage.put(&old_key, png_bytes(32), "image/png"); // uploaded but never completed

    sqlx::query("UPDATE media SET created_at = NOW() - INTERVAL '2 days' WHERE id = $1::uuid")
        .bind(&old_id)
        .execute(&app.db)
        .await
        .unwrap();

    let purged = app
        .state
        .media
        .purge_abandoned(Duration::from_secs(24 * 3600), 100)
        .await
        .unwrap();
    assert_eq!(purged, 1);
    assert!(!app.storage.contains(&old_key));
    let remaining: Vec<String> = sqlx::query_scalar("SELECT id::text FROM media")
        .fetch_all(&app.db)
        .await
        .unwrap();
    assert_eq!(remaining, vec![fresh_id]);
}
