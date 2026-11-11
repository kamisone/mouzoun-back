//! Media uploads: Redis ticket → direct upload → verified `media_assets` row.

mod common;

use axum::http::{Method, StatusCode};
use common::{TestApp, png_bytes};
use serde_json::{Value, json};
use sqlx::PgPool;

fn object_key(ticket: &Value) -> String {
    ticket["upload"]["url"]
        .as_str()
        .unwrap()
        .trim_start_matches("memory://upload/")
        .to_string()
}

async fn start_upload(app: &TestApp, size: usize) -> (String, String, Value) {
    let (status, ticket) = app
        .admin(
            Method::POST,
            "/v1/admin/media/uploads",
            Some(
                json!({ "filename": "dune.png", "content_type": "image/png", "size_bytes": size }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{ticket}");
    (
        ticket["media_id"].as_str().unwrap().to_string(),
        object_key(&ticket),
        ticket,
    )
}

async fn asset_rows(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM media_assets")
        .fetch_one(&app.db)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_verified_upload_becomes_a_media_asset(db: PgPool) {
    let app = TestApp::new(db).await;
    let (id, key, ticket) = start_upload(&app, 2048).await;

    let headers = &ticket["upload"]["headers"];
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-goog-content-length-range"], "2048,2048");
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert!(key.starts_with("media/") && key.ends_with(".png"));
    assert_eq!(
        asset_rows(&app).await,
        0,
        "nothing in the table until the file is verified"
    );

    let (status, _) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/media/{id}/complete"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "file not uploaded yet");

    app.storage.put(&key, png_bytes(2048), "image/png");
    let (status, media) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/media/{id}/complete"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{media}");
    assert_eq!(media["kind"], "image");
    assert_eq!(media["visibility"], "public");
    assert_eq!(media["url"], format!("https://cdn.test/{key}"));

    let (status, again) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/media/{id}/complete"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "idempotent");
    assert_eq!(again["id"], media["id"]);

    let (bucket, size, kind): (String, i64, String) =
        sqlx::query_as("SELECT bucket, size_bytes, kind FROM media_assets WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        (bucket.as_str(), size, kind.as_str()),
        ("memory-bucket", 2048, "image")
    );

    let (status, list) = app
        .admin(Method::GET, "/v1/admin/media?kind=image", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1);

    let (status, _) = app
        .admin(Method::DELETE, &format!("/v1/admin/media/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = app
        .admin(Method::GET, &format!("/v1/admin/media/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "soft-deleted");
    assert!(
        app.storage.contains(&key),
        "soft delete keeps the file (restorable)"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn spoofed_or_resized_content_is_rejected_and_removed(db: PgPool) {
    let app = TestApp::new(db).await;
    let (id, key, _) = start_upload(&app, 64).await;
    let mut html = b"<html><script>alert(1)</script>".to_vec();
    html.resize(64, b' ');
    app.storage.put(&key, html, "image/png");
    let (status, _) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/media/{id}/complete"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!app.storage.contains(&key), "rejected file deleted");
    let (status, _) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/media/{id}/complete"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "ticket forgotten");

    let (id, key, _) = start_upload(&app, 100).await;
    app.storage.put(&key, png_bytes(90), "image/png");
    let (status, _) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/media/{id}/complete"),
            None,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "size differs from the declaration"
    );
    assert_eq!(asset_rows(&app).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn upload_requests_are_validated(db: PgPool) {
    let app = TestApp::new(db).await;
    for (body, expected) in [
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
        (
            json!({ "filename": "x.pdf", "content_type": "application/pdf", "size_bytes": 10, "visibility": "private" }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (status, res) = app
            .admin(Method::POST, "/v1/admin/media/uploads", Some(body.clone()))
            .await;
        assert_eq!(status, expected, "{body} -> {res}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn private_files_are_served_through_signed_urls(db: PgPool) {
    let app = TestApp::with_private_bucket(db, true).await;
    let (status, ticket) = app
        .admin(
            Method::POST,
            "/v1/admin/media/uploads",
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
async fn abandoned_uploads_are_purged(db: PgPool) {
    let app = TestApp::new(db).await;
    let (_, key, _) = start_upload(&app, 32).await;
    app.storage.put(&key, png_bytes(32), "image/png"); // uploaded, never completed

    assert_eq!(
        app.state.media.purge_abandoned(100).await.unwrap(),
        0,
        "not expired yet"
    );
    // Expire the ticket: move its deadline to the past in the pending index.
    let mut redis = app.state.cache.connection();
    let index = app.state.cache.key("media:uploads:pending");
    let members: Vec<String> = redis::cmd("ZRANGE")
        .arg(&index)
        .arg(0)
        .arg(-1)
        .query_async(&mut redis)
        .await
        .unwrap();
    let _: () = redis::cmd("ZADD")
        .arg(&index)
        .arg(0)
        .arg(&members[0])
        .query_async(&mut redis)
        .await
        .unwrap();

    assert_eq!(app.state.media.purge_abandoned(100).await.unwrap(), 1);
    assert!(!app.storage.contains(&key));
    assert_eq!(
        app.state.media.purge_abandoned(100).await.unwrap(),
        0,
        "idempotent"
    );
}
