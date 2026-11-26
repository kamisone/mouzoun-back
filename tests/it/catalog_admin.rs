//! Admin editors of stays, activities, packages and extras (`/v1/admin/catalog/...`): content,
//! publish checklist, slugs and redirects, optimistic lock, duplicate, pricing rules and the
//! simulator, inventory rules (units, pool, calendar, slots), packages, delete vs archive,
//! permissions.

use axum::http::{Method, StatusCode};
use chrono::{Duration, NaiveDate, Utc};
use mouzoun_api::{
    auth::jwt,
    services::{
        booking::{
            self,
            trip::{self, TripContext, TripItem, TripRequest},
        },
        inventory,
    },
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::{JWT_SECRET, TestApp, create_user, fixtures::*};

const API: &str = "/v1/admin/catalog";

fn soon(days: i64) -> NaiveDate {
    mouzoun_api::camp_time::today(Utc::now()) + Duration::days(days)
}

fn tr(name: &str, slug: &str) -> Value {
    json!({ "name": name, "slug": slug, "short_description": format!("{name} : en bref"),
            "highlights": ["  Vue sur les dunes ", "", "Calme"], "included": ["Petit-déjeuner"] })
}

fn stay_body(slug: &str) -> Value {
    json!({
        "booking_mode": "instant",
        "translations": { "fr": tr("Khaïma nomade", slug), "en": tr("Nomad tent", &format!("{slug}-en")) },
        "details": { "accommodation_kind": "nomad_tent", "inventory_mode": "unit", "max_guests": 4, "standard_occupancy": 2,
                     "bed_configuration": [{ "type": "double", "count": 1 }], "min_nights": 1, "max_nights": 14,
                     "surface_m2": 12.5 },
    })
}

fn activity_body(slug: &str, mode: &str) -> Value {
    json!({
        "translations": { "fr": tr("Balade en quad", slug) },
        "details": { "location_type": "off_site", "scheduling_mode": mode, "duration_minutes": 60, "min_participants": 1,
                     "max_participants": 8, "guide_languages": ["fr", "en"] },
    })
}

fn package_body(slug: &str, mode: &str, discount: Value) -> Value {
    json!({
        "translations": { "fr": tr("Week-end désert", slug) },
        "details": { "duration_days": 3, "duration_nights": 2, "pricing_mode": mode, "discount_percent": discount },
    })
}

fn extra_body(slug: &str) -> Value {
    json!({
        "translations": { "fr": tr("Petit-déjeuner", slug) },
        "details": { "applies_to": "accommodation", "requires_date": true },
    })
}

async fn create(app: &TestApp, kind: &str, body: Value) -> Value {
    let (st, p) = app
        .admin(Method::POST, &format!("{API}/{kind}"), Some(body))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{p}");
    p
}

/// PATCH with the product as loaded (`updated_at`) plus `changes`.
async fn save(app: &TestApp, kind: &str, p: &Value, changes: Value) -> (StatusCode, Value) {
    let mut body = json!({
        "updated_at": p["updated_at"], "booking_mode": p["booking_mode"], "translations": p["translations"],
        "details": p["details"], "cover_media_id": p["cover_media_id"], "gallery_media_ids": p["gallery_media_ids"],
        "category_ids": p["category_ids"],
    });
    if let (Some(b), Value::Object(c)) = (body.as_object_mut(), changes) {
        b.extend(c);
    }
    app.admin(
        Method::PATCH,
        &format!("{API}/{kind}/{}", p["id"].as_str().unwrap()),
        Some(body),
    )
    .await
}

async fn image(db: &PgPool, alt: Option<&str>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO media_assets (id, bucket, object_key, original_filename, mime_type, kind, size_bytes, width, height)
         VALUES ($1, 'test', $2, 'x.jpg', 'image/jpeg', 'image', 10, 800, 600)",
    )
    .bind(id)
    .bind(format!("media/{id}.jpg"))
    .execute(db)
    .await
    .unwrap();
    if let Some(alt) = alt {
        sqlx::query("INSERT INTO media_asset_translations (media_asset_id, locale, alt_text) VALUES ($1, 'fr', $2)")
            .bind(id)
            .bind(alt)
            .execute(db)
            .await
            .unwrap();
    }
    id
}

