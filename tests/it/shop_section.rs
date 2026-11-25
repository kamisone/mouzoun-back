//! The page-builder section `shop_products` against real Postgres + Redis: rule and category
//! order, filters, registry links in every language, the shop status, one card DTO with the shop
//! listing, page cache invalidation by stock status; `product_carousel` v2 links and wishlist
//! flag; the 0033 data migration.

use std::borrow::Cow;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use mouzoun_api::{
    db::operations::OutboxEvent,
    modules::{pages::cache::PageCacheHandler, seo::routes},
    services::shop::inventory::{self, MovementRefs},
    worker::OutboxHandler,
};
use serde_json::{Value, json};
use sqlx::{
    PgPool,
    migrate::{Migration, Migrator},
};
use uuid::Uuid;

use crate::common::TestApp;

async fn setting(db: &PgPool, key: &str, value: Value) {
    sqlx::query("UPDATE settings SET value = $2 WHERE key = $1")
        .bind(key)
        .bind(value)
        .execute(db)
        .await
        .unwrap();
}

struct Fx {
    location: Uuid,
}

async fn shop(db: &PgPool) -> Fx {
    setting(db, "shop.status", json!("open")).await;
    sqlx::query("INSERT INTO shipping_profiles (code, name, is_default) VALUES ('standard', 'Standard', true)").execute(db).await.unwrap();
    let location: Uuid = sqlx::query_scalar(
        "INSERT INTO stock_locations (id, code, fulfills_online_orders, country_code) VALUES ($1, 'farm', true, 'MA') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .fetch_one(db)
    .await
    .unwrap();
    Fx { location }
}

/// A published shop product with one variant per price, each with `stock` units.
async fn product(
    db: &PgPool,
    fx: &Fx,
    name: &str,
    prices: &[i64],
    stock: i32,
) -> (Uuid, Vec<Uuid>) {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, status, booking_mode, published_at) VALUES ($1, 'shop_product', 'published', 'not_bookable', now() - interval '60 days')")
        .bind(id).execute(db).await.unwrap();
    sqlx::query("INSERT INTO shop_products (product_id, product_type, shipping_profile_id) SELECT $1, 'food', id FROM shipping_profiles LIMIT 1")
        .bind(id).execute(db).await.unwrap();
    for l in ["fr", "en", "ar"] {
        sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, $2, $3, $4)")
            .bind(id).bind(l).bind(name).bind(format!("{}-{l}", name.to_lowercase())).execute(db).await.unwrap();
    }
    let mut variants = vec![];
    for (i, price) in prices.iter().enumerate() {
        let v: Uuid = sqlx::query_scalar(
            "INSERT INTO product_variants (product_id, sku, price_minor, weight_grams, is_default) VALUES ($1, $2, $3, 500, $4) RETURNING id",
        )
        .bind(id).bind(format!("{name}-{i}")).bind(price).bind(i == 0).fetch_one(db).await.unwrap();
        if stock > 0 {
            receive(db, fx, v, stock).await;
        }
        variants.push(v);
    }
    (id, variants)
}

