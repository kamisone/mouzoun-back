//! The shop end to end against real Postgres + Redis: catalogue, cart, checkout, stock ledger and
//! reservations, orders (fulfilment, refunds, cancellation, edits, invoices), Stripe Checkout
//! through a local mock of the Stripe API and signed webhooks, permissions.

use std::sync::{Arc, Mutex};

use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    routing::post,
};
use chrono::{Duration, NaiveDate, Utc};
use mouzoun_api::{
    config::StripeConfig,
    db::shop::MovementReason,
    services::{
        payments::stripe as stripe_sig,
        shop::{
            emails::InvoiceHandler,
            inventory::{self, MovementRefs, NewLot, ReserveLine},
            jobs, links, orders,
            webhooks::WebhookHandler,
        },
    },
    worker::{Registry, process_batch},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{JWT_SECRET, TestApp, create_user};

const WEBHOOK_SECRET: &str = "whsec_test_shop";

// ---- Fixture ------------------------------------------------------------------------------------

struct Shop {
    location: Uuid,
    dates_500: Uuid,
    oil_1l: Uuid,
    oil_product: Uuid,
    dates_product: Uuid,
}

async fn setting(db: &PgPool, key: &str, value: Value) {
    sqlx::query("UPDATE settings SET value = $2 WHERE key = $1")
        .bind(key)
        .bind(value)
        .execute(db)
        .await
        .unwrap();
}

async fn product(
    db: &PgPool,
    name: &str,
    profile: Uuid,
    track_lots: bool,
    perishable: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO products (id, kind, status, booking_mode, published_at) VALUES ($1, 'shop_product', 'published', 'not_bookable', now())")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO shop_products (product_id, product_type, shipping_profile_id, track_lots, is_perishable, withdrawal_exemption) VALUES ($1, 'food', $2, $3, $4, CASE WHEN $4 THEN 'perishable' ELSE 'none' END)")
        .bind(id)
        .bind(profile)
        .bind(track_lots)
        .bind(perishable)
        .execute(db)
        .await
        .unwrap();
    for locale in ["fr", "en", "ar"] {
        sqlx::query("INSERT INTO product_translations (product_id, locale, name, slug) VALUES ($1, $2, $3, $4)")
            .bind(id)
            .bind(locale)
            .bind(name)
            .bind(format!("{}-{locale}", name.to_lowercase().replace(' ', "-")))
            .execute(db)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO product_taxes (product_id, tax_id) SELECT $1, id FROM taxes WHERE code = 'tva_shop'")
        .bind(id)
        .execute(db)
        .await
        .unwrap();
    id
}

async fn variant(db: &PgPool, product: Uuid, sku: &str, price: i64, weight: i32) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO product_variants (id, product_id, sku, price_minor, weight_grams, is_default) VALUES ($1, $2, $3, $4, $5, true)")
        .bind(id)
        .bind(product)
        .bind(sku)
        .bind(price)
        .bind(weight)
        .execute(db)
        .await
        .unwrap();
    id
}

