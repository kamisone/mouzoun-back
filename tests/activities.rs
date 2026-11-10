mod common;

use axum::http::{Method, StatusCode};
use common::{TestApp, png_bytes};
use serde_json::{Value, json};
use sqlx::PgPool;

fn activity(slug: &str, cover: Option<&str>, published: bool) -> Value {
    json!({
        "slug": slug,
        "title_fr": "Nuit sous les étoiles",
        "title_en": "Night under the stars",
        "description_fr": "Bivouac",
        "description_en": "Bivouac",
        "price_mad": 650,
        "duration_minutes": 720,
        "cover_media_id": cover,
        "is_published": published,
    })
}

async fn ready_image(app: &TestApp) -> (String, String) {
    let (_, ticket) = app
        .admin(
            Method::POST,
            "/v1/media/uploads",
            Some(
                json!({ "filename": "cover.png", "content_type": "image/png", "size_bytes": 128 }),
            ),
        )
        .await;
    let id = ticket["media"]["id"].as_str().unwrap().to_string();
    let key = ticket["upload"]["url"]
        .as_str()
        .unwrap()
        .trim_start_matches("memory://upload/")
        .to_string();
    app.storage.put(&key, png_bytes(128), "image/png");
    let (status, _) = app
        .admin(Method::POST, &format!("/v1/media/{id}/complete"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    (id, key)
}

#[sqlx::test(migrations = "./migrations")]
async fn activity_with_cover_lifecycle(db: PgPool) {
    let app = TestApp::new(db).await;
    let (media_id, key) = ready_image(&app).await;

    let (status, created) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("nuit-desert", Some(&media_id), true)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(
        created["cover_image_url"],
        format!("https://cdn.test/{key}")
    );
    let id = created["id"].as_str().unwrap().to_string();

    // Public catalogue (fills the cache)...
    let (status, list) = app.request(Method::GET, "/v1/activities", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (status, one) = app
        .request(Method::GET, "/v1/activities/nuit-desert", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["cover_media_id"], media_id.as_str());

    // ...and updates invalidate it, including the old slug.
    let mut changed = activity("nuit-sahara", Some(&media_id), true);
    changed["price_mad"] = json!(700);
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("/v1/activities/id/{id}"),
            Some(changed),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, list) = app.request(Method::GET, "/v1/activities", None, None).await;
    assert_eq!(list[0]["price_mad"], 700);
    let (status, _) = app
        .request(Method::GET, "/v1/activities/nuit-desert", None, None)
        .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "stale slug must not be served from cache"
    );

    // A media in use cannot be deleted.
    let (status, body) = app
        .admin(Method::DELETE, &format!("/v1/media/{media_id}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(app.storage.contains(&key));

    // Once the activity is gone, it can.
    let (status, _) = app
        .admin(Method::DELETE, &format!("/v1/activities/id/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = app
        .admin(Method::DELETE, &format!("/v1/media/{media_id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(app.storage.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn cover_must_be_a_ready_public_image(db: PgPool) {
    let app = TestApp::new(db).await;
    let (_, ticket) = app
        .admin(
            Method::POST,
            "/v1/media/uploads",
            Some(json!({ "filename": "c.png", "content_type": "image/png", "size_bytes": 10 })),
        )
        .await;
    let pending_id = ticket["media"]["id"].as_str().unwrap();

    let (status, _) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("a-1", Some(pending_id), true)),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "pending media cannot be a cover"
    );

    let unknown = uuid::Uuid::new_v4().to_string();
    let (status, _) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("a-2", Some(&unknown), true)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("a-3", None, true)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "./migrations")]
async fn drafts_are_hidden_and_slugs_unique(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, _) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("brouillon", None, false)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, list) = app.request(Method::GET, "/v1/activities", None, None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    let (status, _) = app
        .request(Method::GET, "/v1/activities/brouillon", None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("brouillon", None, true)),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = app
        .admin(
            Method::POST,
            "/v1/activities",
            Some(activity("Bad Slug", None, true)),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, page) = app
        .admin(Method::GET, "/v1/activities/admin?per_page=500", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["per_page"], 100, "page size is capped");
}

#[sqlx::test(migrations = "./migrations")]
async fn errors_use_the_standard_json_shape(db: PgPool) {
    let app = TestApp::new(db).await;
    let (status, body) = app
        .request(Method::GET, "/v1/activities/nope", None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    let (status, body) = app.request(Method::GET, "/health/live", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = app.request(Method::GET, "/health/ready", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
