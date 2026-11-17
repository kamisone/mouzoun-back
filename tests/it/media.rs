//! Media uploads: multipart to the API → checked → written to the bucket with its ACL → row.

use crate::common::{TestApp, png_bytes};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use mouzoun_api::infra::storage::ObjectAcl;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

const BOUNDARY: &str = "mouzoun-test-boundary";

/// `POST /v1/admin/media` as the admin, like the browser's `FormData`.
async fn upload(
    app: &TestApp,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
    visibility: Option<&str>,
) -> (StatusCode, Value) {
    let mut body = Vec::new();
    if let Some(v) = visibility {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"visibility\"\r\n\r\n{v}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/admin/media")
        .header(header::AUTHORIZATION, format!("Bearer {}", app.admin_token))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(Body::from(body))
        .unwrap();
    let res = app.router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn object_key(app: &TestApp, id: &str) -> String {
    sqlx::query_scalar("SELECT object_key FROM media_assets WHERE id = $1::uuid")
        .bind(id)
        .fetch_one(&app.db)
        .await
        .unwrap()
}

async fn asset_rows(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM media_assets")
        .fetch_one(&app.db)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn an_uploaded_image_is_stored_public_and_listed(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, media) = upload(&app, "dune.png", "image/png", &png_bytes(2048), None).await;
    assert_eq!(status, StatusCode::CREATED, "{media}");
    assert_eq!(media["kind"], "image");
    assert_eq!(media["visibility"], "public");
    assert_eq!(media["original_filename"], "dune.png");
    let id = media["id"].as_str().unwrap().to_string();
    let key = object_key(&app, &id).await;
    assert!(key.starts_with("media/") && key.ends_with(".png"));
    assert_eq!(media["url"], format!("https://cdn.test/{key}"));

    let stored = app.storage.object(&key).expect("written by the API");
    assert_eq!(stored.size, 2048);
    assert_eq!(stored.content_type, "image/png");
    assert_eq!(stored.acl, Some(ObjectAcl::PublicRead));
    assert_eq!(
        stored.cache_control.as_deref(),
        Some("public, max-age=31536000, immutable")
    );

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
async fn private_files_get_the_private_acl_and_signed_urls(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, media) = upload(
        &app,
        "passeport.pdf",
        "application/pdf",
        b"%PDF-1.7\n0123456",
        Some("private"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{media}");
    assert_eq!(media["visibility"], "private");
    let id = media["id"].as_str().unwrap().to_string();
    let key = object_key(&app, &id).await;
    assert_eq!(media["url"], format!("memory://download/{key}?signed=1"));

    let stored = app.storage.object(&key).unwrap();
    assert_eq!(stored.acl, Some(ObjectAcl::Private));
    assert_eq!(stored.cache_control.as_deref(), Some("private, no-store"));
    let visibility: String =
        sqlx::query_scalar("SELECT visibility FROM media_assets WHERE id = $1::uuid")
            .bind(&id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(visibility, "private");
}

#[sqlx::test(migrations = "./migrations")]
async fn refused_files_never_reach_the_bucket(db: PgPool) {
    let app = TestApp::new(db).await;
    let mut html = b"<html><script>alert(1)</script>".to_vec();
    html.resize(64, b' ');
    let too_big = png_bytes(1024 * 1024 + 1);
    // (filename, type, bytes, visibility): all refused before anything is stored.
    let cases: [(&str, &str, &[u8], Option<&str>); 5] = [
        ("x.png", "image/png", &html, None), // spoofed: HTML announced as PNG
        ("x.svg", "image/svg+xml", b"<svg/>", None),
        ("x.png", "image/png", b"", None),
        ("x.png", "image/png", &too_big, None),
        ("x.png", "image/png", &png_bytes(32), Some("everyone")),
    ];
    for (name, content_type, bytes, visibility) in cases {
        let (status, res) = upload(&app, name, content_type, bytes, visibility).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{name} {content_type} -> {res}"
        );
    }
    assert!(app.storage.is_empty(), "nothing written");
    assert_eq!(asset_rows(&app).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn bodies_over_the_route_limit_are_refused(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, res) = upload(
        &app,
        "x.png",
        "image/png",
        &png_bytes(2 * 1024 * 1024),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{res}");
    assert!(app.storage.is_empty());
}