async fn receive(
    db: &PgPool,
    variant: Uuid,
    location: Uuid,
    qty: i32,
    lot: Option<(&str, NaiveDate)>,
) {
    let mut tx = db.begin().await.unwrap();
    let lot = lot.map(|(code, bb)| NewLot {
        lot_code: code.into(),
        produced_on: None,
        harvested_on: None,
        best_before: Some(bb),
        notes: None,
    });
    inventory::receive(
        &mut tx,
        variant,
        location,
        qty,
        lot.as_ref(),
        &MovementRefs::default(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Published terms of sale (CGV) with one revision; returns the revision id.
async fn terms(db: &PgPool) -> Uuid {
    let page = Uuid::now_v7();
    sqlx::query("INSERT INTO pages (id, page_type, status, revision_counter) VALUES ($1, 'legal', 'published', 1)")
        .bind(page)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO page_translations (page_id, locale, title, slug) VALUES ($1, 'fr', 'Conditions générales de vente', 'cgv')")
        .bind(page)
        .execute(db)
        .await
        .unwrap();
    let rev = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO page_revisions (id, page_id, revision_number, status, snapshot, published_at)
         VALUES ($1, $2, 1, 'published', $3, now())",
    )
    .bind(rev)
    .bind(page)
    .bind(json!({ "format": 1, "page": { "id": page, "page_type": "legal", "translations": { "fr": { "title": "Conditions générales de vente", "slug": "cgv" } } },
                  "blocks": [{ "id": Uuid::now_v7(), "type": "rich_text", "is_visible": true, "settings": {},
                               "content": { "fr": { "body": "<h2>Article 1 — Objet</h2><p>Les présentes conditions régissent les ventes.</p>" } }, "items": [] }],
                  "refs": [], "rule_sources": [] }))
    .execute(db)
    .await
    .unwrap();
    setting(db, "legal.cgv_page_id", json!(page.to_string())).await;
    rev
}

/// The camp flows of the first shop version (pickup at the camp, cash on delivery, Morocco):
/// run with the Moroccan entity as seller, which the French rules do not restrict.
async fn shop(db: &PgPool) -> Shop {
    setting(db, "shop.legal_entity", json!("mouzoun_ma")).await;
    setting(db, "shop.ships_to_countries", json!(["MA"])).await;
    setting(db, "shop.stay_delivery_enabled", json!(true)).await;
    setting(db, "payments.pay_at_pickup.enabled", json!(true)).await;
    sqlx::query("INSERT INTO tax_rates (tax_class_code, country_code, rate_percent, valid_period, is_confirmed) VALUES ('food_reduced', 'MA', 20, '[2020-01-01,)', true), ('standard', 'MA', 20, '[2020-01-01,)', true)")
        .execute(db)
        .await
        .unwrap();
    terms(db).await;
    setting(db, "shop.status", json!("open")).await;
    setting(db, "payments.cod.enabled", json!(true)).await;
    setting(db, "payments.bank_transfer.enabled", json!(true)).await;
    let standard: Uuid = sqlx::query_scalar("INSERT INTO shipping_profiles (id, code, name, is_default) VALUES ($1, 'standard', 'Standard', true) RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(db)
        .await
        .unwrap();
    let liquids: Uuid = sqlx::query_scalar("INSERT INTO shipping_profiles (id, code, name) VALUES ($1, 'liquids', 'Liquides') RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(db)
        .await
        .unwrap();
    let morocco: Uuid = sqlx::query_scalar("INSERT INTO shipping_zones (id, code, country_codes) VALUES ($1, 'ma', '{MA}') RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(db)
        .await
        .unwrap();
    for (profile, code) in [(standard, "ma_std"), (liquids, "ma_liq")] {
        sqlx::query(
            "INSERT INTO shipping_rates (id, zone_id, profile_id, code, kind, carrier, amount_minor, allows_cash_on_delivery, cod_fee_minor)
             VALUES ($1, $2, $3, $4, 'flat', 'amana', 450, true, 150)",
        )
        .bind(Uuid::now_v7())
        .bind(morocco)
        .bind(profile)
        .bind(code)
        .execute(db)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO taxes (id, code, calc_type, rate_percent, is_inclusive) VALUES ($1, 'tva_shop', 'percent', 20, true)")
        .bind(Uuid::now_v7())
        .execute(db)
        .await
        .unwrap();
    let location: Uuid = sqlx::query_scalar(
        "INSERT INTO stock_locations (id, code, is_pickup_point, fulfills_online_orders, country_code, legal_entity_id)
         VALUES ($1, 'camp', true, true, 'MA', (SELECT id FROM legal_entities WHERE code = 'mouzoun_ma')) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO stock_location_translations (location_id, locale, name) VALUES ($1, 'fr', 'Boutique du camp')")
        .bind(location)
        .execute(db)
        .await
        .unwrap();
    let dates_product = product(db, "Dattes Medjool", standard, true, false).await;
    let dates_500 = variant(db, dates_product, "DATES-500", 1200, 550).await;
    let oil_product = product(db, "Huile olive", liquids, false, false).await;
    let oil_1l = variant(db, oil_product, "OIL-1L", 1800, 1100).await;
    receive(
        db,
        dates_500,
        location,
        4,
        Some(("LOT-B", NaiveDate::from_ymd_opt(2027, 9, 1).unwrap())),
    )
    .await;
    receive(
        db,
        dates_500,
        location,
        3,
        Some(("LOT-A", NaiveDate::from_ymd_opt(2027, 6, 1).unwrap())),
    )
    .await;
    receive(db, oil_1l, location, 5, None).await;
    Shop {
        location,
        dates_500,
        oil_1l,
        oil_product,
        dates_product,
    }
}

// ---- HTTP helpers (cart cookie) ---------------------------------------------------------------------

struct Visitor<'a> {
    app: &'a TestApp,
    cookie: Option<String>,
}

impl<'a> Visitor<'a> {
    fn new(app: &'a TestApp) -> Self {
        Self { app, cookie: None }
    }

    async fn call(
        &mut self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        extra: &[(&str, String)],
    ) -> (StatusCode, Value) {
        let mut headers: Vec<(&str, String)> = extra.to_vec();
        if let Some(c) = &self.cookie {
            headers.push(("cookie", c.clone()));
        }
        let (status, h, json) = self.app.send(method, uri, body, &headers).await;
        if let Some(set) = h.get("set-cookie").and_then(|v| v.to_str().ok()) {
            self.cookie = set.split(';').next().map(str::to_string);
        }
        (status, json)
    }

    async fn add(&mut self, variant: Uuid, qty: i32) -> Value {
        let (s, cart) = self
            .call(
                Method::POST,
                "/v1/shop/cart/items",
                Some(json!({ "variant_id": variant, "quantity": qty })),
                &[],
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{cart}");
        cart
    }

    async fn pickup(&mut self) -> Value {
        let (s, cart) = self
            .call(
                Method::PUT,
                "/v1/shop/cart/fulfillment",
                Some(json!({ "method": "pickup" })),
                &[],
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{cart}");
        cart
    }

    async fn ship_to_morocco(&mut self) -> Value {
        let (s, cart) = self
            .call(
                Method::PUT,
                "/v1/shop/cart/fulfillment",
                Some(json!({ "method": "shipping", "shipping_address": {
                    "first_name": "Amina", "last_name": "Ait Ali", "line1": "Rue 1", "city": "Rissani",
                    "country_code": "MA", "phone": "+212600000000", "postal_code": "52450" } })),
                &[],
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{cart}");
        cart
    }

    async fn checkout(&mut self, method: &str, key: &str) -> (StatusCode, Value) {
        self.call(
            Method::POST,
            "/v1/shop/checkout",
            Some(json!({ "payment_method": method, "email": "amina@example.com", "first_name": "Amina",
                         "last_name": "Ait Ali", "phone": "+212600000000", "accept_terms": true })),
            &[("idempotency-key", key.to_string())],
        )
        .await
    }
}

async fn level(db: &PgPool, variant: Uuid, location: Uuid) -> (i32, i32) {
    sqlx::query_as(
        "SELECT on_hand, reserved FROM inventory_levels WHERE variant_id = $1 AND location_id = $2",
    )
    .bind(variant)
    .bind(location)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn order_id(db: &PgPool, number: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM orders WHERE number = $1")
        .bind(number)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn order_row(db: &PgPool, id: Uuid) -> (String, String, String, String) {
    sqlx::query_as(
        "SELECT status, payment_status, fulfillment_status, risk_level FROM orders WHERE id = $1",
    )
    .bind(id)
    .fetch_one(db)
    .await
    .unwrap()
}

fn registry(db: &PgPool) -> Registry {
    Registry::new()
        .register(Arc::new(WebhookHandler::new(
            db.clone(),
            Default::default(),
        )))
        .register(Arc::new(InvoiceHandler::new(
            db.clone(),
            Some(Arc::new(
                mouzoun_api::infra::storage::memory::InMemoryStorage::default(),
            )),
        )))
}

/// Runs the outbox until nothing is due at `at`. Callers pass a time a little ahead: rows are
/// stamped with the database clock (Docker's may run ahead of the test process).
async fn drain(db: &PgPool, at: chrono::DateTime<Utc>) {
    let r = registry(db);
    let at = at + Duration::seconds(5);
    while process_batch(db, &r, 50, 8, at).await.unwrap().claimed > 0 {}
}

/// Every level's ledger sums to its on_hand; lots sum to on_hand for lot-tracked variants.
async fn assert_ledger(db: &PgPool) {
    let bad: Vec<(Uuid, i64, i32)> = sqlx::query_as(
        "SELECT l.variant_id, COALESCE((SELECT sum(delta) FROM inventory_movements m WHERE m.variant_id = l.variant_id AND m.location_id = l.location_id), 0)::bigint, l.on_hand
         FROM inventory_levels l
         WHERE COALESCE((SELECT sum(delta) FROM inventory_movements m WHERE m.variant_id = l.variant_id AND m.location_id = l.location_id), 0) <> l.on_hand",
    )
    .fetch_all(db)
    .await
    .unwrap();
    assert!(bad.is_empty(), "ledger differs from on_hand: {bad:?}");
    let lots: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT l.variant_id FROM inventory_levels l JOIN product_variants v ON v.id = l.variant_id
         JOIN shop_products sp ON sp.product_id = v.product_id AND sp.track_lots
         WHERE (SELECT COALESCE(sum(quantity), 0) FROM inventory_lots i WHERE i.variant_id = l.variant_id AND i.location_id = l.location_id) <> l.on_hand",
    )
    .fetch_all(db)
    .await
    .unwrap();
    assert!(lots.is_empty(), "lots differ from on_hand: {lots:?}");
}

// ---- Catalogue and modes -----------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn catalog_only_shows_the_products_but_hides_buying(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    setting(&db, "shop.status", json!("catalog_only")).await;
    let mut v = Visitor::new(&app);
    let (status, list) = v
        .call(Method::GET, "/v1/shop/products?locale=fr", None, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["total"], 2);
    let (status, p) = v
        .call(
            Method::GET,
            "/v1/shop/products/by-slug/dattes-medjool-fr",
            None,
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(p["variants"][0]["availability"], "in_stock");
    assert!(p["variants"][0].get("cost_minor").is_none());
    for (method, uri) in [
        (Method::GET, "/v1/shop/cart"),
        (Method::POST, "/v1/shop/cart/items"),
        (Method::POST, "/v1/shop/checkout"),
    ] {
        let (status, _) = v
            .call(method, uri, Some(json!({ "variant_id": s.dates_500 })), &[])
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    let (status, _) = v
        .call(
            Method::POST,
            "/v1/shop/notify-me",
            Some(json!({ "email": "fan@example.com" })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let source: String = sqlx::query_scalar(
        "SELECT source FROM newsletter_subscribers WHERE email = 'fan@example.com'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(source, "shop");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_admin_builds_a_product_with_an_option_matrix(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    shop(&db).await;
    let (status, p) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/products",
            Some(json!({
                "translations": { "fr": { "name": "Dattes Boufeggous" }, "en": { "name": "Boufeggous dates" } },
                "shop": { "product_type": "food", "allergens": ["nuts"], "is_perishable": false },
                "variant": { "sku": "boufeg-500", "price_minor": 900, "weight_grams": 550 }
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{p}");
    let id = p["id"].as_str().unwrap().to_string();
    let mode: String = sqlx::query_scalar("SELECT booking_mode FROM products WHERE id = $1::uuid")
        .bind(&id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(mode, "not_bookable", "shop products are never bookable");
    let (status, p) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/products/{id}/options"),
            Some(json!([{ "code": "size", "names": { "fr": "Taille" },
                          "values": [{ "code": "500g", "labels": { "fr": "500 g" } }, { "code": "1kg", "labels": { "fr": "1 kg" } }] }])),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    let (status, g) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/products/{id}/variants/generate"),
            Some(json!({ "price_minor": 900, "weight_grams": 550, "sku_prefix": "BOUFEG" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{g}");
    // The first variant had no option value: it is archived, two new ones exist.
    assert_eq!(
        (g["created"].as_i64(), g["archived"].as_i64()),
        (Some(2), Some(1))
    );
    let active: Vec<&Value> = g["product"]["variants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["status"] == "active")
        .collect();
    assert_eq!(active.len(), 2);
    // Two active variants with the same combination are refused.
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/shop/products/{id}/variants"),
            Some(json!([{ "id": active[1]["id"], "options": { "size": active[0]["options"]["size"] } }])),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{e}");
    // Regenerating keeps what exists.
    let (_, g2) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/products/{id}/variants/generate"),
            Some(json!({ "price_minor": 900, "weight_grams": 550 })),
        )
        .await;
    assert_eq!(g2["created"], 0);
    let mut body = p.clone();
    body["status"] = json!("published");
    body["shop"]["product_type"] = json!("food");
    body["translations"] =
        json!({ "fr": { "name": "Dattes Boufeggous", "slug": "dattes-boufeggous" } });
    // Food is published only with its French mandatory information.
    let (status, out) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/products/{id}"),
            Some(body.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(out["error"]["code"], "publish_incomplete");
    body["translations"]["fr"]["description"] = json!("Dattes moelleuses du Tafilalet.");
    body["shop"]["translations"] = json!({ "fr": { "ingredients": "Dattes (100 %)", "storage_instructions": "Au frais et au sec",
                                                   "producer_name": "Coopérative Mouzoun, M'ssici, Alnif (Maroc)" } });
    let patches: Vec<Value> = active
        .iter()
        .map(|v| json!({ "id": v["id"], "net_quantity": "500", "net_unit": "g", "country_of_origin": "MA" }))
        .collect();
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/shop/products/{id}/variants"),
            Some(json!(patches)),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{e}");
    let (status, out) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/products/{id}"),
            Some(body),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (status, page) = app
        .request(
            Method::GET,
            "/v1/shop/products/by-slug/dattes-boufeggous",
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["variants"].as_array().unwrap().len(), 2);
    assert_eq!(page["options"][0]["values"].as_array().unwrap().len(), 2);
    assert_eq!(page["food"]["allergens"], json!(["nuts"]));
}

#[sqlx::test(migrations = "./migrations")]
async fn library_variations_build_the_variants_and_follow_library_edits(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    // A colour variation in the library: swatches, a price adjustment, French name required.
    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/variations",
            Some(
                json!({ "names": { "en": "Colour" }, "values": [{ "labels": { "fr": "Rouge" } }] }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let (status, colour) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/variations",
            Some(json!({ "names": { "fr": "Couleur", "en": "Colour" }, "display_type": "swatch",
                         "values": [{ "labels": { "fr": "Rouge", "en": "Red" }, "swatch_color": "#B22222" },
                                    { "labels": { "fr": "Bleu nuit" }, "swatch_color": "#191970", "price_adjustment_minor": 300 }] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{colour}");
    assert_eq!(colour["code"], "couleur");
    assert_eq!(colour["values"][1]["code"], "bleu_nuit");
    assert_eq!(colour["values"][0]["swatch_color"], "#b22222");
    let (status, e) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/variations",
            Some(json!({ "names": { "fr": "Motif" }, "values": [{ "labels": { "fr": "Vert" }, "swatch_color": "vert" }] })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "bad colour: {e}");
    let (_, list) = app
        .admin(Method::GET, "/v1/admin/shop/variations", None)
        .await;
    let weight = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == "weight")
        .expect("seeded")
        .clone();
    let w = |code: &str| {
        weight["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["code"] == code)
            .unwrap()["id"]
            .clone()
    };

    // The dates link Poids (two of its three values, 1 kg by default) and Couleur.
    let id = s.dates_product;
    let (status, p) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/products/{id}/options"),
            Some(json!([
                { "attribute_id": weight["id"], "default_value": "1kg",
                  "values": [{ "variation_value_id": w("500g") }, { "variation_value_id": w("1kg") }] },
                { "attribute_id": colour["id"],
                  "values": [{ "variation_value_id": colour["values"][0]["id"] }, { "variation_value_id": colour["values"][1]["id"] }] }
            ])),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert_eq!(p["options"][0]["names"]["fr"], "Poids");
    assert_eq!(p["options"][0]["default_value"], "1kg");
    assert_eq!(p["options"][1]["display_type"], "swatch");
    assert_eq!(p["options"][1]["values"][1]["labels"]["fr"], "Bleu nuit");
    // A library value of another variation is refused; a variation is linked once.
    let (status, _) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/products/{id}/options"),
            Some(json!([{ "attribute_id": weight["id"], "values": [{ "variation_value_id": colour["values"][0]["id"] }] }])),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // 2 × 2 variants: base 10 €, +3 € for the blue one; the default is 1 kg + the first colour.
    let (status, g) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/products/{id}/variants/generate"),
            Some(json!({ "price_minor": 1000, "weight_grams": 550, "sku_prefix": "MEDJ" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(g["created"], 4);
    let active: Vec<Value> = g["product"]["variants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["status"] == "active")
        .cloned()
        .collect();
    let price = |weight: &str, colour: &str| {
        active
            .iter()
            .find(|v| v["options"]["weight"] == weight && v["options"]["couleur"] == colour)
            .unwrap()
            .clone()
    };
    assert_eq!(price("500g", "rouge")["price_minor"], 1000);
    assert_eq!(price("1kg", "bleu_nuit")["price_minor"], 1300);
    assert_eq!(price("1kg", "rouge")["is_default"], true);
    assert_eq!(active.iter().filter(|v| v["is_default"] == true).count(), 1);

    // A library edit reaches the product; repricing applies the new adjustment.
    let mut edit = colour.clone();
    edit["names"]["fr"] = json!("Teinte");
    edit["values"][1]["labels"]["fr"] = json!("Bleu");
    edit["values"][1]["price_adjustment_minor"] = json!(500);
    let (status, out) = app
        .admin(
            Method::PUT,
            &format!(
                "/v1/admin/shop/variations/{}",
                colour["id"].as_str().unwrap()
            ),
            Some(edit.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["product_count"], 1);
    let (status, g) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/products/{id}/variants/generate"),
            Some(json!({ "price_minor": 1000, "weight_grams": 550, "reprice": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(
        (g["created"].as_i64(), g["repriced"].as_i64()),
        (Some(0), Some(2))
    );
    let (status, page) = app
        .request(
            Method::GET,
            "/v1/shop/products/by-slug/dattes-medjool-fr",
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let opt = page["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["code"] == "couleur")
        .unwrap();
    assert_eq!(opt["name"], "Teinte");
    assert_eq!(opt["display_type"], "swatch");
    assert_eq!(opt["values"][1]["label"], "Bleu");
    assert_eq!(opt["values"][0]["swatch_color"], "#b22222");

    // Used values and variations cannot be deleted.
    let mut drop_value = edit.clone();
    drop_value["values"] = json!([edit["values"][0]]);
    let (status, e) = app
        .admin(
            Method::PUT,
            &format!(
                "/v1/admin/shop/variations/{}",
                colour["id"].as_str().unwrap()
            ),
            Some(drop_value),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["error"]["code"], "variation_in_use");
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!(
                "/v1/admin/shop/variations/{}",
                colour["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // An unused one can.
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!(
                "/v1/admin/shop/variations/{}",
                weight["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "weight is linked too");
    let packaging = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["code"] == "packaging")
        .unwrap();
    let (status, _) = app
        .admin(
            Method::DELETE,
            &format!(
                "/v1/admin/shop/variations/{}",
                packaging["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ---- Checkout, idempotency, pickup ---------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_guest_orders_for_pickup_and_collects_at_the_camp(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.dates_500, 2).await;
    let cart = v.pickup().await;
    assert_eq!(cart["totals"]["total_minor"], 2400);
    assert_eq!(cart["totals"]["tax_minor"], 400, "20 % included");
    let (status, out) = v.checkout("pay_at_pickup", "key-1").await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let number = out["order"]["number"].as_str().unwrap().to_string();
    // Same key: same order, nothing new.
    let (status, again) = v.checkout("pay_at_pickup", "key-1").await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again["order"]["number"], number.as_str());
    let orders: i64 = sqlx::query_scalar("SELECT count(*) FROM orders")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(orders, 1);
    let id = order_id(&db, &number).await;
    assert_eq!(
        level(&db, s.dates_500, s.location).await,
        (7, 2),
        "reserved, not sold yet"
    );
    assert_eq!(order_row(&db, id).await.0, "open");

    // The guest page (signed link) shows the order.
    let token = links::order_token(JWT_SECRET, &number);
    let (status, page) = app
        .request(
            Method::GET,
            &format!("/v1/shop/orders/{number}?t={token}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/v1/shop/orders/{number}?email=other@example.com"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Staff prepare it, the customer comes with the code and pays at the counter.
    let (status, d) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/fulfillments"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let fid: Uuid = d["created_fulfillment_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        level(&db, s.dates_500, s.location).await,
        (5, 0),
        "stock left at fulfilment (offline payment)"
    );
    // FEFO: the 2 jars come from LOT-A (best before June) although LOT-B was received first.
    let lot_a: i32 =
        sqlx::query_scalar("SELECT quantity FROM inventory_lots WHERE lot_code = 'LOT-A'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(lot_a, 1);
    let (status, _) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/shop/fulfillments/{fid}"),
            Some(json!({ "action": "ready_for_pickup" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/shop/fulfillments/{fid}"),
            Some(json!({ "action": "pick_up", "code": "000000" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{e}");
    let code = orders::pickup_code(&links::pickup_secret(JWT_SECRET), fid);
    let (status, e) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/shop/fulfillments/{fid}"),
            Some(json!({ "action": "pick_up", "code": code })),
        )
        .await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("balance_due"))
    );
    let (status, d) = app
        .admin(
            Method::PATCH,
            &format!("/v1/admin/shop/fulfillments/{fid}"),
            Some(json!({ "action": "pick_up", "code": code, "collect_payment": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(
        order_row(&db, id).await,
        (
            "completed".into(),
            "paid".into(),
            "fulfilled".into(),
            "normal".into()
        )
    );
    drain(&db, Utc::now()).await;
    let invoices: Vec<String> =
        sqlx::query_scalar("SELECT number FROM invoices WHERE order_id = $1")
            .bind(id)
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(invoices.len(), 1, "invoice issued once paid");
    assert_ledger(&db).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn blocking_issues_refuse_the_checkout(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    let mut v = Visitor::new(&app);
    let cart = v.add(s.oil_1l, 9).await;
    assert_eq!(cart["items"][0]["issues"][0]["code"], "quantity_limited");
    assert_eq!(cart["can_checkout"], false);
    v.pickup().await;
    let (status, e) = v.checkout("pay_at_pickup", "k").await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("cart_has_issues")),
        "{e}"
    );
    // COD needs shipping.
    let (status, e) = v.checkout("cash_on_delivery", "k2").await;
    assert!(status.is_client_error(), "{e}");
}

#[sqlx::test(migrations = "./migrations")]
async fn two_concurrent_checkouts_for_the_last_jar_sell_it_once(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    // Leave exactly one bottle.
    let mut tx = db.begin().await.unwrap();
    inventory::apply_movement(
        &mut tx,
        s.oil_1l,
        s.location,
        None,
        -4,
        MovementReason::Damage,
        &MovementRefs::default(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut a = Visitor::new(&app);
    let mut b = Visitor::new(&app);
    for v in [&mut a, &mut b] {
        v.add(s.oil_1l, 1).await;
        v.pickup().await;
    }
    let (ra, rb) = tokio::join!(
        a.checkout("pay_at_pickup", "ka"),
        b.checkout("pay_at_pickup", "kb")
    );
    let statuses = [ra.0, rb.0];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{ra:?} {rb:?}"
    );
    let refused = if ra.0 == StatusCode::OK { &rb.1 } else { &ra.1 };
    assert!(
        ["out_of_stock", "cart_has_issues"]
            .contains(&refused["error"]["code"].as_str().unwrap_or("")),
        "{refused}"
    );
    assert_eq!(level(&db, s.oil_1l, s.location).await, (1, 1));
}

// ---- Inventory rules -----------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn the_database_refuses_an_oversell_and_rewrites_of_the_ledger(db: PgPool) {
    let s = shop(&db).await;
    let r = sqlx::query("UPDATE inventory_levels SET reserved = on_hand + 1 WHERE variant_id = $1")
        .bind(s.oil_1l)
        .execute(&db)
        .await;
    assert!(r.is_err(), "CHECK reserved <= on_hand + backorder_limit");
    assert!(
        sqlx::query("UPDATE inventory_movements SET delta = 99")
            .execute(&db)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM inventory_movements")
            .execute(&db)
            .await
            .is_err()
    );

    // Backorder: up to the limit, not one more.
    let mut tx = db.begin().await.unwrap();
    inventory::count(
        &mut tx,
        s.oil_1l,
        s.location,
        None,
        0,
        &MovementRefs::default(),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE inventory_levels SET backorder_limit = 2 WHERE variant_id = $1")
        .bind(s.oil_1l)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let order = Uuid::now_v7();
    let mut tx = db.begin().await.unwrap();
    fake_order(&mut tx, order, s.oil_1l, s.oil_product, 3).await;
    let line = ReserveLine {
        order_item_id: item_of(&mut tx, order).await,
        variant_id: s.oil_1l,
        quantity: 3,
    };
    assert!(
        inventory::reserve(&mut tx, order, std::slice::from_ref(&line), None, None)
            .await
            .is_err(),
        "3 > backorder 2"
    );
    tx.rollback().await.unwrap();
    let mut tx = db.begin().await.unwrap();
    fake_order(&mut tx, order, s.oil_1l, s.oil_product, 2).await;
    let line = ReserveLine {
        order_item_id: item_of(&mut tx, order).await,
        variant_id: s.oil_1l,
        quantity: 2,
    };
    inventory::reserve(&mut tx, order, &[line], None, None)
        .await
        .unwrap();
    inventory::consume(&mut tx, order, None, Utc::now())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        level(&db, s.oil_1l, s.location).await,
        (-2, 0),
        "sold on backorder"
    );
    assert_ledger(&db).await;
}

async fn fake_order(tx: &mut sqlx::PgConnection, id: Uuid, variant: Uuid, product: Uuid, qty: i32) {
    let customer: Uuid = sqlx::query_scalar(
        "INSERT INTO customers (id, email, first_name) VALUES ($1, $2, 'T') RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(format!("{id}@example.com"))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO orders (id, number, customer_id, channel, status, email, locale, currency, fulfillment_method, subtotal_minor, total_minor)
         VALUES ($1, 'MZS-27-' || lpad((floor(random() * 900000) + 100000)::text, 6, '0'), $2, 'phone', 'pending', 'x@example.com', 'fr', 'EUR', 'shipping', $3, $3)",
    )
    .bind(id)
    .bind(customer)
    .bind(1000 * i64::from(qty))
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO order_items (id, order_id, variant_id, product_id, sku, quantity, unit_price_minor, subtotal_minor, total_minor)
         VALUES ($1, $2, $3, $4, 'X', $5, 1000, $6, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(variant)
    .bind(product)
    .bind(qty)
    .bind(1000 * i64::from(qty))
    .execute(&mut *tx)
    .await
    .unwrap();
}

async fn item_of(tx: &mut sqlx::PgConnection, order: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT id FROM order_items WHERE order_id = $1")
        .bind(order)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn transfers_keep_the_total_and_fefo_picks_the_earliest_lot(db: PgPool) {
    let s = shop(&db).await;
    let store: Uuid = sqlx::query_scalar("INSERT INTO stock_locations (id, code, fulfills_online_orders, country_code) VALUES ($1, 'city', false, 'MA') RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(&db)
        .await
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    let transfer = inventory::transfer(
        &mut tx,
        s.dates_500,
        s.location,
        store,
        4,
        &MovementRefs::default(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let total: i64 = sqlx::query_scalar(
        "SELECT sum(on_hand)::bigint FROM inventory_levels WHERE variant_id = $1",
    )
    .bind(s.dates_500)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(total, 7);
    let moved: Vec<(i32, String)> = sqlx::query_as(
        "SELECT m.delta, l.lot_code::text FROM inventory_movements m JOIN inventory_lots l ON l.id = m.lot_id WHERE m.transfer_id = $1 ORDER BY m.id",
    )
    .bind(transfer)
    .fetch_all(&db)
    .await
    .unwrap();
    // FEFO: all 3 of LOT-A (June) leave first, then 1 of LOT-B.
    assert_eq!(
        moved,
        vec![
            (-3, "LOT-A".into()),
            (-1, "LOT-B".into()),
            (3, "LOT-A".into()),
            (1, "LOT-B".into())
        ]
    );
    assert_ledger(&db).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn expired_holds_are_released_and_unpaid_orders_cancelled(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.dates_500, 3).await;
    v.ship_to_morocco().await;
    let (status, out) = v.checkout("bank_transfer", "kt").await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    assert_eq!(level(&db, s.dates_500, s.location).await, (7, 3));
    let later = Utc::now() + Duration::days(6);
    assert_eq!(
        jobs::expire_holds(&db, &app.state.shop_payments, later)
            .await
            .unwrap(),
        1
    );
    assert_eq!(level(&db, s.dates_500, s.location).await, (7, 0));
    let (status, pay, _, _) = order_row(&db, id).await;
    assert_eq!((status.as_str(), pay.as_str()), ("cancelled", "voided"));
    assert_ledger(&db).await;
}

// ---- Orders: refunds, cancellation, edits, invoices -----------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn refunds_never_exceed_what_was_paid_and_cancelling_restocks(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.oil_1l, 2).await;
    v.ship_to_morocco().await;
    let (_, out) = v.checkout("bank_transfer", "kr").await;
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    let total: i64 = sqlx::query_scalar("SELECT total_minor FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(total, 3600 + 450);
    let (status, d) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/payments"),
            Some(
                json!({ "provider": "bank_transfer", "amount_minor": total, "reference": "VIR-1" }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["payment_status"], "paid");
    assert_eq!(
        level(&db, s.oil_1l, s.location).await,
        (3, 0),
        "sold when paid"
    );
    let (status, e) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/refunds"),
            Some(json!({ "amount_minor": total + 1 })),
        )
        .await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("refund_exceeds_balance"))
    );
    let (status, d) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/refunds"),
            Some(json!({ "shipping_minor": 450, "reason": "late" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["payment_status"], "partially_refunded");
    assert_eq!(d["refundable_minor"], 3600);
    // Cancel: the rest is refunded, the bottles go back on the shelf.
    let (status, d) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/cancel"),
            Some(json!({ "reason": "customer", "restock": true, "refund": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["status"], "cancelled");
    assert_eq!(d["payment_status"], "refunded");
    assert_eq!(d["cancel_outcome"]["restocked"], 2);
    assert_eq!(level(&db, s.oil_1l, s.location).await, (5, 0));
    drain(&db, Utc::now()).await;
    // Invoice then two credit notes: one gap-free sequence of the French entity (prefix F),
    // credit notes pointing to the invoice, VAT prorated, PDFs sealed.
    let numbers: Vec<(String, String)> =
        sqlx::query_as("SELECT number, kind FROM invoices ORDER BY number")
            .fetch_all(&db)
            .await
            .unwrap();
    let year = chrono::Datelike::year(&mouzoun_api::camp_time::local(Utc::now()));
    // This fixture sells as the Moroccan entity (no prefix).
    let expected: Vec<String> = (1..=numbers.len())
        .map(|n| format!("{year}-{n:06}"))
        .collect();
    assert_eq!(
        numbers.iter().map(|n| n.0.clone()).collect::<Vec<_>>(),
        expected,
        "{numbers:?}"
    );
    let unlinked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invoices WHERE kind = 'credit_note' AND credit_note_for_invoice_id IS NULL",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(unlinked, 0);
    let unsealed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM invoices WHERE pdf_sha256 IS NULL")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(unsealed, 0, "every document is stored and sealed");
    let fr: uuid::Uuid =
        sqlx::query_scalar("SELECT id FROM legal_entities WHERE code = 'mouzoun_fr'")
            .fetch_one(&db)
            .await
            .unwrap();
    let mut tx = db.begin().await.unwrap();
    let next = mouzoun_api::services::numbering::next_invoice_number(&mut tx, fr, year)
        .await
        .unwrap();
    assert_eq!(
        next,
        format!("F{year}-000001"),
        "each entity has its own sequence"
    );
    assert_ledger(&db).await;
}

#[sqlx::test(migrations = "./migrations")]
async fn editing_a_cod_order_adjusts_its_reservations(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.oil_1l, 1).await;
    v.ship_to_morocco().await;
    let (status, out) = v.checkout("cash_on_delivery", "kc").await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    let (cod, total): (i64, i64) =
        sqlx::query_as("SELECT cod_fee_minor, total_minor FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!((cod, total), (150, 1800 + 450 + 150));
    assert_eq!(level(&db, s.oil_1l, s.location).await, (5, 1));
    let expires: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT expires_at FROM inventory_reservations WHERE order_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(expires.is_none(), "held until staff confirm or cancel");
    let (status, d) = app
        .admin(
            Method::PUT,
            &format!("/v1/admin/shop/orders/{id}/edit"),
            Some(json!({ "lines": [{ "variant_id": s.oil_1l, "quantity": 3 }, { "variant_id": s.dates_500, "quantity": 1 }], "note": "phone call" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(level(&db, s.oil_1l, s.location).await, (5, 3));
    assert_eq!(level(&db, s.dates_500, s.location).await, (7, 1));
    assert!(
        d["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "edited")
    );
    let (status, d) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/confirm"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["status"], "open");
    // Total CHECK: the database refuses an inconsistent total.
    assert!(
        sqlx::query("UPDATE orders SET total_minor = total_minor + 1 WHERE id = $1")
            .bind(id)
            .execute(&db)
            .await
            .is_err()
    );
}

// ---- Stripe Checkout (mock API + signed webhooks) ---------------------------------------------------

/// `(path, Idempotency-Key, form fields)` of a request to the mock.
type Call = (String, Option<String>, Vec<(String, String)>);

#[derive(Default)]
struct Mock {
    calls: Vec<Call>,
    coupons: std::collections::HashMap<String, i64>,
}

type MockState = Arc<Mutex<Mock>>;

async fn mock_stripe(
    State(m): State<MockState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Vec<(String, String)>>,
) -> (StatusCode, Json<Value>) {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut m = m.lock().unwrap();
    m.calls.push((path.clone(), key, form.clone()));
    let get = |k: &str| form.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
    match path.as_str() {
        "coupons" => {
            let id = format!("co_{}", m.coupons.len() + 1);
            m.coupons
                .insert(id.clone(), get("amount_off").unwrap().parse().unwrap());
            (StatusCode::OK, Json(json!({ "id": id })))
        }
        "checkout/sessions" => {
            let mut total = 0i64;
            for i in 0..20 {
                if let Some(a) = get(&format!("line_items[{i}][price_data][unit_amount]")) {
                    total += a.parse::<i64>().unwrap()
                        * get(&format!("line_items[{i}][quantity]"))
                            .unwrap()
                            .parse::<i64>()
                            .unwrap();
                }
            }
            total += get("shipping_options[0][shipping_rate_data][fixed_amount][amount]")
                .map_or(0, |a| a.parse().unwrap());
            if let Some(c) = get("discounts[0][coupon]") {
                total -= m.coupons[&c];
            }
            let id = format!("cs_test_{}", m.calls.len());
            (
                StatusCode::OK,
                Json(
                    json!({ "id": id, "url": format!("https://checkout.stripe.test/{id}"), "amount_total": total }),
                ),
            )
        }
        p if p.ends_with("/expire") => (StatusCode::OK, Json(json!({ "status": "expired" }))),
        "refunds" => (
            StatusCode::OK,
            Json(json!({ "id": "re_1", "status": "pending" })),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": { "message": "unknown" } })),
        ),
    }
}

async fn stripe_app(db: &PgPool) -> (TestApp, MockState) {
    let mock: MockState = Arc::default();
    let router = Router::new()
        .route("/v1/{*path}", post(mock_stripe))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let app = TestApp::with_config(db.clone(), |c| {
        c.booking.fake_payments = false;
        c.booking.stripe = Some(StripeConfig {
            secret_key: "sk_test_shop_secret".into(),
            publishable_key: "pk_test_shop".into(),
            webhook_secret: WEBHOOK_SECRET.into(),
            api_base: format!("http://{addr}"),
        });
    })
    .await;
    setting(db, "payments.stripe.enabled", json!(true)).await;
    (app, mock)
}

async fn send_event(app: &TestApp, event: &Value, secret: &str) -> StatusCode {
    let body = event.to_string();
    let sig = stripe_sig::sign(body.as_bytes(), secret, Utc::now().timestamp());
    let req = axum::http::Request::builder()
        .method(Method::POST)
        .uri("/v1/webhooks/stripe")
        .header("stripe-signature", sig)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap();
    use tower::ServiceExt;
    app.router.clone().oneshot(req).await.unwrap().status()
}

fn session_event(id: &str, kind: &str, session: &str, amount: i64, extra: Value) -> Value {
    let mut object = json!({ "id": session, "object": "checkout.session", "payment_status": "paid", "amount_total": amount,
                             "currency": "eur", "payment_intent": "pi_test_1" });
    if let (Some(o), Some(e)) = (object.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    json!({ "id": id, "type": kind, "data": { "object": object } })
}

async fn card_order(app: &TestApp, s: &Shop, key: &str) -> (Uuid, String, i64) {
    let mut v = Visitor::new(app);
    v.add(s.dates_500, 2).await;
    v.ship_to_morocco().await;
    let (status, out) = v.checkout("stripe", key).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(
        out["order"]["redirect_url"]
            .as_str()
            .unwrap()
            .starts_with("https://checkout.stripe.test/")
    );
    let id = order_id(&app.db, out["order"]["number"].as_str().unwrap()).await;
    let (session, total): (String, i64) = sqlx::query_as(
        "SELECT p.provider_session_id, o.total_minor FROM payments p JOIN orders o ON o.id = p.order_id WHERE p.order_id = $1",
    )
    .bind(id)
    .fetch_one(&app.db)
    .await
    .unwrap();
    (id, session, total)
}

#[sqlx::test(migrations = "./migrations")]
async fn a_card_payment_is_applied_once_from_the_signed_webhook(db: PgPool) {
    let (app, mock) = stripe_app(&db).await;
    let s = shop(&db).await;
    let (id, session, total) = card_order(&app, &s, "card-1").await;
    {
        let m = mock.lock().unwrap();
        let (_, key, form) = m.calls.iter().find(|c| c.0 == "checkout/sessions").unwrap();
        assert_eq!(
            key.as_deref(),
            Some(format!("order-{id}").as_str()),
            "Idempotency-Key on create"
        );
        assert!(
            form.iter()
                .any(|(k, v)| k == "client_reference_id" && v == &id.to_string())
        );
        assert!(
            form.iter().any(
                |(k, v)| k == "payment_intent_data[metadata][order_id]" && v == &id.to_string()
            )
        );
    }
    assert_eq!(order_row(&db, id).await.0, "pending");
    let event = session_event(
        "evt_paid",
        "checkout.session.completed",
        &session,
        total,
        json!({}),
    );
    assert_eq!(
        send_event(&app, &event, "whsec_wrong").await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send_event(&app, &event, WEBHOOK_SECRET).await,
        StatusCode::OK
    );
    assert_eq!(
        send_event(&app, &event, WEBHOOK_SECRET).await,
        StatusCode::OK,
        "duplicate delivery"
    );
    // The redirect never changes anything: still pending until the worker applies the event.
    assert_eq!(order_row(&db, id).await.0, "pending");
    drain(&db, Utc::now()).await;
    assert_eq!(
        order_row(&db, id).await,
        (
            "open".into(),
            "paid".into(),
            "unfulfilled".into(),
            "normal".into()
        )
    );
    let applied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM order_events WHERE order_id = $1 AND kind = 'payment_succeeded'",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(applied, 1);
    assert_eq!(
        level(&db, s.dates_500, s.location).await,
        (5, 0),
        "consumed when paid"
    );
    assert_ledger(&db).await;
    // Nothing secret leaves the settings endpoints.
    for uri in [
        "/v1/admin/shop/settings",
        "/v1/admin/shop/settings/stripe",
        "/v1/shop/config",
    ] {
        let (_, body) = app.admin(Method::GET, uri, None).await;
        let text = body.to_string();
        assert!(
            !text.contains("sk_test_shop_secret") && !text.contains(WEBHOOK_SECRET),
            "{uri}"
        );
    }
    let (_, st) = app
        .admin(Method::GET, "/v1/admin/shop/settings/stripe", None)
        .await;
    assert_eq!(
        (st["configured"].as_bool(), st["mode"].as_str()),
        (Some(true), Some("test"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn an_expired_session_releases_the_stock_and_a_wrong_amount_is_flagged(db: PgPool) {
    let (app, _) = stripe_app(&db).await;
    let s = shop(&db).await;
    let (id, session, _) = card_order(&app, &s, "card-exp").await;
    let event = json!({ "id": "evt_exp", "type": "checkout.session.expired", "data": { "object": { "id": session, "payment_status": "unpaid" } } });
    assert_eq!(
        send_event(&app, &event, WEBHOOK_SECRET).await,
        StatusCode::OK
    );
    drain(&db, Utc::now()).await;
    let (status, _, _, _) = order_row(&db, id).await;
    assert_eq!(status, "cancelled");
    assert_eq!(level(&db, s.dates_500, s.location).await, (7, 0));

    let (id2, session2, total2) = card_order(&app, &s, "card-mismatch").await;
    let event = session_event(
        "evt_short",
        "checkout.session.completed",
        &session2,
        total2 - 100,
        json!({}),
    );
    assert_eq!(
        send_event(&app, &event, WEBHOOK_SECRET).await,
        StatusCode::OK
    );
    drain(&db, Utc::now()).await;
    let (_, pay, _, risk) = order_row(&db, id2).await;
    assert_eq!(
        (pay.as_str(), risk.as_str()),
        ("partially_paid", "review"),
        "recorded but never auto-fulfilled"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_refund_event_before_the_payment_event_ends_in_the_right_state(db: PgPool) {
    let (app, _) = stripe_app(&db).await;
    let s = shop(&db).await;
    let (id, session, total) = card_order(&app, &s, "card-ooo").await;
    let payment_id: Uuid = sqlx::query_scalar("SELECT id FROM payments WHERE order_id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    let refund = json!({ "id": "evt_refund", "type": "charge.refunded", "data": { "object": {
        "id": "ch_1", "payment_intent": "pi_test_1", "amount_refunded": 500, "metadata": { "order_id": id, "payment_id": payment_id } } } });
    assert_eq!(
        send_event(&app, &refund, WEBHOOK_SECRET).await,
        StatusCode::OK
    );
    drain(&db, Utc::now()).await;
    let paid = session_event(
        "evt_paid2",
        "checkout.session.completed",
        &session,
        total,
        json!({}),
    );
    assert_eq!(
        send_event(&app, &paid, WEBHOOK_SECRET).await,
        StatusCode::OK
    );
    drain(&db, Utc::now()).await;
    // The refund event is retried with backoff and applies once the payment exists.
    drain(&db, Utc::now() + Duration::hours(1)).await;
    drain(&db, Utc::now() + Duration::hours(7)).await;
    let (_, pay, _, _) = order_row(&db, id).await;
    assert_eq!(pay, "partially_refunded");
    let refunded: i64 =
        sqlx::query_scalar("SELECT amount_refunded_minor FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(refunded, 500);
}

#[sqlx::test(migrations = "./migrations")]
async fn without_stripe_keys_the_shop_boots_and_hides_card_payment(db: PgPool) {
    let app = TestApp::with_config(db.clone(), |c| {
        c.booking.fake_payments = false;
        c.booking.stripe = None;
    })
    .await;
    shop(&db).await;
    setting(&db, "payments.stripe.enabled", json!(true)).await;
    let (status, cfg) = app
        .request(Method::GET, "/v1/shop/config", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !cfg["payment_methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "stripe"),
        "{cfg}"
    );
    let (status, e) = app
        .admin(
            Method::PATCH,
            "/v1/admin/shop/settings",
            Some(json!({ "payments.stripe.enabled": true })),
        )
        .await;
    assert_eq!(
        (status, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("stripe_not_configured"))
    );
}

// ---- Permissions ------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn editors_never_see_orders_and_reception_cannot_refund(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop(&db).await;
    let editor = create_user(&db, "editor@test.ma", "x", true, Some("editor")).await;
    let reception = create_user(&db, "reception@test.ma", "x", true, Some("reception")).await;
    let token = |id| mouzoun_api::auth::jwt::issue(JWT_SECRET, id, true, 60).unwrap();
    let (status, _) = app
        .request(
            Method::GET,
            "/v1/admin/shop/orders",
            None,
            Some(&token(editor)),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(
            Method::GET,
            "/v1/admin/shop/products",
            None,
            Some(&token(editor)),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let mut v = Visitor::new(&app);
    v.add(s.oil_1l, 1).await;
    v.pickup().await;
    let (_, out) = v.checkout("pay_at_pickup", "kp").await;
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    let rt = token(reception);
    let (status, d) = app
        .request(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/fulfillments"),
            Some(json!({})),
            Some(&rt),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let fid: Uuid = d["created_fulfillment_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let code = orders::pickup_code(&links::pickup_secret(JWT_SECRET), fid);
    let (status, d) = app
        .request(
            Method::PATCH,
            &format!("/v1/admin/shop/fulfillments/{fid}"),
            Some(json!({ "action": "pick_up", "code": code, "collect_payment": true })),
            Some(&rt),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/refunds"),
            Some(json!({ "amount_minor": 100 })),
            Some(&rt),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let _ = s.dates_product;
}

#[path = "shop_france.rs"]
mod france;