async fn receive(db: &PgPool, fx: &Fx, variant: Uuid, qty: i32) {
    let mut tx = db.begin().await.unwrap();
    inventory::receive(
        &mut tx,
        variant,
        fx.location,
        qty,
        None,
        &MovementRefs::default(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn category(app: &TestApp, name: &str, parent: Option<Uuid>) -> Uuid {
    let (status, c) = app
        .admin(Method::POST, "/v1/admin/shop/categories", Some(json!({ "translations": { "fr": { "name": name }, "en": { "name": format!("{name} en") }, "ar": { "name": format!("{name} ar") } }, "parent_id": parent, "is_active": true })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");
    c["id"].as_str().unwrap().parse().unwrap()
}

async fn link(db: &PgPool, product: Uuid, category: Uuid, sort: i32, pinned: bool) {
    sqlx::query("INSERT INTO product_categories (product_id, category_id, sort_order, is_pinned, is_primary) VALUES ($1, $2, $3, $4, true)")
        .bind(product).bind(category).bind(sort).bind(pinned).execute(db).await.unwrap();
}

/// A published page `vitrine` holding one section; returns its id.
async fn page_with(app: &TestApp, block_type: &str, settings: Value) -> Uuid {
    let (status, body) = app
        .admin(Method::POST, "/v1/admin/pages", Some(json!({ "page_type": "page", "translations": {
            "fr": { "title": "Vitrine", "slug": "vitrine" }, "en": { "title": "Showcase", "slug": "showcase" }, "ar": { "title": "واجهة", "slug": "wajiha" } } })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let block = json!({ "id": Uuid::now_v7(), "type": block_type, "settings": settings,
                        "content": { "fr": { "title": "Produits" }, "en": { "title": "Products" }, "ar": { "title": "منتجات" } } });
    let (_, tree) = app
        .admin(Method::GET, &format!("/v1/admin/pages/{id}/builder"), None)
        .await;
    let (status, _, body) = app
        .send(
            Method::PUT,
            &format!("/v1/admin/pages/{id}/builder"),
            Some(json!({ "blocks": [block] })),
            &[
                ("authorization", format!("Bearer {}", app.admin_token)),
                ("if-match", format!("\"{}\"", tree["draft_version"])),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
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

async fn section(app: &TestApp, locale: &str, block_type: &str) -> Option<Value> {
    let slug = match locale {
        "en" => "showcase",
        "ar" => "wajiha",
        _ => "vitrine",
    };
    let (status, page) = app
        .request(
            Method::GET,
            &format!("/v1/pages/by-slug/{slug}?locale={locale}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["type"] == block_type)
        .cloned()
}

fn rule(extra: Value) -> Value {
    let mut r = json!({ "category_id": null, "include_subcategories": true, "featured_only": false, "in_stock_only": true,
                        "on_sale_only": false, "sort": "featured", "limit": 24 });
    for (k, v) in extra.as_object().unwrap() {
        r[k] = v.clone();
    }
    json!({ "source": { "mode": "rule", "ids": [], "rule": r }, "layout": "grid_4" })
}

fn ids(s: &Value) -> Vec<String> {
    s["data"]["products"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect()
}

async fn registry_path(db: &PgPool, t: &str, id: Uuid, locale: &str) -> Option<String> {
    let mut conn = db.acquire().await.unwrap();
    let ctx = routes::Ctx::load(&mut conn, Utc::now()).await.unwrap();
    routes::path_for(&mut conn, &ctx, t, id, locale)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn a_category_rule_lists_its_products_in_category_order_with_registry_links(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let fx = shop(&db).await;
    let dates = category(&app, "Dattes", None).await;
    let medjool = category(&app, "Medjool", Some(dates)).await;
    let (a, _) = product(&db, &fx, "Alpha", &[900], 50).await;
    let (b, _) = product(&db, &fx, "Bravo", &[900], 50).await;
    let (c, _) = product(&db, &fx, "Charlie", &[900], 50).await;
    let (other, _) = product(&db, &fx, "Hors", &[900], 50).await;
    link(&db, a, dates, 1, false).await;
    link(&db, b, medjool, 0, false).await;
    link(&db, c, dates, 5, true).await;
    let _ = other;
    page_with(
        &app,
        "shop_products",
        rule(json!({ "category_id": dates, "sort": "category_order" })),
    )
    .await;
    for locale in ["fr", "en", "ar"] {
        let s = section(&app, locale, "shop_products")
            .await
            .expect("section");
        // Pinned first, then the category's order, then its subcategory's products.
        assert_eq!(
            ids(&s),
            [c.to_string(), a.to_string(), b.to_string()],
            "{locale}"
        );
        for p in s["data"]["products"].as_array().unwrap() {
            let id: Uuid = p["id"].as_str().unwrap().parse().unwrap();
            assert_eq!(
                Some(p["url"].as_str().unwrap().to_string()),
                registry_path(&db, routes::SHOP_PRODUCT, id, locale).await
            );
        }
        assert_eq!(
            Some(s["data"]["view_all_url"].as_str().unwrap().to_string()),
            registry_path(&db, routes::SHOP_CATEGORY, dates, locale).await
        );
        assert_eq!(s["data"]["shop_status"], "open");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn hidden_products_are_left_out_and_single_variants_get_quick_add(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let fx = shop(&db).await;
    let (single, variants) = product(&db, &fx, "Seul", &[900], 50).await;
    let (multi, _) = product(&db, &fx, "Multi", &[900, 1500], 50).await;
    let (sold_out, _) = product(&db, &fx, "Epuise", &[900], 0).await;
    let (archived, _) = product(&db, &fx, "Archive", &[900], 50).await;
    let (no_variant, nv) = product(&db, &fx, "Sansvariante", &[900], 50).await;
    let (later, _) = product(&db, &fx, "Bientot", &[900], 50).await;
    sqlx::query("UPDATE products SET status = 'archived' WHERE id = $1")
        .bind(archived)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE product_variants SET status = 'archived' WHERE id = $1")
        .bind(nv[0])
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE shop_products SET available_period = tstzrange(now() + interval '10 days', NULL) WHERE product_id = $1").bind(later).execute(&db).await.unwrap();
    let _ = no_variant;
    page_with(&app, "shop_products", rule(json!({ "sort": "price_asc" }))).await;
    let s = section(&app, "fr", "shop_products").await.unwrap();
    let got = ids(&s);
    assert!(
        got.contains(&single.to_string()) && got.contains(&multi.to_string()),
        "{got:?}"
    );
    for hidden in [sold_out, archived, no_variant, later] {
        assert!(
            !got.contains(&hidden.to_string()),
            "{hidden} should be hidden: {got:?}"
        );
    }
    let card = |id: Uuid| {
        s["data"]["products"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id.to_string())
            .unwrap()
            .clone()
    };
    assert_eq!(card(single)["default_variant_id"], variants[0].to_string());
    assert!(card(multi)["default_variant_id"].is_null());
    assert_eq!(card(multi)["price_is_from"], true);
    assert_eq!(card(multi)["price_minor"], 900);
    // The section's card is the shop listing's card for the same product.
    let (_, list) = app
        .request(
            Method::GET,
            "/v1/shop/products?locale=fr&per_page=60",
            None,
            None,
        )
        .await;
    let listed = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == single.to_string())
        .unwrap()
        .clone();
    assert_eq!(listed, card(single));
}

#[sqlx::test(migrations = "./migrations")]
async fn a_closed_shop_shows_no_section(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let fx = shop(&db).await;
    product(&db, &fx, "Alpha", &[900], 50).await;
    page_with(&app, "shop_products", rule(json!({}))).await;
    assert!(section(&app, "fr", "shop_products").await.is_some());
    setting(&db, "shop.status", json!("catalog_only")).await;
    app.state.cache.delete_prefix("").await.unwrap();
    let s = section(&app, "fr", "shop_products").await.unwrap();
    assert_eq!(s["data"]["shop_status"], "catalog_only");
    assert_eq!(ids(&s).len(), 1);
    setting(&db, "shop.status", json!("closed")).await;
    app.state.cache.delete_prefix("").await.unwrap();
    assert!(section(&app, "fr", "shop_products").await.is_none());
}

fn stock_event(product: Uuid) -> OutboxEvent {
    OutboxEvent {
        id: 1,
        aggregate_type: "product".into(),
        aggregate_id: product.to_string(),
        event_type: "stock.changed".into(),
        payload: json!({ "product_id": product }),
        available_at: Utc::now(),
        attempts: 0,
        last_error: None,
        processed_at: None,
        failed_at: None,
        dedupe_key: None,
        created_at: Utc::now(),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn only_an_availability_status_change_drops_the_cached_page(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let fx = shop(&db).await;
    let (plenty, pv) = product(&db, &fx, "Beaucoup", &[900], 50).await;
    let (few, fv) = product(&db, &fx, "Peu", &[900], 3).await;
    let page = page_with(&app, "shop_products", rule(json!({}))).await;
    section(&app, "fr", "shop_products").await.unwrap();
    let key = format!("pages:{page}:1:fr");
    let cached = || async { app.state.cache.get_json::<Value>(&key).await.is_some() };
    assert!(cached().await);
    let handler = PageCacheHandler::new(db.clone(), app.state.cache.clone());
    // 50 → 60: still in stock, the page stays cached.
    receive(&db, &fx, pv[0], 10).await;
    handler.handle(&stock_event(plenty)).await.unwrap();
    assert!(cached().await, "same status: kept");
    // 3 (low stock) → 13 (in stock): the page goes.
    receive(&db, &fx, fv[0], 10).await;
    handler.handle(&stock_event(few)).await.unwrap();
    assert!(!cached().await, "status changed: dropped");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_bookable_carousel_links_through_the_registry_and_skips_shop_products(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let fx = shop(&db).await;
    let (honey, _) = product(&db, &fx, "Miel", &[900], 50).await;
    let activity = Uuid::now_v7();
    let extra = Uuid::now_v7();
    for (id, kind, slug) in [(activity, "activity", "balade"), (extra, "extra", "bois")] {
        sqlx::query("INSERT INTO products (id, kind, status, published_at) VALUES ($1, $2, 'published', now())").bind(id).bind(kind).execute(&db).await.unwrap();
        for l in ["fr", "en", "ar"] {
            sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, $2, $3, $4)")
                .bind(id).bind(l).bind(slug).bind(format!("{slug}-{l}")).execute(&db).await.unwrap();
        }
    }
    // Saved straight into the draft: the builder now refuses a shop product here.
    let page = page_with(
        &app,
        "product_carousel",
        json!({ "source": { "mode": "manual", "ids": [activity, extra] }, "layout": "carousel" }),
    )
    .await;
    sqlx::query("UPDATE page_revisions SET snapshot = jsonb_set(snapshot, '{blocks,0,settings,source,ids}', $2) WHERE page_id = $1")
        .bind(page).bind(json!([activity, extra, honey])).execute(&db).await.unwrap();
    app.state.cache.delete_prefix("").await.unwrap();
    for locale in ["fr", "en", "ar"] {
        let s = section(&app, locale, "product_carousel").await.unwrap();
        let cards = s["data"]["products"].as_array().unwrap();
        // The extra has no page, the shop product has its own section.
        assert_eq!(cards.len(), 1, "{cards:?}");
        assert_eq!(cards[0]["id"], activity.to_string());
        assert_eq!(
            Some(cards[0]["url"].as_str().unwrap().to_string()),
            registry_path(&db, routes::ACTIVITY, activity, locale).await
        );
        assert_eq!(cards[0]["wishlistable"], true);
    }
    // The builder refuses a shop product in this section.
    let (_, tree) = app
        .admin(
            Method::GET,
            &format!("/v1/admin/pages/{page}/builder"),
            None,
        )
        .await;
    let mut blocks = tree["blocks"].clone();
    blocks[0]["settings"]["source"]["ids"] = json!([activity, honey]);
    let (status, _, body) = app
        .send(
            Method::PUT,
            &format!("/v1/admin/pages/{page}/builder"),
            Some(json!({ "blocks": blocks })),
            &[
                ("authorization", format!("Bearer {}", app.admin_token)),
                ("if-match", format!("\"{}\"", tree["draft_version"])),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(body.to_string().contains("kind_not_allowed"), "{body}");
}

static ALL: Migrator = sqlx::migrate!("./migrations");

fn up_to(max: i64) -> Migrator {
    let migrations: Vec<Migration> = ALL
        .iter()
        .filter(|m| m.version <= max && m.migration_type.is_up_migration())
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ..Migrator::DEFAULT
    }
}

#[sqlx::test(migrations = false)]
async fn the_0033_migration_moves_shop_products_to_their_section_and_back(db: PgPool) {
    up_to(32).run(&db).await.unwrap();
    let page: Uuid =
        sqlx::query_scalar("INSERT INTO pages (page_type) VALUES ('page') RETURNING id")
            .fetch_one(&db)
            .await
            .unwrap();
    let shop_product: Uuid = sqlx::query_scalar(
        "INSERT INTO products (kind, status) VALUES ('shop_product', 'published') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let activity: Uuid = sqlx::query_scalar(
        "INSERT INTO products (kind, status) VALUES ('activity', 'published') RETURNING id",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let block = |settings: Value| {
        let db = db.clone();
        async move {
            let id: Uuid = sqlx::query_scalar("INSERT INTO content_blocks (page_id, block_type, settings) VALUES ($1, 'product_carousel', $2) RETURNING id")
                .bind(page).bind(settings).fetch_one(&db).await.unwrap();
            sqlx::query("INSERT INTO content_block_translations (content_block_id, locale, content) VALUES ($1, 'fr', '{\"title\": \"Boutique\", \"cta_primary_label\": \"Voir\"}')")
                .bind(id).execute(&db).await.unwrap();
            id
        }
    };
    let by_rule = block(json!({ "source": { "mode": "rule", "ids": [], "rule": { "kind": "shop_product", "sort": "rating", "limit": 4 } }, "layout": "grid_3" })).await;
    let shop_only = block(
        json!({ "source": { "mode": "manual", "ids": [shop_product] }, "layout": "carousel" }),
    )
    .await;
    let mixed = block(json!({ "source": { "mode": "manual", "ids": [activity, shop_product] }, "layout": "carousel" })).await;
    let stays = block(json!({ "source": { "mode": "rule", "ids": [], "rule": { "kind": "accommodation", "sort": "sort_order", "limit": 6 } } })).await;
    ALL.run(&db).await.unwrap();
    let row = |id: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (String, Value)>(
                "SELECT block_type, settings FROM content_blocks WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let (t, s) = row(by_rule).await;
    assert_eq!(t, "shop_products");
    assert_eq!(s["source"]["rule"]["sort"], "featured");
    assert_eq!(s["source"]["rule"]["limit"], 4);
    assert_eq!(s["layout"], "grid_3");
    let content: Value = sqlx::query_scalar(
        "SELECT content FROM content_block_translations WHERE content_block_id = $1",
    )
    .bind(by_rule)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(content, json!({ "title": "Boutique" }));
    assert_eq!(row(shop_only).await.0, "shop_products");
    let (t, s) = row(mixed).await;
    assert_eq!(
        (t.as_str(), s["source"]["ids"].clone()),
        ("product_carousel", json!([activity]))
    );
    assert_eq!(row(stays).await.0, "product_carousel");
    let flagged: bool =
        sqlx::query_scalar("SELECT has_unpublished_changes FROM pages WHERE id = $1")
            .bind(page)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(flagged);
    // Down, then up again.
    ALL.undo(&db, 32).await.unwrap();
    let (t, s) = row(mixed).await;
    assert_eq!(
        (t.as_str(), s["source"]["ids"].clone()),
        ("product_carousel", json!([activity, shop_product]))
    );
    assert_eq!(row(by_rule).await.0, "product_carousel");
    ALL.run(&db).await.unwrap();
    assert_eq!(row(by_rule).await.0, "shop_products");
}
