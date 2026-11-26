//! Stay and activity categories (`/v1/admin/catalog/categories`, scope `product`) against real
//! Postgres: the shop category screens' rules on another scope, kept apart from the shop's (each
//! endpoint only sees its own scope), a flat ordered list, bookable products only, no SEO
//! redirects, suggested categories, permissions (`products.*`), and the pickers that must only
//! offer product categories (catalog editor, builder carousel and tiles).

use axum::http::{Method, StatusCode};
use mouzoun_api::auth::jwt;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{JWT_SECRET, TestApp, create_user};

const BASE: &str = "/v1/admin/catalog/categories";
const SHOP: &str = "/v1/admin/shop/categories";

fn names(fr: &str) -> Value {
    json!({ "fr": { "name": fr }, "en": { "name": format!("{fr} (en)") }, "ar": { "name": format!("{fr} (ar)") } })
}

async fn create_in(app: &TestApp, base: &str, fr: &str) -> Uuid {
    let (status, c) = app
        .admin(
            Method::POST,
            base,
            Some(json!({ "translations": names(fr), "is_active": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    c["id"].as_str().unwrap().parse().unwrap()
}

async fn create(app: &TestApp, fr: &str) -> Uuid {
    create_in(app, BASE, fr).await
}

async fn product(db: &PgPool, kind: &str, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    let mode = if kind == "shop_product" {
        "not_bookable"
    } else {
        "instant"
    };
    sqlx::query(
        "INSERT INTO products (id, kind, status, booking_mode) VALUES ($1, $2, 'published', $3)",
    )
    .bind(id)
    .bind(kind)
    .bind(mode)
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, 'fr', $2, $3)")
        .bind(id)
        .bind(name)
        .bind(name.to_lowercase().replace(' ', "-"))
        .execute(db)
        .await
        .unwrap();
    id
}

async fn list(app: &TestApp, base: &str) -> Value {
    let (status, l) = app.admin(Method::GET, base, None).await;
    assert_eq!(status, StatusCode::OK, "{l}");
    l
}

fn ids(l: &Value) -> Vec<Uuid> {
    l["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().parse().unwrap())
        .collect()
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

#[sqlx::test(migrations = "./migrations")]
async fn the_form_saves_every_language_and_the_scopes_stay_apart(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (status, c) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({
                "icon": "i-lucide-mountain",
                "translations": {
                    "fr": { "name": "Aventure désert", "description": "<p>Dunes <script>x</script></p>",
                            "seo_title": "Aventure · Merzouga", "seo_description": "Dromadaires et quads." },
                    "en": { "name": "Desert adventure" },
                    "ar": { "name": "مغامرة في الصحراء" },
                },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    let id = c["id"].as_str().unwrap().to_string();
    assert_eq!(c["translations"]["fr"]["slug"], "aventure-desert");
    assert_eq!(c["translations"]["en"]["slug"], "desert-adventure");
    assert_eq!(c["translations"]["ar"]["slug"], "مغامرة-في-الصحراء");
    assert_eq!(c["translations"]["fr"]["seo_title"], "Aventure · Merzouga");
    assert!(
        !c["translations"]["fr"]["description"]
            .as_str()
            .unwrap()
            .contains("script")
    );
    assert_eq!(c["icon"], "i-lucide-mountain");
    assert_eq!(c["is_active"], false, "new categories start inactive");
    // No public page: no address, so no "View on site".
    assert_eq!(c["paths"], json!({}));
    let scope: String = sqlx::query_scalar("SELECT scope FROM categories WHERE id = $1::uuid")
        .bind(&id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(scope, "product");

    // Each endpoint only sees its own scope.
    let shop = create_in(&app, SHOP, "Dattes").await;
    assert_eq!(ids(&list(&app, BASE).await), [id.parse::<Uuid>().unwrap()]);
    assert_eq!(ids(&list(&app, SHOP).await), [shop]);
    for uri in [format!("{SHOP}/{id}"), format!("{BASE}/{shop}")] {
        let (status, _) = app.admin(Method::GET, &uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        let (status, _) = app
            .admin(Method::PATCH, &uri, Some(json!({ "is_active": false })))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // A slug is unique per language across scopes (one table).
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("{BASE}/{id}"),
            Some(json!({ "translations": { "fr": { "name": "Aventure désert", "slug": "dattes" } } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.slug"],
        "taken"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_list_is_flat_and_ordered_in_one_call(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "Aventure désert").await;
    let b = create(&app, "Vie à la ferme").await;
    let c = create(&app, "Hébergements").await;
    let l = list(&app, BASE).await;
    assert_eq!(l["max_depth"], 1);
    assert_eq!(ids(&l), [a, b, c]);

    // Subcategories are refused, by the form and by a tree move.
    let (status, e) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": names("Quad"), "parent_id": a })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&e), "category_too_deep");
    let version = l["version"].as_str().unwrap();
    let (status, e) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": version, "items": [{ "id": b, "parent_id": a, "sort_order": 0 }] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&e), "category_too_deep");
    // A shop category is not a parent here.
    let shop = create_in(&app, SHOP, "Dattes").await;
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("{BASE}/{a}"),
            Some(json!({ "parent_id": shop })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&e), "category_parent_unknown");

    // Reorder: one call, then the new order; a stale version is refused.
    let (status, out) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": version, "items": [
                { "id": c, "parent_id": null, "sort_order": 0 },
                { "id": a, "parent_id": null, "sort_order": 1 },
                { "id": b, "parent_id": null, "sort_order": 2 },
            ] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(ids(&list(&app, BASE).await), [c, a, b]);
    let (status, e) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": version, "items": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&e), "tree_changed");
    // The shop tree's version did not move.
    let audit: String = sqlx::query_scalar(
        "SELECT entity_id FROM audit_logs WHERE action = 'category.tree_saved' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(audit, "product");
}

#[sqlx::test(migrations = "./migrations")]
async fn it_holds_bookable_products_in_order_and_delete_moves_them(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let desert = create(&app, "Aventure désert").await;
    let farm = create(&app, "Vie à la ferme").await;
    let quad = product(&db, "activity", "Quad 1h").await;
    let camel = product(&db, "activity", "Dromadaire").await;
    let weekend = product(&db, "package", "Week-end désert").await;
    let honey = product(&db, "shop_product", "Miel").await;

    let (status, e) = app
        .admin(
            Method::POST,
            &format!("{BASE}/{desert}/products"),
            Some(json!({ "product_ids": [quad, honey] })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a shop product is refused"
    );
    assert_eq!(e["error"]["details"]["fields"]["product_ids"], "invalid");
    let (status, out) = app
        .admin(
            Method::POST,
            &format!("{BASE}/{desert}/products"),
            Some(json!({ "product_ids": [quad, camel, weekend] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["added"], 3);
    // The shop side cannot add bookable products either.
    let shop = create_in(&app, SHOP, "Dattes").await;
    let (status, _) = app
        .admin(
            Method::POST,
            &format!("{SHOP}/{shop}/products"),
            Some(json!({ "product_ids": [quad] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Manual order and pins.
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/{desert}/products"),
            Some(json!([
                { "product_id": weekend, "sort_order": 0, "is_pinned": true },
                { "product_id": camel, "sort_order": 1 },
                { "product_id": quad, "sort_order": 2 },
            ])),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, p) = app
        .admin(Method::GET, &format!("{BASE}/{desert}/products"), None)
        .await;
    let order: Vec<&str> = p["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["product_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [weekend.to_string(), camel.to_string(), quad.to_string()]
    );
    assert_eq!(p["data"][0]["kind"], "package");
    assert_eq!(p["data"][0]["is_pinned"], true);
    // A product order sent to the other scope's endpoint is not found.
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("{SHOP}/{desert}/products"),
            Some(json!([{ "product_id": quad, "sort_order": 0 }])),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!("{SHOP}/{desert}/products/{quad}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Delete: the impact counts the products, the name confirms, they move to "farm".
    let (status, impact) = app
        .admin(Method::GET, &format!("{BASE}/{desert}/impact"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(impact["products"], 3);
    assert_eq!(impact["products_without_category"], 3);
    assert_eq!(impact["requires_confirmation"], true);
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!("{BASE}/{desert}"),
            Some(json!({ "products_to": farm, "confirm_name": "Aventure désert" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let moved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM product_categories WHERE category_id = $1")
            .bind(farm)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(moved, 3);
    // No public page, so no redirect.
    let redirects: i64 = sqlx::query_scalar("SELECT count(*) FROM redirects")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(redirects, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn slugs_change_freely_without_redirects(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let id = create(&app, "Culture & village").await;
    // `p` is a shop sub-path, not reserved here.
    let (status, c) = app
        .admin(
            Method::PATCH,
            &format!("{BASE}/{id}"),
            Some(json!({ "translations": { "fr": { "name": "Culture & village", "slug": "p" } } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    assert_eq!(c["translations"]["fr"]["slug"], "p");
    let redirects: i64 = sqlx::query_scalar("SELECT count(*) FROM redirects")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(redirects, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn suggested_categories_are_the_stay_and_activity_ones(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (status, out) = app
        .admin(Method::POST, &format!("{BASE}/suggested"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["created"], 4);
    let l = list(&app, BASE).await;
    let fr: Vec<&str> = l["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["translations"]["fr"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        fr,
        [
            "Aventure désert",
            "Vie à la ferme",
            "Culture & village",
            "Hébergements"
        ]
    );
    assert!(
        l["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["is_active"] == false && c["translations"]["ar"]["name"].is_string())
    );
    assert_eq!(list(&app, SHOP).await["data"], json!([]));
    // Twice: nothing new (the addresses are taken).
    let (_, out) = app
        .admin(Method::POST, &format!("{BASE}/suggested"), None)
        .await;
    assert_eq!(out["created"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn catalog_staff_edit_and_read_only_or_shop_staff_do_not(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "Aventure désert").await;
    let token = |id| jwt::issue(JWT_SECRET, id, true, 60).unwrap();
    // editor: products.write.
    let editor = token(create_user(&db, "editor@test.ma", "x", true, Some("editor")).await);
    let (status, c) = app
        .request(
            Method::POST,
            BASE,
            Some(json!({ "translations": names("Vie à la ferme") })),
            Some(&editor),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    // reception: products.read only.
    let reader = token(create_user(&db, "reception@test.ma", "x", true, Some("reception")).await);
    let (status, _) = app.request(Method::GET, BASE, None, Some(&reader)).await;
    assert_eq!(status, StatusCode::OK);
    for (method, uri, body) in [
        (
            Method::PATCH,
            format!("{BASE}/{a}"),
            json!({ "is_active": false }),
        ),
        (
            Method::PUT,
            format!("{BASE}/tree"),
            json!({ "version": "x", "items": [] }),
        ),
        (
            Method::POST,
            BASE.to_string(),
            json!({ "translations": names("X") }),
        ),
        (Method::POST, format!("{BASE}/suggested"), json!({})),
        (Method::DELETE, format!("{BASE}/{a}"), json!({})),
    ] {
        let (status, _) = app.request(method, &uri, Some(body), Some(&reader)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn pickers_only_offer_and_accept_product_categories(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let desert = create(&app, "Aventure désert").await;
    let shop = create_in(&app, SHOP, "Dattes").await;

    // The builder's lookup with the schema's `x-scope` (and the catalog editor's picker).
    let (status, l) = app
        .admin(Method::GET, "/v1/admin/lookup/category?scope=product", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{l}");
    let found: Vec<&str> = l["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(found, [desert.to_string()]);

    // The catalog editor refuses a shop category.
    let (status, p) = app
        .admin(
            Method::POST,
            "/v1/admin/catalog/activities",
            Some(json!({
                "translations": { "fr": { "name": "Quad", "slug": "quad" } },
                "category_ids": [shop],
                "details": { "location_type": "off_site", "scheduling_mode": "fixed_slots", "duration_minutes": 60,
                             "min_participants": 1, "max_participants": 8 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{p}");
    assert_eq!(
        p["error"]["details"]["fields"]["category_ids"],
        "wrong_scope"
    );
    let (status, p) = app
        .admin(
            Method::POST,
            "/v1/admin/catalog/activities",
            Some(json!({
                "translations": { "fr": { "name": "Quad", "slug": "quad" } },
                "category_ids": [desert],
                "details": { "location_type": "off_site", "scheduling_mode": "fixed_slots", "duration_minutes": 60,
                             "min_participants": 1, "max_participants": 8 },
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{p}");
    assert_eq!(p["category_ids"], json!([desert]));
    assert_eq!(p["primary_category_id"], json!(desert));

    // The builder's carousel and tiles schemas restrict their category pickers to `product`.
    let (_, types) = app
        .admin(Method::GET, "/v1/admin/section-types", None)
        .await;
    let schema = |code: &str| {
        types["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["code"] == code)
            .unwrap()["settings_schema"]
            .clone()
    };
    assert_eq!(
        schema("product_carousel")["properties"]["source"]["properties"]["rule"]["properties"]["category_id"]
            ["x-scope"],
        "product"
    );
    assert_eq!(
        schema("category_tiles")["properties"]["source"]["properties"]["ids"]["x-scope"],
        "product"
    );
}
