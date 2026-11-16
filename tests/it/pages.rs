//! Page builder: draft saves, publishing, revisions, shared sections, public rendering.

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, TimeZone, Utc};
use mouzoun_api::{auth::jwt, modules::pages::service};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{JWT_SECRET, TestApp, create_user};

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2027, 1, 1, 10, 0, 0).unwrap()
}

async fn new_page(app: &TestApp, page_type: &str, slug: &str) -> Uuid {
    let (status, body) = app
        .admin(
            Method::POST,
            "/v1/admin/pages",
            Some(json!({"page_type": page_type, "translations": {
                "fr": {"title": "Accueil", "slug": slug},
                "ar": {"title": "الرئيسية", "slug": format!("{slug}-ar")}
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn tree(app: &TestApp, page: Uuid) -> Value {
    let (status, body) = app
        .admin(
            Method::GET,
            &format!("/v1/admin/pages/{page}/builder"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn save_as(
    app: &TestApp,
    token: &str,
    page: Uuid,
    version: i64,
    blocks: Value,
) -> (StatusCode, Value) {
    let (status, _, body) = app
        .send(
            Method::PUT,
            &format!("/v1/admin/pages/{page}/builder"),
            Some(json!({ "blocks": blocks })),
            &[
                ("authorization", format!("Bearer {token}")),
                ("if-match", format!("\"{version}\"")),
            ],
        )
        .await;
    (status, body)
}

async fn save(app: &TestApp, page: Uuid, version: i64, blocks: Value) -> (StatusCode, Value) {
    let token = app.admin_token.clone();
    save_as(app, &token, page, version, blocks).await
}

/// Saves and asserts success; returns the new tree.
async fn save_ok(app: &TestApp, page: Uuid, blocks: Value) -> Value {
    let version = tree(app, page).await["draft_version"].as_i64().unwrap();
    let (status, body) = save(app, page, version, blocks).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

fn section(code: &str, settings: Value, content: Value) -> Value {
    json!({ "id": Uuid::now_v7(), "type": code, "settings": settings, "content": content })
}

fn rich(title: &str) -> Value {
    section(
        "rich_text",
        json!({"max_width": "normal"}),
        json!({"fr": {"title": title, "body": "<p>Texte</p>"}}),
    )
}

async fn publish(app: &TestApp, page: Uuid, body: Value) -> (StatusCode, Value) {
    app.admin(
        Method::POST,
        &format!("/v1/admin/pages/{page}/publish"),
        Some(body),
    )
    .await
}

async fn media(db: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO media_assets (id, bucket, object_key, original_filename, mime_type, kind, size_bytes, width, height)
         VALUES ($1, 'test-bucket', $2, 'dunes.jpg', 'image/jpeg', 'image', 10, 1600, 900)",
    )
    .bind(id)
    .bind(format!("media/{id}.jpg"))
    .execute(db)
    .await
    .unwrap();
    id
}

async fn product(db: &PgPool, kind: &str, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO products (id, kind, status, published_at) VALUES ($1, $2, 'published', now())",
    )
    .bind(id)
    .bind(kind)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, 'fr', $2, $3)")
        .bind(id)
        .bind(name)
        .bind(format!("p-{id}"))
        .execute(db)
        .await
        .unwrap();
    id
}

async fn public(app: &TestApp, uri: &str) -> (StatusCode, Value) {
    app.request(Method::GET, uri, None, None).await
}

fn titles(page: &Value) -> Vec<String> {
    page["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            s["content"]["title"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

// ---- Builder ---------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_stale_draft_version_is_refused_with_the_current_one(db: PgPool) {
    let app = TestApp::new(db).await;
    let page = new_page(&app, "home", "accueil").await;
    let first = tree(&app, page).await["draft_version"].as_i64().unwrap();
    let (status, body) = save(&app, page, first, json!([rich("Un")])).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["draft_version"], json!(first + 1));

    // Another tab still edits version `first`.
    let (status, body) = save(&app, page, first, json!([rich("Deux")])).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "draft_conflict");
    assert_eq!(
        body["error"]["details"]["current_version"],
        json!(first + 1)
    );
    assert_eq!(body["error"]["details"]["saved_by"]["name"], "User");

    // Without If-Match: 428.
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/pages/{page}/builder"),
            Some(json!({"blocks": []})),
        )
        .await;
    assert_eq!(status, StatusCode::PRECONDITION_REQUIRED);
}

#[sqlx::test(migrations = "./migrations")]
async fn reorder_and_delete_keep_the_order_contiguous(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "page", "a-propos").await;
    let (a, b, c) = (rich("A"), rich("B"), rich("C"));
    save_ok(&app, page, json!([a, b, c])).await;
    // move C first, delete B
    let body = save_ok(&app, page, json!([c, a])).await;
    let ids: Vec<&str> = body["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [c["id"].as_str().unwrap(), a["id"].as_str().unwrap()]);
    let rows: Vec<(Uuid, i32, bool)> = sqlx::query_as(
        "SELECT id, sort_order, deleted_at IS NOT NULL FROM content_blocks WHERE page_id = $1 ORDER BY deleted_at NULLS FIRST, sort_order",
    )
    .bind(page)
    .fetch_all(&db)
    .await
    .unwrap();
    let live: Vec<i32> = rows.iter().filter(|r| !r.2).map(|r| r.1).collect();
    assert_eq!(live, [0, 1]);
    assert_eq!(rows.iter().filter(|r| r.2).count(), 1, "B is soft-deleted");

    // the small operations keep it contiguous too
    let version = body["draft_version"].as_i64().unwrap();
    let (status, _, body) = app
        .send(
            Method::POST,
            &format!("/v1/admin/blocks/{}/duplicate", a["id"].as_str().unwrap()),
            None,
            &[
                ("authorization", format!("Bearer {}", app.admin_token)),
                ("if-match", version.to_string()),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = app
        .admin(
            Method::DELETE,
            &format!("/v1/admin/blocks/{}", c["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["blocks"].as_array().unwrap().len(), 2);
    let orders: Vec<i32> = sqlx::query_scalar("SELECT sort_order FROM content_blocks WHERE page_id = $1 AND deleted_at IS NULL ORDER BY 1")
        .bind(page)
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(orders, [0, 1]);

    // undo: sending the deleted section again brings it back
    let mut blocks = body["blocks"].as_array().unwrap().clone();
    blocks.insert(0, c.clone());
    let body = save_ok(&app, page, Value::Array(blocks)).await;
    assert_eq!(body["blocks"][0]["id"], c["id"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn references_are_rebuilt_on_every_save(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    let (m1, m2) = (media(&db).await, media(&db).await);
    let hero_id = Uuid::now_v7();
    let hero = |m: Uuid| json!({"id": hero_id, "type": "hero", "settings": {"media_id": m}, "content": {"fr": {"title": "Désert"}}});
    save_ok(&app, page, json!([hero(m1)])).await;
    let refs = |db: PgPool| async move {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT ref_id FROM content_block_refs WHERE block_id = $1 AND ref_type = 'media'",
        )
        .bind(hero_id)
        .fetch_all(&db)
        .await
        .unwrap()
    };
    assert_eq!(refs(db.clone()).await, [m1]);
    save_ok(&app, page, json!([hero(m2)])).await;
    assert_eq!(refs(db.clone()).await, [m2]);

    let (status, body) = app
        .admin(Method::GET, &format!("/v1/admin/usages/media/{m2}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["usages"][0]["block_id"], json!(hero_id));
    assert_eq!(body["usages"][0]["live"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_media_used_by_a_published_section_cannot_be_deleted(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    let (used, free) = (media(&db).await, media(&db).await);
    save_ok(&app, page, json!([{"id": Uuid::now_v7(), "type": "hero", "settings": {"media_id": used}, "content": {"fr": {"title": "T"}}}])).await;
    let (status, body) = publish(&app, page, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = app
        .admin(Method::DELETE, &format!("/v1/admin/media/{used}"), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "media_in_use");
    assert_eq!(body["error"]["details"]["usages"][0]["live"], true);
    let (status, _) = app
        .admin(Method::DELETE, &format!("/v1/admin/media/{free}"), None)
        .await;
    assert!(status.is_success(), "{status}");
}

#[sqlx::test(migrations = "./migrations")]
async fn an_editor_cannot_add_an_html_section_but_keeps_one_made_by_a_super_admin(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let editor = create_user(&db, "editor@test.ma", "x", true, Some("editor")).await;
    let editor = jwt::issue(JWT_SECRET, editor, true, 60).unwrap();
    let page = new_page(&app, "page", "infos").await;
    let html = section("html", json!({}), json!({"fr": {"html": "<p>Hi</p>"}}));

    let version = tree(&app, page).await["draft_version"].as_i64().unwrap();
    let (status, body) = save_as(&app, &editor, page, version, json!([html])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let errors = &body["error"]["details"]["errors"];
    assert_eq!(errors[0]["path"], "/blocks/0");
    assert_eq!(errors[0]["code"], "forbidden_section");

    let saved = save_ok(&app, page, json!([html])).await; // super admin
    let version = saved["draft_version"].as_i64().unwrap();
    let mut blocks = saved["blocks"].as_array().unwrap().clone();
    blocks.push(rich("Texte"));
    let (status, body) = save_as(&app, &editor, page, version, Value::Array(blocks)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // ...and an editor cannot publish
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/admin/pages/{page}/publish"),
            Some(json!({})),
            Some(&editor),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn invalid_sections_are_refused_with_their_path(db: PgPool) {
    let app = TestApp::new(db).await;
    let page = new_page(&app, "home", "accueil").await;
    let hero = |t: &str| section("hero", json!({}), json!({"fr": {"title": t}}));
    let mut bad = rich("x");
    bad["content"]["en"] = json!({"body": "<script>alert(1)</script>"});
    let version = tree(&app, page).await["draft_version"].as_i64().unwrap();
    let (status, body) = save(&app, page, version, json!([hero("Un"), bad, hero("Deux")])).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let errors: Vec<(String, String)> = body["error"]["details"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["path"].as_str().unwrap().into(),
                e["code"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(
        errors.contains(&("/blocks/1/content/en/body".into(), "unsafe_html".into())),
        "{errors:?}"
    );
    assert!(
        errors.contains(&("/blocks/2/type".into(), "second_h1".into())),
        "{errors:?}"
    );
}

// ---- Publishing -------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn publishing_snapshots_the_draft_and_supersedes_the_previous_revision(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    save_ok(&app, page, json!([rich("Version 1")])).await;
    let (status, body) = publish(&app, page, json!({"note": "first"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["revision"]["revision_number"], 1);
    assert_eq!(body["revision"]["status"], "published");
    assert_eq!(tree(&app, page).await["has_unpublished_changes"], false);

    save_ok(&app, page, json!([rich("Version 2")])).await;
    assert_eq!(tree(&app, page).await["has_unpublished_changes"], true);
    let (status, check) = app
        .admin(
            Method::GET,
            &format!("/v1/admin/pages/{page}/publish-check"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["diff"]["added"].as_array().unwrap().len(), 1);
    assert_eq!(check["diff"]["removed"].as_array().unwrap().len(), 1);
    let (status, _) = publish(&app, page, json!({})).await;
    assert_eq!(status, StatusCode::CREATED);

    let rows: Vec<(i32, String)> = sqlx::query_as(
        "SELECT revision_number, status FROM page_revisions WHERE page_id = $1 ORDER BY 1",
    )
    .bind(page)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [(1, "superseded".to_string()), (2, "published".to_string())]
    );
    let (_, list) = app
        .admin(
            Method::GET,
            &format!("/v1/admin/pages/{page}/revisions"),
            None,
        )
        .await;
    let first = list["revisions"][1]["id"].as_str().unwrap();
    let (_, rev) = app
        .admin(Method::GET, &format!("/v1/admin/revisions/{first}"), None)
        .await;
    assert_eq!(
        rev["snapshot"]["blocks"][0]["content"]["fr"]["title"],
        "Version 1"
    );
    assert_eq!(rev["note"], "first");
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_publishes_leave_exactly_one_published_revision(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    save_ok(&app, page, json!([rich("Un")])).await;
    let (a, b, c) = tokio::join!(
        publish(&app, page, json!({})),
        publish(&app, page, json!({})),
        publish(&app, page, json!({}))
    );
    for (status, body) in [a, b, c] {
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let published: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM page_revisions WHERE page_id = $1 AND status = 'published'",
    )
    .bind(page)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(published, 1);
    let numbers: Vec<i32> = sqlx::query_scalar(
        "SELECT revision_number FROM page_revisions WHERE page_id = $1 ORDER BY 1",
    )
    .bind(page)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(numbers, [1, 2, 3], "gap-free numbers");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_scheduled_revision_is_published_by_the_job(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    app.state.clock.set(Some(t0()));
    let page = new_page(&app, "home", "accueil").await;
    save_ok(&app, page, json!([rich("Soon")])).await;
    let at = t0() + Duration::hours(2);
    let (status, body) = publish(&app, page, json!({"publish_at": at})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["revision"]["status"], "scheduled");
    let (status, _) = public(&app, "/v1/pages/home").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing live yet");

    assert_eq!(
        service::promote_due(&db, t0() + Duration::hours(1))
            .await
            .unwrap(),
        0
    );
    assert_eq!(service::promote_due(&db, at).await.unwrap(), 1);
    assert_eq!(
        service::promote_due(&db, at).await.unwrap(),
        0,
        "idempotent"
    );
    let (status, page_body) = public(&app, "/v1/pages/home").await;
    assert_eq!(status, StatusCode::OK, "{page_body}");
    assert_eq!(titles(&page_body), ["Soon"]);

    // a scheduled revision can be cancelled
    save_ok(&app, page, json!([rich("Later")])).await;
    let (_, body) = publish(&app, page, json!({"publish_at": at + Duration::days(1)})).await;
    let id = body["revision"]["id"].as_str().unwrap();
    let (status, body) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/revisions/{id}/cancel"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        service::promote_due(&db, at + Duration::days(2))
            .await
            .unwrap(),
        0
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn restoring_a_revision_copies_it_into_the_draft_without_publishing(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    save_ok(&app, page, json!([rich("Original"), rich("Second")])).await;
    let (_, body) = publish(&app, page, json!({})).await;
    let first = body["revision"]["id"].as_str().unwrap().to_string();
    save_ok(&app, page, json!([rich("Rewritten")])).await;
    publish(&app, page, json!({})).await;

    let (status, body) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/revisions/{first}/restore"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let restored: Vec<&str> = body["tree"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["content"]["fr"]["title"].as_str().unwrap())
        .collect();
    assert_eq!(restored, ["Original", "Second"]);
    assert_eq!(body["tree"]["has_unpublished_changes"], true);
    assert_eq!(
        body["tree"]["published_revision"], 2,
        "still revision 2 live"
    );
    let (_, live) = public(&app, "/v1/pages/home").await;
    assert_eq!(titles(&live), ["Rewritten"]);
}

// ---- Public rendering -------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn scheduled_sections_appear_when_their_period_starts(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    app.state.clock.set(Some(t0()));
    let page = new_page(&app, "home", "accueil").await;
    let start = t0() + Duration::minutes(30);
    let mut promo = rich("Promo");
    promo["active_period"] = json!({"start": start, "end": start + Duration::days(1)});
    save_ok(&app, page, json!([rich("Toujours"), promo])).await;
    publish(&app, page, json!({})).await;

    let (_, body) = public(&app, "/v1/pages/home?locale=fr").await;
    assert_eq!(titles(&body), ["Toujours"]);
    // cached until the section starts, not for the full hour
    let ttl: i64 = redis::cmd("TTL")
        .arg(app.state.cache.key(&format!("pages:{page}:1:fr")))
        .query_async(&mut app.state.cache.connection())
        .await
        .unwrap();
    assert!((1700..=1800).contains(&ttl), "ttl {ttl}");

    // Redis expiry runs on real time: drop the entry the way the TTL would.
    app.state.cache.delete_prefix("pages:").await.unwrap();
    app.state.clock.set(Some(start));
    let (_, body) = public(&app, "/v1/pages/home?locale=fr").await;
    assert_eq!(titles(&body), ["Toujours", "Promo"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn languages_hide_sections_and_fall_back_field_by_field(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    let both = section(
        "rich_text",
        json!({}),
        json!({"fr": {"title": "Titre", "subtitle": "Sous-titre", "body": "<p>Corps</p>"}, "ar": {"title": "عنوان"}}),
    );
    let mut french_only = rich("Seulement en français");
    french_only["hidden_locales"] = json!(["ar"]);
    save_ok(&app, page, json!([both, french_only])).await;
    publish(&app, page, json!({})).await;

    let (_, ar) = public(&app, "/v1/pages/home?locale=ar").await;
    assert_eq!(ar["sections"].as_array().unwrap().len(), 1);
    let content = &ar["sections"][0]["content"];
    assert_eq!(content["title"], "عنوان");
    assert_eq!(
        content["subtitle"], "Sous-titre",
        "missing Arabic text falls back to French"
    );
    assert_eq!(ar["page"]["path"], "/ar");
    let (_, fr) = public(&app, "/v1/pages/home").await;
    assert_eq!(fr["sections"].as_array().unwrap().len(), 2);
    let langs: Vec<&str> = fr["page"]["hreflang"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["locale"].as_str().unwrap())
        .collect();
    assert!(langs.contains(&"ar") && langs.contains(&"fr"), "{langs:?}");
}

#[sqlx::test(migrations = "./migrations")]
async fn archived_products_of_a_manual_list_are_skipped(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    let (quad, camel) = (
        product(&db, "activity", "Quad").await,
        product(&db, "activity", "Dromadaire").await,
    );
    let list = section(
        "product_carousel",
        json!({"source": {"mode": "manual", "ids": [camel, quad]}, "layout": "grid_3"}),
        json!({"fr": {"title": "Activités"}}),
    );
    save_ok(&app, page, json!([list])).await;
    publish(&app, page, json!({})).await;
    sqlx::query("UPDATE products SET status = 'archived' WHERE id = $1")
        .bind(quad)
        .execute(&db)
        .await
        .unwrap();

    let (status, body) = public(&app, "/v1/pages/home").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["sections"][0]["data"]["products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Dromadaire"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn publishing_changes_the_cache_key(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "home", "accueil").await;
    save_ok(&app, page, json!([rich("Un")])).await;
    publish(&app, page, json!({})).await;
    let (_, body) = public(&app, "/v1/pages/home").await;
    assert_eq!(body["page"]["revision"], 1);
    let key1 = format!("pages:{page}:1:fr");
    assert!(app.state.cache.get_json::<Value>(&key1).await.is_some());

    save_ok(&app, page, json!([rich("Deux")])).await;
    publish(&app, page, json!({})).await;
    let (_, body) = public(&app, "/v1/pages/home").await;
    assert_eq!(body["page"]["revision"], 2);
    assert_eq!(titles(&body), ["Deux"]);
    assert!(
        app.state
            .cache
            .get_json::<Value>(&format!("pages:{page}:2:fr"))
            .await
            .is_some()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn previews_render_the_draft_and_are_never_cached(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let page = new_page(&app, "page", "a-propos").await;
    save_ok(&app, page, json!([rich("Brouillon")])).await;
    let (status, _) = public(&app, "/v1/pages/by-slug/a-propos").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "never published");

    let (status, body) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/pages/{page}/preview-token"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let token = body["token"].as_str().unwrap();
    let (status, headers, body) = app
        .send(
            Method::GET,
            &format!("/v1/pages/by-slug/a-propos?preview={token}"),
            None,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body["preview"], true);
    assert_eq!(titles(&body), ["Brouillon"]);
    let (status, _) = public(&app, "/v1/pages/by-slug/a-propos?preview=0000").await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // live preview of a section being edited
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/v1/pages/{page}/preview/section?locale=fr"),
            Some(json!({"token": token, "section": rich("En cours")})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["section"]["content"]["title"], "En cours");
}

// ---- Shared sections ------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn shared_sections_are_placed_inlined_and_protected(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let home = new_page(&app, "home", "accueil").await;
    let about = new_page(&app, "page", "a-propos").await;
    let (status, shared) = app
        .admin(
            Method::POST,
            "/v1/admin/shared-sections",
            Some(section(
                "cta_band",
                json!({}),
                json!({"fr": {"title": "Venez !"}}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{shared}");
    let shared_id = shared["id"].as_str().unwrap();
    for page in [home, about] {
        save_ok(
            &app,
            page,
            json!([{"id": Uuid::now_v7(), "type": "cta_band", "shared_block_id": shared_id}]),
        )
        .await;
        publish(&app, page, json!({})).await;
    }
    let (_, body) = public(&app, "/v1/pages/home").await;
    assert_eq!(titles(&body), ["Venez !"]);
    let builder = tree(&app, home).await;
    assert_eq!(
        builder["blocks"][0]["shared"]["content"]["fr"]["title"],
        "Venez !"
    );

    let (status, body) = app
        .admin(
            Method::DELETE,
            &format!("/v1/admin/shared-sections/{shared_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["error"]["details"]["usages"].as_array().unwrap().len(),
        2
    );

    let (status, body) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shared-sections/{shared_id}"),
            Some(section(
                "cta_band",
                json!({}),
                json!({"fr": {"title": "Réservez"}}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for page in [home, about] {
        assert_eq!(tree(&app, page).await["has_unpublished_changes"], true);
    }
    // a placement cannot point to an ordinary section
    let ordinary = rich("x");
    save_ok(&app, about, json!([ordinary])).await;
    let version = tree(&app, home).await["draft_version"].as_i64().unwrap();
    let placement =
        json!([{"id": Uuid::now_v7(), "type": "rich_text", "shared_block_id": ordinary["id"]}]);
    let (status, body) = save(&app, home, version, placement).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["error"]["details"]["errors"][0]["code"],
        "shared_not_found"
    );
}
