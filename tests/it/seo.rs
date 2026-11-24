//! SEO against real Postgres + Redis: automatic redirects for every content type with a URL,
//! nested pages, chains, loops, live content winning, unpublish / delete, manual redirects,
//! indexability reasons, sitemaps (indexable URLs only, hreflang groups, lastmod, schema),
//! per-file invalidation, visit counting and permissions.

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use mouzoun_api::{
    db::operations::OutboxEvent,
    modules::seo::{SeoCacheHandler, hits, redirects, routes},
    services::audit::Actor,
    worker::OutboxHandler,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{INTERNAL_TOKEN, JWT_SECRET, TestApp, create_user};

type Row = (String, String, bool, String);

async fn rows(db: &PgPool) -> Vec<Row> {
    sqlx::query_as("SELECT from_path, to_path, is_active, source FROM redirects ORDER BY from_path")
        .fetch_all(db)
        .await
        .unwrap()
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

fn field(v: &Value, f: &str) -> String {
    v["error"]["details"]["fields"][f]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// ---- Pages ----------------------------------------------------------------------------------------

/// A published builder page with the given slugs (`(locale, slug)`).
async fn page(app: &TestApp, texts: &[(&str, &str)], parent: Option<Uuid>) -> Uuid {
    let translations: serde_json::Map<String, Value> = texts
        .iter()
        .map(|(l, s)| {
            (
                (*l).to_string(),
                json!({ "title": format!("Page {s}"), "slug": s }),
            )
        })
        .collect();
    let (status, body) = app
        .admin(
            Method::POST,
            "/v1/admin/pages",
            Some(json!({ "page_type": "page", "translations": translations })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    if let Some(p) = parent {
        sqlx::query("UPDATE pages SET parent_id = $2 WHERE id = $1")
            .bind(id)
            .bind(p)
            .execute(&app.db)
            .await
            .unwrap();
    }
    let (status, body) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/pages/{id}/publish"),
            Some(json!({})),
        )
        .await;
    assert!(status.is_success(), "{body}");
    id
}

async fn rename_page(app: &TestApp, id: Uuid, texts: &[(&str, &str)]) {
    let (_, tree) = app
        .admin(Method::GET, &format!("/v1/admin/pages/{id}/builder"), None)
        .await;
    let translations: serde_json::Map<String, Value> = texts
        .iter()
        .map(|(l, s)| {
            (
                (*l).to_string(),
                json!({ "title": format!("Page {s}"), "slug": s }),
            )
        })
        .collect();
    let (status, _, body) = app
        .send(
            Method::PUT,
            &format!("/v1/admin/pages/{id}/builder"),
            Some(json!({ "blocks": tree["blocks"], "translations": translations })),
            &[
                ("authorization", format!("Bearer {}", app.admin_token)),
                ("if-match", format!("\"{}\"", tree["draft_version"])),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ---- Shop -----------------------------------------------------------------------------------------

async fn shop_open(db: &PgPool) {
    sqlx::query("UPDATE settings SET value = '\"open\"' WHERE key = 'shop.status'")
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO shipping_profiles (code, name, is_default) VALUES ('standard', 'Standard', true)")
        .execute(db)
        .await
        .unwrap();
}

async fn shop_product(app: &TestApp, fr: &str, en: &str) -> String {
    let (status, p) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/products",
            Some(json!({
                "translations": { "fr": { "name": fr, "slug": fr }, "en": { "name": en, "slug": en } },
                "shop": { "product_type": "craft" },
                "variant": { "sku": format!("sku-{fr}"), "price_minor": 900, "weight_grams": 500 }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{p}");
    let id = p["id"].as_str().unwrap().to_string();
    let (status, b) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/products/bulk",
            Some(json!({ "ids": [id], "action": "publish" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    id
}

/// Every language's path of a piece of content, through the registry.
async fn paths(db: &PgPool, t: &str, id: Uuid) -> BTreeMap<String, String> {
    let mut conn = db.acquire().await.unwrap();
    let ctx = routes::Ctx::load(&mut conn, Utc::now()).await.unwrap();
    routes::paths_for(&mut conn, &ctx, t, id).await.unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_slug_change_redirects_each_changed_language_for_every_content_type(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    shop_open(&db).await;

    // Builder page: fr and en renamed, ar unchanged → two redirects.
    let p = page(
        &app,
        &[("fr", "a-propos"), ("en", "about"), ("ar", "hawl")],
        None,
    )
    .await;
    rename_page(
        &app,
        p,
        &[
            ("fr", "notre-histoire"),
            ("en", "our-story"),
            ("ar", "hawl"),
        ],
    )
    .await;
    let r = rows(&db).await;
    assert!(
        r.contains(&(
            "/a-propos".into(),
            "/notre-histoire".into(),
            true,
            "auto".into()
        )),
        "{r:?}"
    );
    assert!(
        r.contains(&(
            "/en/about".into(),
            "/en/our-story".into(),
            true,
            "auto".into()
        )),
        "{r:?}"
    );
    assert_eq!(r.len(), 2, "{r:?}");

    // Shop product (admin PUT with a new French slug).
    let id = shop_product(&app, "panier", "basket").await;
    let (status, b) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/products/{id}"),
            Some(json!({ "status": "published", "translations": { "fr": { "name": "Panier", "slug": "panier-tresse", "description": "Panier tressé." }, "en": { "name": "Basket", "slug": "basket" } },
                         "shop": { "product_type": "craft" } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert!(rows(&db).await.contains(&(
        "/boutique/p/panier".into(),
        "/boutique/p/panier-tresse".into(),
        true,
        "auto".into()
    )));

    // Shop category (admin PATCH).
    let (status, c) = app
        .admin(Method::POST, "/v1/admin/shop/categories", Some(json!({ "translations": { "fr": { "name": "Miel" }, "en": { "name": "Honey" } }, "is_active": true })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    let cid = c["id"].as_str().unwrap();
    app.admin(
        Method::PATCH,
        &format!("/v1/admin/shop/categories/{cid}"),
        Some(json!({ "translations": { "en": { "name": "Honey", "slug": "wild-honey" } } })),
    )
    .await;
    assert!(rows(&db).await.contains(&(
        "/en/shop/honey".into(),
        "/en/shop/wild-honey".into(),
        true,
        "auto".into()
    )));

    // Activity (no admin editor yet: the service, as the seed does).
    let act = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, status, published_at) VALUES ($1, 'activity', 'published', now())").bind(act).execute(&db).await.unwrap();
    for (l, s) in [("fr", "balade"), ("ar", "jawla")] {
        sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, $2, 'Balade', $3)")
            .bind(act).bind(l).bind(s).execute(&db).await.unwrap();
    }
    let mut tx = db.begin().await.unwrap();
    let before = redirects::snapshot(&mut tx, routes::ACTIVITY, act)
        .await
        .unwrap();
    sqlx::query("UPDATE product_translations SET slug = 'jawlat-jamal' WHERE product_id = $1 AND locale = 'ar'").bind(act).execute(&mut *tx).await.unwrap();
    redirects::after_change(&mut tx, &Actor::system(), routes::ACTIVITY, act, before)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let r = rows(&db).await;
    assert!(
        r.contains(&(
            "/ar/activities/jawla".into(),
            "/ar/activities/jawlat-jamal".into(),
            true,
            "auto".into()
        )),
        "{r:?}"
    );
    assert!(
        !r.iter().any(|x| x.0 == "/activities/balade"),
        "unchanged languages get no redirect"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn renaming_a_parent_page_moves_and_redirects_its_descendants(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let parent = page(&app, &[("fr", "ferme")], None).await;
    let child = page(&app, &[("fr", "ferme/dattes")], Some(parent)).await;
    let grandchild = page(&app, &[("fr", "ferme/dattes/recolte")], Some(child)).await;
    rename_page(&app, parent, &[("fr", "la-ferme")]).await;
    assert_eq!(
        paths(&db, routes::PAGE, grandchild).await["fr"],
        "/la-ferme/dattes/recolte"
    );
    let r: Vec<(String, String)> = rows(&db).await.into_iter().map(|r| (r.0, r.1)).collect();
    assert_eq!(
        r,
        vec![
            ("/ferme".into(), "/la-ferme".into()),
            ("/ferme/dattes".into(), "/la-ferme/dattes".into()),
            (
                "/ferme/dattes/recolte".into(),
                "/la-ferme/dattes/recolte".into()
            ),
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn chains_are_flattened_loops_refused_and_live_content_wins(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let new =
        |from: &str, to: &str| json!({ "from_path": from, "to_path": to, "status_code": 301 });
    let (status, b) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/a", "/b")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let (status, _) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/b", "/c")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let r: Vec<(String, String)> = rows(&db).await.into_iter().map(|r| (r.0, r.1)).collect();
    assert_eq!(
        r,
        vec![("/a".into(), "/c".into()), ("/b".into(), "/c".into())]
    );
    // A target that is itself redirected: the final target is stored.
    let (_, b) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/x", "/a")),
        )
        .await;
    assert_eq!(b["checked"]["to_path"], "/c");

    // Loops: direct and through other redirects (API and database).
    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/d", "/d")),
        )
        .await;
    assert_eq!(
        (status, field(&e, "to_path")),
        (StatusCode::UNPROCESSABLE_ENTITY, "loop".into())
    );
    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/c", "/a")),
        )
        .await;
    assert_eq!(
        (status, field(&e, "to_path")),
        (StatusCode::UNPROCESSABLE_ENTITY, "loop".into()),
        "{e}"
    );
    let err = sqlx::query("INSERT INTO redirects (from_path, to_path) VALUES ('/c', '/b')")
        .execute(&db)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("redirect_loop"), "{err}");

    // Renaming back to an old slug deletes the redirect that started there.
    let p = page(&app, &[("fr", "camp")], None).await;
    rename_page(&app, p, &[("fr", "bivouac")]).await;
    assert!(
        rows(&db)
            .await
            .iter()
            .any(|r| r.0 == "/camp" && r.1 == "/bivouac")
    );
    rename_page(&app, p, &[("fr", "camp")]).await;
    let r = rows(&db).await;
    assert!(!r.iter().any(|x| x.0 == "/camp"), "{r:?}");
    assert!(
        r.iter().any(|x| x.0 == "/bivouac" && x.1 == "/camp"),
        "{r:?}"
    );
    let claimed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE action = 'redirect.claimed'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(claimed, 1);

    // A manual redirect may not start at live content.
    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/camp", "/x")),
        )
        .await;
    assert_eq!(
        (status, field(&e, "from_path")),
        (StatusCode::UNPROCESSABLE_ENTITY, "live_content".into())
    );
    // External targets: https only, manual only.
    let (status, _) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/insta", "https://www.instagram.com/mouzoun_mssici/")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(new("/fb", "http://facebook.com")),
        )
        .await;
    assert_eq!(
        (status, field(&e, "to_path")),
        (StatusCode::UNPROCESSABLE_ENTITY, "https_only".into())
    );
    // Editing an automatic redirect makes it manual.
    let id: Uuid = sqlx::query_scalar("SELECT id FROM redirects WHERE from_path = '/bivouac'")
        .fetch_one(&db)
        .await
        .unwrap();
    let (status, _) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/seo/redirects/{id}"),
            Some(json!({ "note": "gardée" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let source: String = sqlx::query_scalar("SELECT source FROM redirects WHERE id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(source, "manual");
}

#[sqlx::test(migrations = "./migrations")]
async fn unpublishing_keeps_an_inactive_redirect_that_deletion_activates(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    shop_open(&db).await;
    let (_, c) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/categories",
            Some(json!({ "translations": { "fr": { "name": "Artisanat" } }, "is_active": true })),
        )
        .await;
    let cat = c["id"].as_str().unwrap().to_string();
    let id = shop_product(&app, "panier", "basket").await;
    app.admin(
        Method::POST,
        &format!("/v1/admin/shop/categories/{cat}/products"),
        Some(json!({ "product_ids": [id] })),
    )
    .await;

    app.admin(
        Method::POST,
        "/v1/admin/shop/products/bulk",
        Some(json!({ "ids": [id], "action": "archive" })),
    )
    .await;
    let r = rows(&db).await;
    assert!(
        r.contains(&(
            "/boutique/p/panier".into(),
            "/boutique/artisanat".into(),
            false,
            "auto".into()
        )),
        "{r:?}"
    );
    assert!(
        r.contains(&(
            "/en/shop/p/basket".into(),
            "/en/shop".into(),
            false,
            "auto".into()
        )),
        "{r:?}"
    );
    // Published again: its own addresses win.
    app.admin(
        Method::POST,
        "/v1/admin/shop/products/bulk",
        Some(json!({ "ids": [id], "action": "publish" })),
    )
    .await;
    assert!(rows(&db).await.is_empty());
    // Archived, then deleted: the redirect becomes active.
    app.admin(
        Method::POST,
        "/v1/admin/shop/products/bulk",
        Some(json!({ "ids": [id], "action": "archive" })),
    )
    .await;
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!("/v1/admin/shop/products/{id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let r = rows(&db).await;
    assert!(
        r.contains(&(
            "/boutique/p/panier".into(),
            "/boutique/artisanat".into(),
            true,
            "auto".into()
        )),
        "{r:?}"
    );
    // The map the Nuxt server reads has it.
    let (_, map) = app
        .request(Method::GET, "/v1/seo/redirects/map", None, None)
        .await;
    assert_eq!(
        map["redirects"]["/boutique/p/panier"]["to"],
        "/boutique/artisanat"
    );
}

async fn reason(app: &TestApp, path: &str) -> Option<String> {
    let (status, t) = app
        .admin(
            Method::GET,
            &format!("/v1/admin/seo/test?path={path}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{t}");
    t["content"]["indexability"]["reason"]
        .as_str()
        .map(String::from)
}

#[sqlx::test(migrations = "./migrations")]
async fn indexability_gives_one_reason_per_rule(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let ok = page(&app, &[("fr", "ok"), ("en", "ok-en")], None).await;
    assert_eq!(reason(&app, "/ok").await, None);
    assert_eq!(
        reason(&app, "/ar/ok").await.as_deref(),
        Some("translation_missing")
    );
    sqlx::query("UPDATE page_translations SET noindex = true WHERE page_id = $1 AND locale = 'en'")
        .bind(ok)
        .execute(&db)
        .await
        .unwrap();
    app.state.cache.delete_prefix("seo:").await.unwrap();
    assert_eq!(reason(&app, "/en/ok-en").await.as_deref(), Some("noindex"));
    let p = page(&app, &[("fr", "hors-plan")], None).await;
    sqlx::query("UPDATE pages SET in_sitemap = false WHERE id = $1")
        .bind(p)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        reason(&app, "/hors-plan").await.as_deref(),
        Some("not_in_sitemap")
    );
    let p = page(&app, &[("fr", "copie")], None).await;
    sqlx::query("UPDATE page_translations SET canonical_url = 'https://ailleurs.example/x' WHERE page_id = $1").bind(p).execute(&db).await.unwrap();
    assert_eq!(
        reason(&app, "/copie").await.as_deref(),
        Some("canonical_elsewhere")
    );
    let parent = page(&app, &[("fr", "parent")], None).await;
    page(&app, &[("fr", "parent/enfant")], Some(parent)).await;
    sqlx::query("UPDATE pages SET status = 'archived' WHERE id = $1")
        .bind(parent)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        reason(&app, "/parent").await.as_deref(),
        Some("not_published")
    );
    assert_eq!(
        reason(&app, "/parent/enfant").await.as_deref(),
        Some("parent_not_public")
    );
    let p = page(&app, &[("fr", "bientot")], None).await;
    sqlx::query("UPDATE pages SET publish_at = now() + interval '1 day' WHERE id = $1")
        .bind(p)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(reason(&app, "/bientot").await.as_deref(), Some("scheduled"));
    let p = page(&app, &[("fr", "efface")], None).await;
    sqlx::query("UPDATE pages SET deleted_at = now() WHERE id = $1")
        .bind(p)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(reason(&app, "/efface").await.as_deref(), Some("deleted"));
    // Shop category without a published product; shop closed; inactive language.
    shop_open(&db).await;
    app.admin(
        Method::POST,
        "/v1/admin/shop/categories",
        Some(json!({ "translations": { "fr": { "name": "Vide" } }, "is_active": true })),
    )
    .await;
    assert_eq!(
        reason(&app, "/boutique/vide").await.as_deref(),
        Some("no_published_products")
    );
    sqlx::query("UPDATE settings SET value = '\"closed\"' WHERE key = 'shop.status'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        reason(&app, "/boutique/vide").await.as_deref(),
        Some("shop_closed")
    );
    sqlx::query("UPDATE locales SET is_active = false WHERE code = 'en'")
        .execute(&db)
        .await
        .unwrap();
    let mut conn = db.acquire().await.unwrap();
    let ctx = routes::Ctx::load(&mut conn, Utc::now()).await.unwrap();
    let verdict = routes::indexability(
        &mut conn,
        &ctx,
        routes::PAGE,
        ok,
        "en",
        "https://mouzoun.test",
    )
    .await
    .unwrap();
    assert_eq!(
        verdict,
        routes::Indexability::NotIndexable("locale_inactive")
    );
    // The head tags say the same.
    sqlx::query("UPDATE locales SET is_active = true WHERE code = 'en'")
        .execute(&db)
        .await
        .unwrap();
    app.state.cache.delete_prefix("seo:").await.unwrap();
    let (_, head) = app
        .request(Method::GET, "/v1/seo/head?path=/en/ok-en", None, None)
        .await;
    assert_eq!(head["noindex"], true);
    let (_, head) = app
        .request(Method::GET, "/v1/seo/head?path=/ok", None, None)
        .await;
    assert_eq!(head["noindex"], false);
    assert!(head["canonical"].as_str().unwrap().ends_with("/ok"));
}

#[sqlx::test(migrations = "./migrations")]
async fn sitemaps_list_indexable_versions_with_hreflang_lastmod_and_images(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let both = page(&app, &[("fr", "visite"), ("en", "visit")], None).await;
    let fr_only = page(&app, &[("fr", "seulement")], None).await;
    let hidden = page(&app, &[("fr", "cachee"), ("en", "hidden")], None).await;
    sqlx::query("UPDATE page_translations SET noindex = true WHERE page_id = $1 AND locale = 'en'")
        .bind(hidden)
        .execute(&db)
        .await
        .unwrap();
    let _ = (both, fr_only);
    let (status, _, _) = app
        .send(Method::GET, "/v1/seo/sitemaps/pages.xml", None, &[])
        .await;
    assert_eq!(status, StatusCode::OK);
    let xml = sitemap_text(&app, "/v1/seo/sitemaps/pages.xml").await;
    // One <url> per indexable version; alternates only to indexable versions; x-default = fr.
    assert!(xml.contains("<loc>https://"), "{xml}");
    let block = |loc: &str| {
        let start = xml
            .find(&format!("{loc}</loc>"))
            .unwrap_or_else(|| panic!("{loc} missing:\n{xml}"));
        let end = xml[start..].find("</url>").unwrap() + start;
        xml[start..end].to_string()
    };
    let visite = block("/visite");
    assert!(
        visite.contains("hreflang=\"en\"")
            && visite.contains("/en/visit\"")
            && visite.contains("hreflang=\"x-default\""),
        "{visite}"
    );
    assert!(!visite.contains("hreflang=\"ar\""), "{visite}");
    assert!(visite.contains("<lastmod>"), "{visite}");
    let seule = block("/seulement");
    assert!(
        !seule.contains("hreflang=\"en\"") && !seule.contains("hreflang=\"ar\""),
        "{seule}"
    );
    assert!(!xml.contains("/en/hidden"), "noindex versions stay out");
    assert!(xml.contains("/cachee</loc>"));
    // Listing pages are in pages.xml; the index lists only non-empty files.
    assert!(xml.contains("/activities</loc>"));
    let index = sitemap_text(&app, "/v1/seo/sitemap.xml").await;
    assert!(
        index.contains("/sitemaps/pages.xml") && !index.contains("/sitemaps/posts.xml"),
        "{index}"
    );
    // Unknown files: 404.
    let (status, _, _) = app
        .send(Method::GET, "/v1/seo/sitemaps/secrets.xml", None, &[])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    validate_schema(&index, "sitemapindex");
    validate_schema(&xml, "urlset");
}

async fn sitemap_text(app: &TestApp, uri: &str) -> String {
    use tower::ServiceExt;
    let res = app
        .router
        .clone()
        .oneshot(
            axum::http::Request::get(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers()["content-type"],
        "application/xml; charset=utf-8"
    );
    let bytes = http_body_util::BodyExt::collect(res.into_body())
        .await
        .unwrap()
        .to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Validates against the sitemaps.org and Google image schemas (tests/it/seo_xsd) with xmllint
/// when it is installed (macOS, most Linux images); skipped otherwise.
fn validate_schema(xml: &str, root: &str) {
    let Ok(out) = std::process::Command::new("xmllint")
        .arg("--version")
        .output()
    else {
        eprintln!("xmllint not installed: sitemap schema check skipped");
        return;
    };
    let _ = out;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/it/seo_xsd");
    let schema = dir.join(if root == "urlset" {
        "sitemap.xsd"
    } else {
        "siteindex.xsd"
    });
    let file = std::env::temp_dir().join(format!("mouzoun-sitemap-{}.xml", Uuid::new_v4()));
    std::fs::write(&file, xml).unwrap();
    let res = std::process::Command::new("xmllint")
        .arg("--noout")
        .arg("--schema")
        .arg(&schema)
        .arg(&file)
        .output()
        .unwrap();
    std::fs::remove_file(&file).ok();
    assert!(
        res.status.success(),
        "{}\n{xml}",
        String::from_utf8_lossy(&res.stderr)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_write_invalidates_only_the_affected_sitemap_file(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let cache = &app.state.cache;
    for f in ["pages:1", "activities:1", "shop-categories:1", "index"] {
        cache
            .set_json(
                &format!("seo:sitemap:{f}"),
                &"x".to_string(),
                std::time::Duration::from_secs(60),
            )
            .await;
    }
    let handler = SeoCacheHandler::new(db.clone(), cache.clone());
    let event = |event_type: &str, payload: Value| OutboxEvent {
        id: 1,
        aggregate_type: "content".into(),
        aggregate_id: "x".into(),
        event_type: event_type.into(),
        payload,
        available_at: Utc::now(),
        attempts: 0,
        last_error: None,
        processed_at: None,
        failed_at: None,
        dedupe_key: None,
        created_at: Utc::now(),
    };
    handler
        .handle(&event(
            "content.changed",
            json!({ "ref_type": "category", "ref_id": Uuid::nil() }),
        ))
        .await
        .unwrap();
    let present = |k: &'static str| async move {
        cache
            .get_json::<String>(&format!("seo:sitemap:{k}"))
            .await
            .is_some()
    };
    assert!(!present("shop-categories:1").await);
    assert!(!present("index").await);
    assert!(present("pages:1").await && present("activities:1").await);
    // A redirect-only change leaves every sitemap file alone and bumps the map version.
    let (_, before) = app
        .request(Method::GET, "/v1/seo/redirects/version", None, None)
        .await;
    handler
        .handle(&event(
            "seo.changed",
            json!({ "redirects": true, "sitemaps": [] }),
        ))
        .await
        .unwrap();
    assert!(present("pages:1").await && present("activities:1").await);
    let (_, after) = app
        .request(Method::GET, "/v1/seo/redirects/version", None, None)
        .await;
    assert_ne!(before["version"], after["version"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn visits_and_404s_are_counted_through_redis_and_flushed(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    sqlx::query("INSERT INTO redirects (from_path, to_path) VALUES ('/promo', '/activities')")
        .execute(&db)
        .await
        .unwrap();
    let batch = json!({
        "redirects": { "/promo": 3 },
        "not_found": [
            { "path": "/sejours/", "hits": 2, "referrer": "https://www.google.com/" },
            { "path": "/wp-login.php", "hits": 50 },
            { "path": "/.env", "hits": 9 }
        ]
    });
    // Without the internal token: refused.
    let (status, _) = app
        .request(Method::POST, "/v1/seo/hits", Some(batch.clone()), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/seo/hits",
            Some(batch.clone()),
            &[("x-internal-token", INTERNAL_TOKEN.into())],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/seo/hits",
            Some(batch),
            &[("x-internal-token", INTERNAL_TOKEN.into())],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Nothing in Postgres until the flush.
    let hits_now: i64 = sqlx::query_scalar("SELECT hits FROM redirects WHERE from_path = '/promo'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(hits_now, 0);
    hits::flush(&app.state.cache, &db).await.unwrap();
    let (hits_now, last): (i64, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT hits, last_hit_at FROM redirects WHERE from_path = '/promo'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((hits_now, last.is_some()), (6, true));
    let logged: Vec<(String, i64, Option<String>)> =
        sqlx::query_as("SELECT path, hits, last_referrer FROM not_found_log ORDER BY path")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(
        logged,
        vec![("/sejours".into(), 4, Some("https://www.google.com/".into()))]
    );
    // A second flush has nothing new.
    hits::flush(&app.state.cache, &db).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT hits FROM not_found_log WHERE path = '/sejours'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(n, 4);
    // Old rows are purged.
    sqlx::query("UPDATE not_found_log SET last_seen_at = now() - interval '91 days'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(hits::purge(&db).await.unwrap(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn seo_manage_edits_and_others_get_403(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let token = |id| mouzoun_api::auth::jwt::issue(JWT_SECRET, id, true, 60).unwrap();
    let editor = token(create_user(&db, "editor@test.ma", "x", true, Some("editor")).await);
    let reception =
        token(create_user(&db, "reception@test.ma", "x", true, Some("reception")).await);
    let (status, b) = app
        .request(
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(json!({ "from_path": "/old", "to_path": "/stays" })),
            Some(&editor),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    for (method, uri, body) in [
        (Method::GET, "/v1/admin/seo/redirects", None),
        (
            Method::POST,
            "/v1/admin/seo/redirects",
            Some(json!({ "from_path": "/x", "to_path": "/stays" })),
        ),
        (Method::GET, "/v1/admin/seo/not-found", None),
        (Method::GET, "/v1/admin/seo/test?path=/", None),
        (Method::POST, "/v1/admin/seo/sitemap/rebuild", None),
    ] {
        let (status, _) = app.request(method, uri, body, Some(&reception)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE action = 'redirect.created'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(audits, 1);
    let _ = code(&json!({}));
}
