//! Shop categories against real Postgres: tree rules (API and database), the one-call tree move
//! and its version, slug redirects (no chains, no loops), delete with reassignment, the public
//! tree and category pages, permissions.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{JWT_SECRET, TestApp, create_user};

const BASE: &str = "/v1/admin/shop/categories";

fn names(fr: &str) -> Value {
    json!({ "fr": { "name": fr }, "en": { "name": format!("{fr} (en)") } })
}

async fn create(app: &TestApp, fr: &str, parent: Option<Uuid>) -> Uuid {
    let (status, c) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": names(fr), "parent_id": parent, "is_active": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    c["id"].as_str().unwrap().parse().unwrap()
}

async fn product(db: &PgPool, name: &str, status: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, status, booking_mode) VALUES ($1, 'shop_product', $2, 'not_bookable')")
        .bind(id)
        .bind(status)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, 'fr', $2, $3)")
        .bind(id)
        .bind(name)
        .bind(name.to_lowercase())
        .execute(db)
        .await
        .unwrap();
    id
}

async fn link(db: &PgPool, product: Uuid, category: Uuid, primary: bool) {
    sqlx::query(
        "INSERT INTO product_categories (product_id, category_id, is_primary) VALUES ($1, $2, $3)",
    )
    .bind(product)
    .bind(category)
    .bind(primary)
    .execute(db)
    .await
    .unwrap();
}

async fn version(app: &TestApp) -> String {
    let (_, l) = app.admin(Method::GET, BASE, None).await;
    l["version"].as_str().unwrap().to_string()
}

async fn redirects(db: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as("SELECT from_path, to_path FROM redirects ORDER BY from_path")
        .fetch_all(db)
        .await
        .unwrap()
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_default()
}