/// A cancellation policy and a rate plan using it (what "a policy reachable through a rate
/// plan" needs).
async fn flexible_plan(app: &TestApp) -> String {
    let (_, plans) = app
        .admin(Method::GET, &format!("{API}/pricing/rate-plans"), None)
        .await;
    if let Some(p) = plans["data"]
        .as_array()
        .and_then(|a| a.iter().find(|p| p["code"] == "flex"))
    {
        return p["id"].as_str().unwrap().to_string();
    }
    let (st, policy) = app
        .admin(
            Method::POST,
            &format!("{API}/pricing/cancellation-policies"),
            Some(json!({ "code": "flexible", "names": { "fr": "Flexible" },
                          "rules": [{ "min_hours_before": 48, "refund_percent": 100 }, { "min_hours_before": 0, "refund_percent": 0 }] })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{policy}");
    let (st, plan) = app
        .admin(
            Method::POST,
            &format!("{API}/pricing/rate-plans"),
            Some(json!({ "code": "flex", "names": { "fr": "Flexible" }, "cancellation_policy_id": policy["id"],
                          "deposit_type": "percent", "deposit_percent": 30 })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{plan}");
    plan["id"].as_str().unwrap().to_string()
}

async fn put_prices(app: &TestApp, kind: &str, id: &str, rows: Value) -> (StatusCode, Value) {
    app.admin(
        Method::PUT,
        &format!("{API}/{kind}/{id}/prices"),
        Some(rows),
    )
    .await
}

async fn token_for(db: &PgPool, role: &str) -> String {
    let id = create_user(db, &format!("{role}@test.ma"), "x", true, Some(role)).await;
    jwt::issue(JWT_SECRET, id, true, 60).unwrap()
}

fn codes(list: &Value, key: &str) -> Vec<String> {
    list[key]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["ok"] == false)
        .map(|c| c["code"].as_str().unwrap().to_string())
        .collect()
}

/// A stay ready to publish: cover with alt text, 3 units, a price, a rate plan with a policy.
async fn publishable_stay(app: &TestApp, db: &PgPool, slug: &str) -> Value {
    let p = create(app, "stays", stay_body(slug)).await;
    let id = p["id"].as_str().unwrap();
    let cover = image(db, Some("Une khaïma au coucher du soleil")).await;
    let (st, p) = save(app, "stays", &p, json!({ "cover_media_id": cover })).await;
    assert_eq!(st, StatusCode::OK, "{p}");
    let (st, _) = app
        .admin(
            Method::POST,
            &format!("{API}/stays/{id}/units/bulk"),
            Some(json!({ "prefix": "K-", "from": 1, "to": 3 })),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    flexible_plan(app).await;
    let (st, r) = put_prices(
        app,
        "stays",
        id,
        json!([{ "pricing_unit": "per_unit_per_night", "amount_minor": 6_000 }]),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (_, p) = app
        .admin(Method::GET, &format!("{API}/stays/{id}"), None)
        .await;
    p
}

// ---- Content, checklist, status ------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_stay_is_created_edited_and_published_through_its_checklist(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let p = create(&app, "stays", stay_body("khaima-nomade")).await;
    let id = p["id"].as_str().unwrap();
    assert_eq!(p["status"], "draft");
    assert_eq!(
        p["translations"]["fr"]["highlights"],
        json!(["Vue sur les dunes", "Calme"]),
        "lists trimmed"
    );
    assert_eq!(
        p["details"]["surface_m2"], "12.5",
        "decimal in, decimal string out"
    );
    let nights: i64 =
        sqlx::query_scalar("SELECT count(*) FROM inventory_calendar WHERE product_id = $1::uuid")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(
        nights > 400,
        "calendar rows for the booking horizon ({nights})"
    );

    // Blocking items first.
    let (_, list) = app
        .admin(
            Method::GET,
            &format!("{API}/stays/{id}/publish-checklist"),
            None,
        )
        .await;
    let blocking = codes(&list, "blocking");
    for code in [
        "cover",
        "cover_alt",
        "price",
        "cancellation_policy",
        "inventory",
    ] {
        assert!(
            blocking.contains(&code.to_string()),
            "{code} in {blocking:?}"
        );
    }
    let (st, e) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/publish"), None)
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("publish_blocked"))
    );

    let p = publishable_stay(&app, &db, "khaima-prete").await;
    let id = p["id"].as_str().unwrap();
    let (_, list) = app
        .admin(
            Method::GET,
            &format!("{API}/stays/{id}/publish-checklist"),
            None,
        )
        .await;
    assert!(codes(&list, "blocking").is_empty(), "{list}");
    // Warnings never block: ar missing, no gallery, no SEO description, no category.
    let warnings = codes(&list, "warnings");
    for code in ["ar_missing", "gallery", "seo_description", "category"] {
        assert!(
            warnings.contains(&code.to_string()),
            "{code} in {warnings:?}"
        );
    }
    let (st, p) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/publish"), None)
        .await;
    assert_eq!(
        (st, p["status"].as_str()),
        (StatusCode::OK, Some("published")),
        "{p}"
    );
    assert!(p["published_at"].is_string());
    let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_logs WHERE entity_id = $1 AND action = 'product.status_changed'")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(audit, 1);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM outbox_events WHERE event_type = 'product.updated' AND aggregate_id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(events >= 1);

    // fr name required; a published product keeps passing its checklist on save.
    let mut tr = p["translations"].clone();
    tr["fr"]["name"] = json!("");
    let (st, e) = save(&app, "stays", &p, json!({ "translations": tr })).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.name"],
        "required"
    );
    let (st, e) = save(&app, "stays", &p, json!({ "cover_media_id": null })).await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("publish_blocked"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn every_kind_is_created_with_its_details_and_validated(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(&app, "activities", activity_body("quad", "on_request")).await;
    assert_eq!(
        a["booking_mode"], "on_request",
        "on-request scheduling forces on-request booking"
    );
    assert_eq!(a["details"]["guide_languages"], json!(["fr", "en"]));
    let x = create(&app, "extras", extra_body("petit-dejeuner")).await;
    assert_eq!(x["details"]["requires_date"], true);
    let k = create(
        &app,
        "packages",
        package_body("week-end", "sum_minus_discount", json!(12.35)),
    )
    .await;
    assert_eq!(k["details"]["discount_percent"], "12.35", "no float error");
    let k2 = create(
        &app,
        "packages",
        package_body("week-end-2", "sum_minus_discount", json!("7,5")),
    )
    .await;
    assert_eq!(k2["details"]["discount_percent"], "7.5");

    let mut bad = stay_body("trop-petit");
    bad["details"]["standard_occupancy"] = json!(6);
    bad["details"]["max_nights"] = json!(0);
    bad["details"]["max_children"] = json!(9);
    let (st, e) = app
        .admin(Method::POST, &format!("{API}/stays"), Some(bad))
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let fields = &e["error"]["details"]["fields"];
    assert_eq!(fields["details.standard_occupancy"], "above_max_guests");
    assert_eq!(fields["details.max_nights"], "below_min_nights");
    assert_eq!(fields["details.max_children"], "above_max_guests");
    let mut bad = activity_body("quad-2", "fixed_slots");
    bad["details"]["min_participants"] = json!(4);
    bad["details"]["max_participants"] = json!(2);
    let (_, e) = app
        .admin(Method::POST, &format!("{API}/activities"), Some(bad))
        .await;
    assert_eq!(
        e["error"]["details"]["fields"]["details.max_participants"],
        "below_min_participants"
    );
    let (_, e) = app
        .admin(
            Method::POST,
            &format!("{API}/packages"),
            Some(package_body("x", "sum_minus_discount", json!(null))),
        )
        .await;
    assert_eq!(
        e["error"]["details"]["fields"]["details.discount_percent"],
        "required"
    );
    let (_, e) = app
        .admin(
            Method::POST,
            &format!("{API}/packages"),
            Some(package_body("y", "fixed", json!(101))),
        )
        .await;
    assert_eq!(
        e["error"]["details"]["fields"]["details.discount_percent"],
        "range"
    );
    // Slugs are unique per language.
    let (_, e) = app
        .admin(
            Method::POST,
            &format!("{API}/stays"),
            Some(stay_body("khaima")),
        )
        .await;
    assert!(e["id"].is_string());
    let (_, e) = app
        .admin(
            Method::POST,
            &format!("{API}/stays"),
            Some(stay_body("khaima")),
        )
        .await;
    assert_eq!(
        e["error"]["details"]["fields"]["translations.fr.slug"],
        "slug_taken"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_stale_save_answers_409(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let p = create(&app, "extras", extra_body("bois")).await;
    let (st, fresh) = save(&app, "extras", &p, json!({ "is_featured": true })).await;
    assert_eq!(st, StatusCode::OK);
    assert_ne!(fresh["updated_at"], p["updated_at"]);
    let (st, e) = save(&app, "extras", &p, json!({ "is_featured": false })).await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("stale"))
    );
    assert_eq!(e["error"]["details"]["updated_at"], fresh["updated_at"]);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_slug_change_gives_one_301_per_changed_language_for_each_kind(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = publishable_stay(&app, &db, "tente").await;
    let (st, stay) = app
        .admin(
            Method::POST,
            &format!("{API}/stays/{}/publish", stay["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{stay}");
    let mut tr = stay["translations"].clone();
    tr["fr"]["slug"] = json!("khaima-du-desert");
    tr["en"]["slug"] = json!("desert-khaima");
    let (st, after) = save(&app, "stays", &stay, json!({ "translations": tr })).await;
    assert_eq!(st, StatusCode::OK, "{after}");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT from_path, to_path FROM redirects WHERE entity_id = $1::uuid AND is_active ORDER BY from_path",
    )
    .bind(stay["id"].as_str().unwrap())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "/en/stays/tente-en".into(),
                "/en/stays/desert-khaima".into()
            ),
            ("/stays/tente".into(), "/stays/khaima-du-desert".into()),
        ]
    );

    // Activities and packages: same rule, through the same registry.
    for (kind, body, segment) in [
        (
            "activities",
            activity_body("quad-tour", "anytime"),
            "activities",
        ),
        (
            "packages",
            package_body("evasion", "fixed", json!(null)),
            "packages",
        ),
    ] {
        let p = create(&app, kind, body).await;
        let id = p["id"].as_str().unwrap().to_string();
        // Published straight in the database: this test is about the URLs.
        sqlx::query(
            "UPDATE products SET status = 'published', published_at = now() WHERE id = $1::uuid",
        )
        .bind(&id)
        .execute(&db)
        .await
        .unwrap();
        let (_, p) = app
            .admin(Method::GET, &format!("{API}/{kind}/{id}"), None)
            .await;
        let mut tr = p["translations"].clone();
        let old = tr["fr"]["slug"].as_str().unwrap().to_string();
        tr["fr"]["slug"] = json!(format!("{old}-v2"));
        let (st, _) = save(&app, kind, &p, json!({ "translations": tr })).await;
        assert_eq!(st, StatusCode::OK);
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT from_path, to_path FROM redirects WHERE entity_id = $1::uuid")
                .bind(&id)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![(format!("/{segment}/{old}"), format!("/{segment}/{old}-v2"))]
        );
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn duplicate_copies_the_content_and_nothing_operational(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = publishable_stay(&app, &db, "bivouac").await;
    let id = stay["id"].as_str().unwrap();
    let gallery = image(&db, Some("Dunes")).await;
    let (_, stay) = save(
        &app,
        "stays",
        &stay,
        json!({ "gallery_media_ids": [gallery] }),
    )
    .await;
    let (st, copy) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/duplicate"), None)
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, copy) = app
        .admin(
            Method::GET,
            &format!("{API}/stays/{}", copy["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(copy["status"], "draft");
    assert_eq!(copy["translations"]["fr"]["slug"], "bivouac-copie");
    assert_eq!(copy["translations"]["en"]["slug"], "bivouac-en-copy");
    assert_eq!(copy["translations"]["fr"]["name"], "Khaïma nomade (copie)");
    assert_eq!(copy["cover_media_id"], stay["cover_media_id"]);
    assert_eq!(copy["gallery_media_ids"], json!([gallery]));
    assert_ne!(copy["gallery_id"], stay["gallery_id"], "its own gallery");
    let cid = copy["id"].as_str().unwrap();
    let count = |sql: &'static str| {
        let db = db.clone();
        let cid = cid.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .bind(cid)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM prices WHERE product_id = $1::uuid").await,
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM accommodation_units WHERE product_id = $1::uuid").await,
        0,
        "never units"
    );
    // Its own calendar rows (zero capacity: no units yet), never the original's bookings.
    assert_eq!(
        count(
            "SELECT count(*) FROM inventory_calendar WHERE product_id = $1::uuid AND capacity > 0"
        )
        .await,
        0
    );

    // Activities: schedules copied inactive, no slot.
    let a = create(
        &app,
        "activities",
        activity_body("quad-sunset", "fixed_slots"),
    )
    .await;
    let aid = a["id"].as_str().unwrap();
    let (st, sync) = app
        .admin(Method::PUT, &format!("{API}/activities/{aid}/schedules"),
               Some(json!([{ "days_of_week": 127, "start_time": "09:00", "capacity": 8, "valid_from": soon(1) }])))
        .await;
    assert_eq!(st, StatusCode::OK, "{sync}");
    assert!(sync["created"].as_u64().unwrap() > 300);
    let (_, ac) = app
        .admin(
            Method::POST,
            &format!("{API}/activities/{aid}/duplicate"),
            None,
        )
        .await;
    let acid = ac["id"].as_str().unwrap();
    let (active, slots): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM activity_schedules WHERE product_id = $1::uuid AND is_active),
                (SELECT count(*) FROM activity_slots WHERE product_id = $1::uuid)",
    )
    .bind(acid)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((active, slots), (0, 0));
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM activity_schedules WHERE product_id = $1::uuid")
            .bind(acid)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(total, 1);
}

// ---- Pricing --------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn price_rules_are_checked_per_kind_and_ambiguities_flagged(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = create(&app, "stays", stay_body("emplacement")).await;
    let id = stay["id"].as_str().unwrap();
    let (st, e) = put_prices(
        &app,
        "stays",
        id,
        json!([{ "pricing_unit": "per_person", "amount_minor": 1_000 }]),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        e["error"]["details"]["fields"]["prices[0].pricing_unit"],
        "not_allowed_for_kind"
    );
    let act = create(&app, "activities", activity_body("chameau", "anytime")).await;
    let (st, _) = put_prices(
        &app,
        "activities",
        act["id"].as_str().unwrap(),
        json!([{ "pricing_unit": "per_unit_per_night", "amount_minor": 1 }]),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    let (st, r) = put_prices(&app, "stays", id, json!([
        { "pricing_unit": "per_unit_per_night", "amount_minor": 4_590, "extra_guest_amount_minor": 1_005 },
        { "pricing_unit": "per_unit_per_night", "amount_minor": 5_000 },
        { "pricing_unit": "per_unit_per_night", "amount_minor": 6_000, "days_of_week": 96, "priority": 0 },
    ])).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let rows = r["data"].as_array().unwrap();
    let amb: Vec<(i64, usize)> = rows
        .iter()
        .map(|x| {
            (
                x["amount_minor"].as_i64().unwrap(),
                x["ambiguous_with"].as_array().unwrap().len(),
            )
        })
        .collect();
    assert!(
        amb.contains(&(4_590, 1)) && amb.contains(&(5_000, 1)),
        "two equal rules: {amb:?}"
    );
    assert!(amb.contains(&(6_000, 0)), "weekend rule is more specific");
    assert_eq!(
        rows.iter().find(|x| x["amount_minor"] == 4_590).unwrap()["extra_guest_amount_minor"],
        1_005,
        "cents exact"
    );
    // A different priority settles it.
    let mut edited: Vec<Value> = rows.to_vec();
    for x in &mut edited {
        if x["amount_minor"] == 5_000 {
            x["priority"] = json!(1);
        }
    }
    let (_, r) = put_prices(&app, "stays", id, json!(edited)).await;
    assert!(
        r["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["ambiguous_with"].as_array().unwrap().is_empty()),
        "{r}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_simulator_charges_exactly_what_the_funnel_charges(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = publishable_stay(&app, &db, "simu").await;
    let id = stay["id"].as_str().unwrap();
    let (st, _) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/publish"), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let guests = guest_ids(&db).await;
    let arrival = soon(20);
    let (st, sim) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/price-quote"),
               Some(json!({ "date": arrival, "departure": arrival + Duration::days(2), "guests": { guests.adult.to_string(): 3 } })))
        .await;
    assert_eq!(st, StatusCode::OK, "{sim}");
    // The public funnel, same input.
    let ctx = TripContext {
        locale: mouzoun_api::db::types::Locale::new("fr"),
        public: true,
        now: Utc::now(),
        settings: booking::settings::load(&db).await.unwrap(),
        online_payments: app.state.payments.is_enabled(),
    };
    let request = TripRequest {
        items: vec![TripItem::Stay {
            product_id: id.parse().unwrap(),
            arrival,
            departure: arrival + Duration::days(2),
            quantity: 1,
            guests: adults(&guests, 3),
            unit_ids: vec![],
        }],
        ..TripRequest::default()
    };
    let mut conn = db.acquire().await.unwrap();
    let public = trip::quote(&mut conn, &request, &ctx).await.unwrap();
    let q = &sim["quote"];
    assert_eq!(q["total_minor"], public.total_minor);
    assert_eq!(q["subtotal_minor"], public.subtotal_minor);
    assert_eq!(
        q["items"][0]["lines"],
        serde_json::to_value(&public.items[0].lines).unwrap()
    );
    assert_eq!(
        public.total_minor,
        2 * 6_000,
        "two nights, standard occupancy 2, no extra-guest price"
    );
    let rule = q["items"][0]["lines"][0]["price_id"].as_str().unwrap();
    assert!(sim["rules"][rule].is_object(), "the rule used is returned");
    assert!(
        !q["cancellation"].as_array().unwrap().is_empty(),
        "policy shown"
    );
}

// ---- Inventory --------------------------------------------------------------------------------------

async fn book_nights(
    db: &PgPool,
    product: Uuid,
    arrival: NaiveDate,
    nights: i64,
    quantity: i32,
) -> String {
    let guests = guest_ids(db).await;
    let created = booking::book_accommodation(
        db,
        &booking::AccommodationRequest {
            product_id: product,
            arrival,
            departure: arrival + Duration::days(nights),
            quantity,
            guests: adults(&guests, 2),
            meta: meta(&format!("{}@x.ma", Uuid::now_v7())),
        },
        Utc::now(),
        mouzoun_api::camp_time::today(Utc::now()),
    )
    .await
    .unwrap();
    created.reference
}

#[sqlx::test(migrations = "./migrations")]
async fn capacity_never_drops_below_bookings_and_follows_units_and_closures(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = publishable_stay(&app, &db, "capacite").await;
    let id = stay["id"].as_str().unwrap();
    let pid: Uuid = id.parse().unwrap();
    app.admin(Method::POST, &format!("{API}/stays/{id}/publish"), None)
        .await;
    let night = soon(30);
    let cap = |day: NaiveDate| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i32>(
                "SELECT capacity FROM inventory_calendar WHERE product_id = $1 AND day = $2",
            )
            .bind(pid)
            .bind(day)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(cap(night).await, 3);
    let reference = book_nights(&db, pid, night, 2, 2).await;

    // Unit mode: retiring an allocated unit is refused with the booking listed.
    let (_, units) = app
        .admin(Method::GET, &format!("{API}/stays/{id}/units"), None)
        .await;
    let allocated: Vec<Uuid> =
        sqlx::query_scalar("SELECT unit_id FROM unit_allocations WHERE released_at IS NULL")
            .fetch_all(&db)
            .await
            .unwrap();
    let unit = units["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| allocated.contains(&u["id"].as_str().unwrap().parse().unwrap()))
        .unwrap();
    let (st, e) = app
        .admin(
            Method::PATCH,
            &format!("{API}/stays/{id}/units/{}", unit["id"].as_str().unwrap()),
            Some(json!({ "code": unit["code"], "status": "retired" })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("unit_booked"))
    );
    assert_eq!(e["error"]["details"]["bookings"][0]["reference"], reference);

    // The free unit can go to maintenance: capacity 3 → 2 everywhere, still ≥ 2 reserved.
    let free = units["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| !allocated.contains(&u["id"].as_str().unwrap().parse().unwrap()))
        .unwrap();
    let (st, e) = app
        .admin(
            Method::PATCH,
            &format!("{API}/stays/{id}/units/{}", free["id"].as_str().unwrap()),
            Some(json!({ "code": free["code"], "status": "maintenance" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{e}");
    assert_eq!(cap(night).await, 2);
    assert_eq!(cap(soon(100)).await, 2);

    // A unit closure lowers capacity on its nights only; removing it restores them.
    let (st, units_now) = app
        .admin(Method::GET, &format!("{API}/stays/{id}/units"), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let active_free = units_now["data"].as_array().unwrap().iter().find(|u| {
        u["status"] == "active" && !allocated.contains(&u["id"].as_str().unwrap().parse().unwrap())
    });
    assert!(
        active_free.is_none(),
        "both active units are booked on that night"
    );
    let (st, closure) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/closures"),
               Some(json!({ "from": soon(60), "to": soon(62), "unit_id": free["id"], "reason": "Toile à changer" })))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{closure}");
    // The free unit is already in maintenance: nothing more to remove.
    assert_eq!(cap(soon(60)).await, 2);
    let (st, _) = app
        .admin(
            Method::PATCH,
            &format!("{API}/stays/{id}/units/{}", free["id"].as_str().unwrap()),
            Some(json!({ "code": free["code"], "status": "active" })),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        (
            cap(soon(60)).await,
            cap(soon(61)).await,
            cap(soon(62)).await,
            cap(night).await
        ),
        (2, 2, 3, 3)
    );
    let (st, _) = app
        .admin(
            Method::DELETE,
            &format!(
                "{API}/stays/{id}/closures/{}",
                closure["id"].as_str().unwrap()
            ),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(cap(soon(60)).await, 3);

    // Switching mode with future bookings is refused.
    let (st, e) = app
        .admin(
            Method::PUT,
            &format!("{API}/stays/{id}/inventory"),
            Some(json!({ "inventory_mode": "pool", "pool_capacity": 20 })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("mode_locked"))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pool_cannot_shrink_below_its_bookings(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let mut body = stay_body("emplacement-tente");
    body["details"]["inventory_mode"] = json!("pool");
    body["details"]["pool_capacity"] = json!(20);
    let p = create(&app, "stays", body).await;
    let id = p["id"].as_str().unwrap();
    let pid: Uuid = id.parse().unwrap();
    price(&db, pid, "per_unit_per_night", 1_500).await;
    sqlx::query("UPDATE products SET status = 'published' WHERE id = $1")
        .bind(pid)
        .execute(&db)
        .await
        .unwrap();
    let night = soon(15);
    let reference = book_nights(&db, pid, night, 1, 5).await;
    let (st, e) = app
        .admin(
            Method::PATCH,
            &format!("{API}/stays/{id}/calendar"),
            Some(json!({ "from": night, "to": night + Duration::days(1), "capacity": 4 })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("below_reserved")),
        "{e}"
    );
    assert_eq!(e["error"]["details"]["bookings"][0]["reference"], reference);
    assert_eq!(e["error"]["details"]["nights"][0]["reserved"], 5);
    let (st, e) = app
        .admin(
            Method::PUT,
            &format!("{API}/stays/{id}/inventory"),
            Some(json!({ "inventory_mode": "pool", "pool_capacity": 3 })),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT, "{e}");
    // Down to the bookings is fine; a range with weekdays and stop sell.
    let (st, r) = app
        .admin(Method::PATCH, &format!("{API}/stays/{id}/calendar"),
               Some(json!({ "from": night, "to": night + Duration::days(14), "capacity": 5, "stop_sell": true, "days_of_week": 32 | 64 })))
        .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (_, cal) = app
        .admin(
            Method::GET,
            &format!(
                "{API}/stays/{id}/calendar?from={night}&to={}",
                night + Duration::days(14)
            ),
            None,
        )
        .await;
    let weekend: Vec<&Value> = cal["nights"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["stop_sell"] == true)
        .collect();
    assert_eq!(weekend.len(), 4, "two weekends");
    assert!(weekend.iter().all(|n| n["capacity"] == 5));
}

#[sqlx::test(migrations = "./migrations")]
async fn departures_with_bookings_are_never_shrunk_cancelled_or_deleted(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let a = create(
        &app,
        "activities",
        activity_body("quad-matin", "fixed_slots"),
    )
    .await;
    let id = a["id"].as_str().unwrap();
    let pid: Uuid = id.parse().unwrap();
    price(&db, pid, "per_person", 4_500).await;
    sqlx::query("UPDATE products SET status = 'published' WHERE id = $1")
        .bind(pid)
        .execute(&db)
        .await
        .unwrap();
    let schedule = json!([{ "days_of_week": 127, "start_time": "09:00", "capacity": 6, "valid_from": soon(1), "valid_to": soon(31) }]);
    let (_, first) = app
        .admin(
            Method::PUT,
            &format!("{API}/activities/{id}/schedules"),
            Some(schedule.clone()),
        )
        .await;
    assert_eq!(first["created"], 30, "{first}");
    // Saved again as the editor sends it (with its id): nothing changes.
    let (_, saved) = app
        .admin(
            Method::GET,
            &format!("{API}/activities/{id}/schedules"),
            None,
        )
        .await;
    let (_, again) = app
        .admin(
            Method::PUT,
            &format!("{API}/activities/{id}/schedules"),
            Some(saved["data"].clone()),
        )
        .await;
    assert_eq!(
        (again["created"].as_u64(), again["deleted"].as_u64()),
        (Some(0), Some(0)),
        "idempotent"
    );

    let slot: (Uuid, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT id, starts_at FROM activity_slots WHERE product_id = $1 ORDER BY starts_at OFFSET 4 LIMIT 1")
            .bind(pid)
            .fetch_one(&db)
            .await
            .unwrap();
    let guests = guest_ids(&db).await;
    let created = booking::book_activity(
        &db,
        &booking::ActivityRequest {
            slot_id: slot.0,
            guests: adults(&guests, 3),
            quantity: 1,
            meta: meta("slot@x.ma"),
        },
        Utc::now(),
        mouzoun_api::camp_time::today(Utc::now()),
    )
    .await
    .unwrap();
    let (st, e) = app
        .admin(
            Method::PATCH,
            &format!("{API}/slots/{}", slot.0),
            Some(json!({ "capacity": 2 })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("below_reserved"))
    );
    let (st, e) = app
        .admin(
            Method::PATCH,
            &format!("{API}/slots/{}", slot.0),
            Some(json!({ "status": "cancelled" })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("slot_booked"))
    );
    assert_eq!(
        e["error"]["details"]["bookings"][0]["reference"],
        created.reference
    );
    let (st, r) = app
        .admin(
            Method::PATCH,
            &format!("{API}/slots/{}", slot.0),
            Some(json!({ "capacity": 3 })),
        )
        .await;
    assert_eq!((st, r["status"].as_str()), (StatusCode::OK, Some("full")));

    // The schedule moves to 10:00: unbooked 09:00 departures go, the booked one stays (reported).
    let (_, moved) = app
        .admin(Method::PUT, &format!("{API}/activities/{id}/schedules"),
               Some(json!([{ "days_of_week": 127, "start_time": "10:00", "capacity": 6, "valid_from": soon(1), "valid_to": soon(31) }])))
        .await;
    assert_eq!(moved["deleted"], 29, "{moved}");
    assert_eq!(moved["kept_booked"].as_array().unwrap().len(), 1);
    assert_eq!(moved["kept_booked"][0]["slot_id"], slot.0.to_string());
    let still: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM activity_slots WHERE id = $1)")
            .bind(slot.0)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(still);

    // A product closure closes the departures it covers; removing it reopens them.
    let (st, c) = app
        .admin(
            Method::POST,
            &format!("{API}/activities/{id}/closures"),
            Some(json!({ "from": soon(10), "to": soon(12) })),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let closed = || {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM activity_slots WHERE product_id = $1 AND status = 'closed'",
            )
            .bind(pid)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(closed().await, 2);
    app.admin(
        Method::DELETE,
        &format!(
            "{API}/activities/{id}/closures/{}",
            c["id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert_eq!(closed().await, 0);
}

#[test]
fn slots_follow_moroccan_clock_changes() {
    use chrono::{NaiveTime, TimeZone};
    let rule = inventory::ScheduleRule {
        id: Uuid::nil(),
        product_id: Uuid::nil(),
        days_of_week: 127,
        start_time: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        capacity: 4,
        valid_period: mouzoun_api::db::types::DateRange {
            start: None,
            end: None,
        },
        duration_minutes: Some(90),
    };
    let at = |y, m, d| {
        inventory::plan_slots(
            std::slice::from_ref(&rule),
            NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            1,
        )[0]
        .starts_at
    };
    // 2026: UTC+1, UTC+0 during Ramadan (15 Feb – 22 Mar), UTC+1 again, then UTC+0 for good
    // from 20 September 2026 (tzdata 2026c).
    assert_eq!(
        at(2026, 2, 1),
        Utc.with_ymd_and_hms(2026, 2, 1, 8, 0, 0).unwrap()
    );
    assert_eq!(
        at(2026, 3, 1),
        Utc.with_ymd_and_hms(2026, 3, 1, 9, 0, 0).unwrap()
    );
    assert_eq!(
        at(2026, 6, 1),
        Utc.with_ymd_and_hms(2026, 6, 1, 8, 0, 0).unwrap()
    );
    assert_eq!(
        at(2026, 10, 1),
        Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap()
    );
    let slot = &inventory::plan_slots(&[rule], NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(), 1)[0];
    assert_eq!(slot.ends_at - slot.starts_at, Duration::minutes(90));
}

// ---- Packages ---------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn package_content_rules_are_enforced(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let k = create(
        &app,
        "packages",
        package_body("weekend-regles", "fixed", json!(null)),
    )
    .await;
    let kid = k["id"].as_str().unwrap();
    let other = create(
        &app,
        "packages",
        package_body("autre", "fixed", json!(null)),
    )
    .await;
    let s1 = create(&app, "stays", stay_body("s-un")).await;
    let s2 = create(&app, "stays", stay_body("s-deux")).await;
    let act = create(&app, "activities", activity_body("visite", "anytime")).await;
    let shop: Uuid =
        sqlx::query_scalar("INSERT INTO products (kind) VALUES ('shop_product') RETURNING id")
            .fetch_one(&db)
            .await
            .unwrap();
    let uri = format!("{API}/packages/{kid}/items");
    let items = |v: Value| app.admin(Method::PUT, &uri, Some(v));
    let (_, e) =
        items(json!([{ "item_product_id": other["id"] }, { "item_product_id": shop }])).await;
    let f = &e["error"]["details"]["fields"];
    assert_eq!(f["items[0].item_product_id"], "package_in_package");
    assert_eq!(f["items[1].item_product_id"], "not_bookable_kind");
    let (_, e) = items(json!([
        { "item_product_id": s1["id"], "quantity": 2, "day_number": 1 },
        { "item_product_id": s2["id"], "quantity": 2, "day_number": 1 },
    ]))
    .await;
    assert_eq!(
        e["error"]["details"]["fields"]["items[1].item_product_id"],
        "one_stay_only"
    );
    let (_, e) =
        items(json!([{ "item_product_id": s1["id"], "quantity": 3, "day_number": 1 }])).await;
    assert_eq!(
        e["error"]["details"]["fields"]["items[0].quantity"],
        "nights_mismatch"
    );
    let (_, e) = items(json!([{ "item_product_id": act["id"], "day_number": 4 }])).await;
    assert_eq!(
        e["error"]["details"]["fields"]["items[0].day_number"],
        "range"
    );
    let (st, _) = items(json!([
        { "item_product_id": s1["id"], "quantity": 2, "day_number": 1 },
        { "item_product_id": act["id"], "day_number": 2 },
    ]))
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // A published package keeps its items on the site.
    sqlx::query("UPDATE products SET status = 'published' WHERE id = ANY($1::uuid[])")
        .bind(vec![
            kid.to_string(),
            act["id"].as_str().unwrap().to_string(),
            s1["id"].as_str().unwrap().to_string(),
        ])
        .execute(&db)
        .await
        .unwrap();
    let (st, e) = app
        .admin(
            Method::POST,
            &format!("{API}/activities/{}/archive", act["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("in_use"))
    );
    assert_eq!(e["error"]["details"]["packages"][0]["id"], kid);
    let (_, detail) = app
        .admin(
            Method::GET,
            &format!("{API}/activities/{}", act["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(detail["used_in"]["packages"].as_array().unwrap().len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_sum_minus_discount_package_costs_its_items_minus_the_discount_to_the_cent(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    flexible_plan(&app).await;
    let stay = create(&app, "stays", stay_body("pk-tente")).await;
    let sid = stay["id"].as_str().unwrap();
    app.admin(
        Method::POST,
        &format!("{API}/stays/{sid}/units/bulk"),
        Some(json!({ "prefix": "T", "from": 1, "to": 4 })),
    )
    .await;
    put_prices(
        &app,
        "stays",
        sid,
        json!([{ "pricing_unit": "per_unit_per_night", "amount_minor": 6_333 }]),
    )
    .await;
    let act = create(&app, "activities", activity_body("pk-ferme", "anytime")).await;
    let aid = act["id"].as_str().unwrap();
    put_prices(
        &app,
        "activities",
        aid,
        json!([{ "pricing_unit": "per_person", "amount_minor": 1_999 }]),
    )
    .await;
    let k = create(
        &app,
        "packages",
        package_body("pk-weekend", "sum_minus_discount", json!("12.5")),
    )
    .await;
    let kid = k["id"].as_str().unwrap();
    let (st, e) = app
        .admin(
            Method::PUT,
            &format!("{API}/packages/{kid}/items"),
            Some(json!([
                { "item_product_id": sid, "quantity": 2, "day_number": 1 },
                { "item_product_id": aid, "day_number": 2 },
            ])),
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{e}");
    let guests = guest_ids(&db).await;
    let start = soon(40);
    let (st, sim) = app
        .admin(
            Method::POST,
            &format!("{API}/packages/{kid}/price-quote"),
            Some(json!({ "date": start, "guests": { guests.adult.to_string(): 2 } })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{sim}");
    let items = 2 * 6_333 + 2 * 1_999; // 2 nights × 1 tent + 2 adults
    let discount = (Decimal::from(items) * Decimal::new(125, 3))
        .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
    let expected = items - i64::try_from(discount.mantissa()).unwrap();
    assert_eq!(sim["quote"]["total_minor"], expected, "{}", sim["quote"]);
    let (_, list) = app
        .admin(
            Method::GET,
            &format!("{API}/packages/{kid}/items?date={start}"),
            None,
        )
        .await;
    let sample: i64 = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["sample_price_minor"].as_i64().unwrap())
        .sum();
    assert_eq!(
        sample,
        2 * 6_333 + 1_999,
        "one adult per item on the sample date"
    );

    // "Vérifier une date": the funnel tells which item blocks.
    let (st, av) = app
        .admin(
            Method::GET,
            &format!("{API}/packages/{kid}/availability?start={start}&party=adult:2"),
            None,
        )
        .await;
    assert_eq!(
        (st, av["available"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{av}"
    );
    sqlx::query(
        "UPDATE inventory_calendar SET stop_sell = true WHERE product_id = $1::uuid AND day = $2",
    )
    .bind(sid)
    .bind(start)
    .execute(&db)
    .await
    .unwrap();
    let (_, av) = app
        .admin(
            Method::GET,
            &format!("{API}/packages/{kid}/availability?start={start}&party=adult:2"),
            None,
        )
        .await;
    assert_eq!(av["available"], false);
    assert!(
        av["blocking"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["product_id"] == sid),
        "{av}"
    );
}

use rust_decimal::Decimal;

// ---- Delete vs archive ------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_booked_product_is_archived_never_deleted(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = publishable_stay(&app, &db, "a-garder").await;
    let id = stay["id"].as_str().unwrap();
    app.admin(Method::POST, &format!("{API}/stays/{id}/publish"), None)
        .await;
    let reference = book_nights(&db, id.parse().unwrap(), soon(10), 2, 1).await;
    let (st, e) = app
        .admin(
            Method::DELETE,
            &format!("{API}/stays/{id}?confirm=Khaïma%20nomade"),
            None,
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("booked"))
    );
    let (st, p) = app
        .admin(Method::POST, &format!("{API}/stays/{id}/archive"), None)
        .await;
    assert_eq!(
        (st, p["status"].as_str()),
        (StatusCode::OK, Some("archived"))
    );
    let status: String = sqlx::query_scalar("SELECT status FROM bookings WHERE reference = $1")
        .bind(&reference)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_ne!(status, "cancelled", "archiving never touches bookings");
    let reserved: i64 = sqlx::query_scalar(
        "SELECT sum(reserved) FROM inventory_calendar WHERE product_id = $1::uuid",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(reserved, 2);
    // The old page now redirects (inactive) to the stays listing; nothing was deleted.
    let redirect: bool =
        sqlx::query_scalar("SELECT is_active FROM redirects WHERE from_path = '/stays/a-garder'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(!redirect);

    // A product never booked: deleted, with its name typed when it has a setup.
    let other = publishable_stay(&app, &db, "a-jeter").await;
    let oid = other["id"].as_str().unwrap();
    let (st, e) = app
        .admin(Method::DELETE, &format!("{API}/stays/{oid}"), None)
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("confirm_name"))
    );
    let (st, _) = app
        .admin(
            Method::DELETE,
            &format!("{API}/stays/{oid}?confirm=Khaïma%20nomade"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = app
        .admin(Method::GET, &format!("{API}/stays/{oid}"), None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ---- Permissions ------------------------------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn editors_write_content_only_reception_reads_the_calendar(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = create(&app, "stays", stay_body("droits")).await;
    let id = stay["id"].as_str().unwrap();
    let editor = token_for(&db, "editor").await;
    let reception = token_for(&db, "reception").await;
    let as_ = |token: &str, m: Method, uri: String, body: Option<Value>| {
        let token = token.to_string();
        let app = &app;
        async move { app.request(m, &uri, body, Some(&token)).await.0 }
    };
    // Editor: content yes.
    let mut body = json!({ "updated_at": stay["updated_at"], "translations": stay["translations"], "details": stay["details"] });
    body["is_featured"] = json!(true);
    assert_eq!(
        as_(
            &editor,
            Method::PATCH,
            format!("{API}/stays/{id}"),
            Some(body)
        )
        .await,
        StatusCode::OK
    );
    // Editor: prices, units, calendar, departures no.
    assert_eq!(
        as_(
            &editor,
            Method::PUT,
            format!("{API}/stays/{id}/prices"),
            Some(json!([]))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        as_(
            &editor,
            Method::POST,
            format!("{API}/stays/{id}/units"),
            Some(json!({ "code": "Z1" }))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    let range = json!({ "from": soon(5), "to": soon(6), "stop_sell": true });
    assert_eq!(
        as_(
            &editor,
            Method::PATCH,
            format!("{API}/stays/{id}/calendar"),
            Some(range.clone())
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        as_(
            &editor,
            Method::PATCH,
            format!("{API}/slots/{}", Uuid::now_v7()),
            Some(json!({ "capacity": 1 }))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Reception: reads the calendar, cannot change it nor the content.
    let window = format!("{API}/stays/{id}/calendar?from={}&to={}", soon(1), soon(30));
    assert_eq!(
        as_(&reception, Method::GET, window, None).await,
        StatusCode::OK
    );
    assert_eq!(
        as_(
            &reception,
            Method::PATCH,
            format!("{API}/stays/{id}/calendar"),
            Some(range)
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        as_(
            &reception,
            Method::POST,
            format!("{API}/stays/{id}/publish"),
            None
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Guide: reads the editors (departures, calendar), changes nothing.
    let guide = token_for(&db, "guide").await;
    assert_eq!(
        as_(&guide, Method::GET, format!("{API}/stays"), None).await,
        StatusCode::OK
    );
    let window = format!("{API}/stays/{id}/calendar?from={}&to={}", soon(1), soon(30));
    assert_eq!(as_(&guide, Method::GET, window, None).await, StatusCode::OK);
    assert_eq!(
        as_(
            &guide,
            Method::PUT,
            format!("{API}/stays/{id}/prices"),
            Some(json!([]))
        )
        .await,
        StatusCode::FORBIDDEN
    );
    // Staff without products.read (no role) get no editor.
    let nobody_id = create_user(&db, "nobody@test.ma", "x", true, None).await;
    let nobody = jwt::issue(JWT_SECRET, nobody_id, true, 60).unwrap();
    assert_eq!(
        as_(&nobody, Method::GET, format!("{API}/stays"), None).await,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_list_shows_key_numbers_missing_languages_and_the_from_price(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let stay = publishable_stay(&app, &db, "liste").await;
    let (st, list) = app
        .admin(Method::GET, &format!("{API}/stays?status=draft"), None)
        .await;
    assert_eq!(st, StatusCode::OK);
    let row = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == stay["id"])
        .unwrap();
    assert_eq!(row["missing_locales"], json!(["ar"]));
    assert_eq!(row["numbers"]["units"], 3);
    assert_eq!(row["from_price_minor"], 6_000);
    let (_, none) = app
        .admin(Method::GET, &format!("{API}/stays?status=published"), None)
        .await;
    assert!(none["data"].as_array().unwrap().is_empty());
    // Order and bulk publish.
    let other = create(&app, "stays", stay_body("liste-2")).await;
    let (st, _) = app
        .admin(
            Method::PUT,
            &format!("{API}/stays/order"),
            Some(json!({ "ids": [other["id"], stay["id"]] })),
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = app.admin(Method::GET, &format!("{API}/stays"), None).await;
    assert_eq!(list["data"][0]["id"], other["id"]);
    let (_, r) = app
        .admin(
            Method::POST,
            &format!("{API}/stays/bulk"),
            Some(json!({ "action": "publish", "ids": [stay["id"], other["id"]] })),
        )
        .await;
    let results = r["results"].as_array().unwrap();
    assert_eq!(results[0]["ok"], true);
    assert_eq!(
        (results[1]["ok"].as_bool(), results[1]["code"].as_str()),
        (Some(false), Some("publish_blocked"))
    );
}

// ---- Public product page: the video --------------------------------------------------------------

async fn media_file(db: &PgPool, kind: &str, visibility: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO media_assets (id, bucket, object_key, original_filename, mime_type, kind, size_bytes, visibility)
         VALUES ($1, 'test', $2, 'x', $3, $4, 10, $5)",
    )
    .bind(id)
    .bind(format!("media/{id}"))
    .bind(if kind == "video" { "video/mp4" } else { "image/jpeg" })
    .bind(kind)
    .bind(visibility)
    .execute(db)
    .await
    .unwrap();
    id
}

#[sqlx::test(migrations = "./migrations")]
async fn the_public_page_carries_the_video_of_stays_activities_and_packages(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let public_video = media_file(&db, "video", "public").await;
    let private_video = media_file(&db, "video", "private").await;
    let image = media_file(&db, "image", "public").await;
    let mut cases = vec![];
    for (kind, body, video, expected) in [
        (
            "activities",
            activity_body("video-quad", "anytime"),
            Some(public_video),
            true,
        ),
        ("stays", stay_body("video-khaima"), Some(public_video), true),
        (
            "packages",
            package_body("video-weekend", "fixed", json!(null)),
            Some(public_video),
            true,
        ),
        (
            "activities",
            activity_body("video-private", "anytime"),
            Some(private_video),
            false,
        ),
        (
            "activities",
            activity_body("video-image", "anytime"),
            Some(image),
            false,
        ),
        (
            "activities",
            activity_body("no-video", "anytime"),
            None,
            false,
        ),
    ] {
        let p = create(&app, kind, body).await;
        let (st, _) = save(&app, kind, &p, json!({ "video_media_id": video })).await;
        assert_eq!(st, StatusCode::OK);
        sqlx::query(
            "UPDATE products SET status = 'published', published_at = now() WHERE id = $1::uuid",
        )
        .bind(p["id"].as_str().unwrap())
        .execute(&db)
        .await
        .unwrap();
        cases.push((
            p["translations"]["fr"]["slug"]
                .as_str()
                .unwrap()
                .to_string(),
            expected,
        ));
    }
    for (slug, expected) in cases {
        let (st, page) = app
            .request(
                Method::GET,
                &format!("/v1/catalog/products/{slug}"),
                None,
                None,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{slug}: {page}");
        if expected {
            assert_eq!(page["video"]["id"], public_video.to_string(), "{slug}");
            assert!(
                page["video"]["url"]
                    .as_str()
                    .unwrap()
                    .contains(&public_video.to_string())
            );
        } else {
            assert!(page["video"].is_null(), "{slug}: {}", page["video"]);
        }
    }
}