#[sqlx::test(migrations = "./migrations")]
async fn cycles_and_depth_are_refused_by_the_api_and_the_database(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "A", None).await;
    let b = create(&app, "B", Some(a)).await;
    let c = create(&app, "C", Some(b)).await;

    // Direct parent, deep descendant, itself.
    for parent in [b, c, a] {
        let (status, e) = app
            .admin(
                Method::PATCH,
                &format!("{BASE}/{a}"),
                Some(json!({ "parent_id": parent })),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
        assert_eq!(code(&e), "category_cycle");
    }
    // Depth 4.
    let (status, e) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": names("D"), "parent_id": c })),
        )
        .await;
    assert_eq!(
        (status, code(&e)),
        (StatusCode::UNPROCESSABLE_ENTITY, "category_too_deep")
    );
    // A tree move that would nest a two-level branch under C.
    let d = create(&app, "D", None).await;
    let _e = create(&app, "E", Some(d)).await;
    let (status, e) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": version(&app).await, "items": [{ "id": d, "parent_id": b, "sort_order": 0 }] })),
        )
        .await;
    assert_eq!(
        (status, code(&e)),
        (StatusCode::UNPROCESSABLE_ENTITY, "category_too_deep")
    );

    // The database refuses the same, at commit.
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE categories SET parent_id = $2 WHERE id = $1")
        .bind(a)
        .bind(c)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err().to_string();
    assert!(err.contains("category_cycle"), "{err}");
    let mut tx = db.begin().await.unwrap();
    sqlx::query("UPDATE categories SET parent_id = $2 WHERE id = $1")
        .bind(d)
        .bind(c)
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err().to_string();
    assert!(err.contains("category_too_deep"), "{err}");
    // Not across scopes.
    let other: Uuid =
        sqlx::query_scalar("INSERT INTO categories (scope) VALUES ('post') RETURNING id")
            .fetch_one(&db)
            .await
            .unwrap();
    let err = sqlx::query("UPDATE categories SET parent_id = $2 WHERE id = $1")
        .bind(d)
        .bind(other)
        .execute(&db)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("category_scope"), "{err}");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_tree_move_is_atomic_and_refused_on_a_stale_version(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "A", None).await;
    let b = create(&app, "B", None).await;
    let c = create(&app, "C", None).await;
    let v1 = version(&app).await;

    // One valid row and one cycle: nothing is saved.
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": v1, "items": [
                { "id": b, "parent_id": a, "sort_order": 0 },
                { "id": a, "parent_id": b, "sort_order": 0 },
            ] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(version(&app).await, v1);

    let (status, out) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": v1, "items": [
                { "id": c, "parent_id": null, "sort_order": 0 },
                { "id": b, "parent_id": a, "sort_order": 0 },
                { "id": a, "parent_id": null, "sort_order": 1 },
            ] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let parent: Option<Uuid> = sqlx::query_scalar("SELECT parent_id FROM categories WHERE id = $1")
        .bind(b)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(parent, Some(a));
    assert_eq!(out["version"].as_str().unwrap(), version(&app).await);

    // Someone else's change in between: 409.
    let (status, e) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": v1, "items": [{ "id": c, "parent_id": a, "sort_order": 3 }] })),
        )
        .await;
    assert_eq!((status, code(&e)), (StatusCode::CONFLICT, "tree_changed"));
    let audits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE action = 'category.tree_saved'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(audits, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_slug_change_redirects_without_chains_or_loops(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let id = create(&app, "Miel", None).await;
    let rename = |fr: &'static str, en: Option<&'static str>| {
        let mut t = json!({ "fr": { "name": "Miel", "slug": fr } });
        if let Some(en) = en {
            t["en"] = json!({ "name": "Honey", "slug": en });
        }
        json!({ "translations": t })
    };
    let (status, c) = app
        .admin(
            Method::PATCH,
            &format!("{BASE}/{id}"),
            Some(rename("miel-du-desert", Some("honey"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{c}");
    // One redirect per changed language.
    assert_eq!(
        redirects(&db).await,
        vec![
            ("/boutique/miel".into(), "/boutique/miel-du-desert".into()),
            ("/en/shop/miel-en".into(), "/en/shop/honey".into()),
        ]
    );
    app.admin(
        Method::PATCH,
        &format!("{BASE}/{id}"),
        Some(rename("miel-bio", None)),
    )
    .await;
    // The first redirect now points to the newest address (no chain).
    let fr: Vec<_> = redirects(&db)
        .await
        .into_iter()
        .filter(|(f, _)| f.starts_with("/boutique"))
        .collect();
    assert_eq!(
        fr,
        vec![
            ("/boutique/miel".into(), "/boutique/miel-bio".into()),
            (
                "/boutique/miel-du-desert".into(),
                "/boutique/miel-bio".into()
            ),
        ]
    );
    // Back to the first address: it is live again, nothing loops.
    app.admin(
        Method::PATCH,
        &format!("{BASE}/{id}"),
        Some(rename("miel", None)),
    )
    .await;
    let all = redirects(&db).await;
    assert!(
        all.iter()
            .all(|(f, t)| f != t && !all.iter().any(|(f2, _)| f2 == t)),
        "{all:?}"
    );
    assert!(!all.iter().any(|(f, _)| f == "/boutique/miel"));

    // Old addresses are served by the site-wide redirect layer (the map the Nuxt server reads);
    // the category endpoint itself no longer looks them up.
    let (_, map) = app
        .request(Method::GET, "/v1/seo/redirects/map", None, None)
        .await;
    assert_eq!(
        map["redirects"]["/boutique/miel-bio"]["to"],
        "/boutique/miel"
    );
    let (status, _) = app
        .request(Method::GET, "/v1/shop/categories/miel-bio", None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // An explicit slug already used elsewhere is refused; a generated one gets "-2".
    let (status, e) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": { "fr": { "name": "Autre", "slug": "miel" } } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.slug"],
        "taken"
    );
    let (_, dup) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": { "fr": { "name": "Miel" } } })),
        )
        .await;
    assert_eq!(dup["translations"]["fr"]["slug"], "miel-2");
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_moves_products_and_subcategories_and_leaves_redirects(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "Dattes", None).await;
    let a1 = create(&app, "Medjool", Some(a)).await;
    let b = create(&app, "Épicerie", None).await;
    let only_a = product(&db, "P1", "published").await;
    let both = product(&db, "P2", "published").await;
    link(&db, only_a, a, true).await;
    link(&db, both, a, true).await;
    link(&db, both, b, false).await;

    let (_, impact) = app
        .admin(Method::GET, &format!("{BASE}/{a}/impact"), None)
        .await;
    assert_eq!(impact["products"], 2);
    assert_eq!(impact["products_without_category"], 1);
    assert_eq!(impact["subcategories"].as_array().unwrap().len(), 1);
    assert_eq!(impact["requires_confirmation"], true);

    // The name must be typed.
    let (status, e) = app
        .admin(
            Method::DELETE,
            &format!("{BASE}/{a}"),
            Some(json!({ "products_to": b })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{e}");

    let (status, e) = app
        .admin(
            Method::DELETE,
            &format!("{BASE}/{a}"),
            Some(json!({ "products_to": b, "confirm_name": "Dattes" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{e}");
    // Products moved; exactly one primary each, on the replacement.
    let rows: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(
        "SELECT product_id, category_id, is_primary FROM product_categories WHERE product_id = ANY($1) ORDER BY product_id",
    )
    .bind(vec![only_a, both])
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(rows, vec![(only_a, b, true), (both, b, true)]);
    // The subcategory went to the parent (the root).
    let parent: Option<Uuid> = sqlx::query_scalar("SELECT parent_id FROM categories WHERE id = $1")
        .bind(a1)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(parent, None);
    assert_eq!(
        redirects(&db).await,
        vec![
            ("/boutique/dattes".into(), "/boutique/epicerie".into()),
            ("/en/shop/dattes-en".into(), "/en/shop/epicerie-en".into()),
        ]
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_logs WHERE action = 'category.deleted' AND entity_id = $1",
    )
    .bind(a.to_string())
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    // Deleting the replacement later rewrites the older redirect (no chain).
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!("{BASE}/{b}"),
            Some(json!({ "confirm_name": "Épicerie" })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let fr: Vec<_> = redirects(&db)
        .await
        .into_iter()
        .filter(|(f, _)| f.starts_with("/boutique"))
        .collect();
    assert_eq!(
        fr,
        vec![
            ("/boutique/dattes".into(), "/boutique".into()),
            ("/boutique/epicerie".into(), "/boutique".into()),
        ]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_is_refused_while_an_active_promotion_targets_the_category(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "Dattes", None).await;
    let promo: Uuid = sqlx::query_scalar(
        "INSERT INTO promotions (code, discount_type, discount_percent, applies_to_all) VALUES ('DATES10', 'percent', 10, false) RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO promotion_categories (promotion_id, category_id) VALUES ($1, $2)")
        .bind(promo)
        .bind(a)
        .execute(&db)
        .await
        .unwrap();
    let (status, e) = app
        .admin(Method::DELETE, &format!("{BASE}/{a}"), None)
        .await;
    assert_eq!(
        (status, code(&e)),
        (StatusCode::CONFLICT, "category_in_use")
    );
    // Once the promotion is inactive, the delete goes through (and drops the link).
    sqlx::query("UPDATE promotions SET is_active = false WHERE id = $1")
        .bind(promo)
        .execute(&db)
        .await
        .unwrap();
    let (status, _) = app
        .admin(Method::DELETE, &format!("{BASE}/{a}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn activation_needs_the_french_name(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (status, e) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": { "en": { "name": "Honey" } }, "is_active": true })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.name"],
        "required"
    );
    // An older category with no French text cannot be activated.
    let legacy: Uuid = sqlx::query_scalar(
        "INSERT INTO categories (scope, is_active) VALUES ('shop', false) RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO category_translations (category_id, locale, name, slug) VALUES ($1, 'en', 'Honey', 'honey')")
        .bind(legacy)
        .execute(&db)
        .await
        .unwrap();
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("{BASE}/{legacy}"),
            Some(json!({ "is_active": true })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.name"],
        "required_to_activate"
    );
    // Inactive drafts need French too; removing French from an active category is refused.
    let id = create(&app, "Miel", None).await;
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("{BASE}/{id}"),
            Some(json!({ "translations": { "fr": { "name": "" } } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.name"],
        "required"
    );
    // A reserved address.
    let (status, e) = app
        .admin(
            Method::POST,
            BASE,
            Some(json!({ "translations": { "fr": { "name": "Panier", "slug": "cart" } } })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.slug"],
        "reserved"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn public_counts_include_subcategories_and_an_inactive_parent_hides_its_children(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let parent = create(&app, "Dattes", None).await;
    let child = create(&app, "Medjool", Some(parent)).await;
    let hidden = create(&app, "Saison", None).await;
    app.admin(
        Method::PATCH,
        &format!("{BASE}/{hidden}"),
        Some(json!({ "show_in_menu": false })),
    )
    .await;
    let published = product(&db, "P1", "published").await;
    let draft = product(&db, "P2", "draft").await;
    link(&db, published, child, true).await;
    link(&db, draft, parent, true).await;

    let (status, list) = app
        .request(Method::GET, "/v1/shop/categories", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let find = |id: Uuid| {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id.to_string())
            .cloned()
    };
    assert_eq!(find(parent).unwrap()["product_count"], 1);
    assert_eq!(find(child).unwrap()["product_count"], 1);
    assert_eq!(find(hidden).unwrap()["in_menu"], false);

    let (_, page) = app
        .request(Method::GET, "/v1/shop/categories/dattes", None, None)
        .await;
    let c = &page["category"];
    assert_eq!(c["children"][0]["slug"], "medjool");
    assert_eq!(c["breadcrumbs"].as_array().unwrap().len(), 1);
    assert_eq!(c["indexable"], true);
    let (_, empty) = app
        .request(Method::GET, "/v1/shop/categories/saison", None, None)
        .await;
    assert_eq!(empty["category"]["indexable"], false);

    // Deactivating the parent hides the child; the child's page goes to the shop home.
    app.admin(
        Method::PATCH,
        &format!("{BASE}/{parent}"),
        Some(json!({ "is_active": false })),
    )
    .await;
    let (_, list) = app
        .request(Method::GET, "/v1/shop/categories", None, None)
        .await;
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"] != child.to_string())
    );
    let (_, page) = app
        .request(Method::GET, "/v1/shop/categories/medjool", None, None)
        .await;
    assert_eq!(page["redirect"], "/boutique");
    // Its products are no longer listed through it.
    let (_, products) = app
        .request(
            Method::GET,
            &format!("/v1/shop/products?category={child}"),
            None,
            None,
        )
        .await;
    assert_eq!(products["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn manual_order_and_pins_lead_the_category_listing(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let cat = create(&app, "Dattes", None).await;
    sqlx::query("INSERT INTO shipping_profiles (code, name, is_default) VALUES ('standard', 'Standard', true)")
        .execute(&db)
        .await
        .unwrap();
    let mut ids = vec![];
    for name in ["Alpha", "Bravo", "Charlie"] {
        let p = product(&db, name, "published").await;
        sqlx::query("INSERT INTO shop_products (product_id, product_type, shipping_profile_id) SELECT $1, 'food', id FROM shipping_profiles LIMIT 1")
            .bind(p)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO product_variants (product_id, sku, price_minor, weight_grams, is_default) VALUES ($1, $2, 1000, 100, true)")
            .bind(p)
            .bind(name)
            .execute(&db)
            .await
            .unwrap();
        ids.push(p);
    }
    let (status, out) = app
        .admin(
            Method::POST,
            &format!("{BASE}/{cat}/products"),
            Some(json!({ "product_ids": ids })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    // Charlie pinned, then Bravo, then Alpha.
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("{BASE}/{cat}/products"),
            Some(json!([
                { "product_id": ids[0], "sort_order": 2 },
                { "product_id": ids[1], "sort_order": 1 },
                { "product_id": ids[2], "sort_order": 0, "is_pinned": true },
            ])),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = app
        .request(
            Method::GET,
            &format!("/v1/shop/products?category={cat}"),
            None,
            None,
        )
        .await;
    let names: Vec<_> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["Charlie", "Bravo", "Alpha"], "{list}");
    assert_eq!(list["sort"], "manual");
    // The visitor's sort wins (pins no longer lead).
    let (_, list) = app
        .request(
            Method::GET,
            &format!("/v1/shop/products?category={cat}&sort=name"),
            None,
            None,
        )
        .await;
    assert_eq!(list["data"][0]["name"], "Alpha");
    // Removing a product keeps one primary on its other categories.
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!("{BASE}/{cat}/products/{}", ids[0]),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrations = "./migrations")]
async fn editors_reorder_and_read_only_staff_only_read(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "A", None).await;
    let b = create(&app, "B", None).await;
    let token = |id| mouzoun_api::auth::jwt::issue(JWT_SECRET, id, true, 60).unwrap();
    let editor = token(create_user(&db, "editor@test.ma", "x", true, Some("editor")).await);
    let reader = token(create_user(&db, "reception@test.ma", "x", true, Some("reception")).await);

    let (status, _) = app
        .request(
            Method::PUT,
            &format!("{BASE}/tree"),
            Some(json!({ "version": version(&app).await, "items": [{ "id": b, "parent_id": null, "sort_order": 0 }, { "id": a, "parent_id": null, "sort_order": 1 }] })),
            Some(&editor),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .request(
            Method::PATCH,
            &format!("{BASE}/{a}"),
            Some(json!({ "show_in_menu": false })),
            Some(&editor),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

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
        (Method::DELETE, format!("{BASE}/{a}"), json!({})),
    ] {
        let (status, _) = app.request(method, &uri, Some(body), Some(&reader)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}
