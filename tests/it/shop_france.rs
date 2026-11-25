//! The shop operated by the French company: VAT by rate and destination (OSS, gross price
//! policy), terms of sale accepted by revision, documents attached to the confirmation, the
//! right of withdrawal, immutable per-entity invoices, cash and camp methods locked, the
//! compliance checklist, the 30-day reference price and relay points.

use mouzoun_api::services::{
    email::{EmailSettings, Mailer, sender::RecordingSender},
    shop::emails::ShopEmailHandler,
};

use super::*;

struct Fr {
    location: Uuid,
    dates: Uuid,
    basket: Uuid,
    fresh: Uuid,
    gift: Uuid,
    dates_product: Uuid,
    terms: Uuid,
}

async fn classify(db: &PgPool, product: Uuid, class: &str, exemption: &str, split: Option<Value>) {
    sqlx::query("UPDATE shop_products SET tax_class_code = $2, withdrawal_exemption = $3, bundle_vat_split = $4 WHERE product_id = $1")
        .bind(product)
        .bind(class)
        .bind(exemption)
        .bind(split)
        .execute(db)
        .await
        .unwrap();
}

/// The French shop: stock in France, home delivery (Colissimo) and relay points (Mondial
/// Relay) in France, dates (5.5 %), a basket (20 %), fresh dates (perishable), a gift box
/// (60 % food / 40 % basket), published terms of sale, bank transfer to pay without Stripe.
async fn shop_fr(db: &PgPool) -> Fr {
    let terms = terms(db).await;
    setting(db, "shop.status", json!("open")).await;
    setting(db, "payments.bank_transfer.enabled", json!(true)).await;
    let profile: Uuid = sqlx::query_scalar("INSERT INTO shipping_profiles (id, code, name, is_default) VALUES ($1, 'standard', 'Standard', true) RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(db)
        .await
        .unwrap();
    let zone: Uuid = sqlx::query_scalar("INSERT INTO shipping_zones (id, code, country_codes) VALUES ($1, 'fr_metro', '{FR}') RETURNING id")
        .bind(Uuid::now_v7())
        .fetch_one(db)
        .await
        .unwrap();
    for (code, carrier, amount, delivery) in [
        ("colissimo", "colissimo", 790, "home"),
        ("mondial_relay", "mondial_relay", 450, "relay_point"),
    ] {
        sqlx::query(
            "INSERT INTO shipping_rates (id, zone_id, profile_id, code, kind, carrier, amount_minor, delivery_type)
             VALUES ($1, $2, $3, $4, 'flat', $5, $6, $7)",
        )
        .bind(Uuid::now_v7())
        .bind(zone)
        .bind(profile)
        .bind(code)
        .bind(carrier)
        .bind(amount)
        .bind(delivery)
        .execute(db)
        .await
        .unwrap();
    }
    let location: Uuid = sqlx::query_scalar(
        "INSERT INTO stock_locations (id, code, fulfills_online_orders, country_code, legal_entity_id)
         VALUES ($1, 'entrepot_fr', true, 'FR', (SELECT id FROM legal_entities WHERE code = 'mouzoun_fr')) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .fetch_one(db)
    .await
    .unwrap();
    let dates_product = product(db, "Dattes Medjool", profile, false, false).await;
    classify(db, dates_product, "food_reduced", "unsealed_hygiene", None).await;
    let dates = variant(db, dates_product, "FR-DATES-500", 1290, 550).await;
    let basket_product = product(db, "Panier osier", profile, false, false).await;
    classify(db, basket_product, "standard", "none", None).await;
    let basket = variant(db, basket_product, "FR-BASKET", 2400, 400).await;
    let fresh_product = product(db, "Dattes fraiches", profile, false, true).await;
    classify(db, fresh_product, "food_reduced", "perishable", None).await;
    let fresh = variant(db, fresh_product, "FR-FRESH", 900, 500).await;
    let gift_product = product(db, "Coffret cadeau", profile, false, false).await;
    classify(
        db,
        gift_product,
        "food_reduced",
        "none",
        Some(json!([{ "tax_class_code": "food_reduced", "share_percent": 60 }, { "tax_class_code": "standard", "share_percent": 40 }])),
    )
    .await;
    let gift = variant(db, gift_product, "FR-GIFT", 5000, 1500).await;
    for v in [dates, basket, fresh, gift] {
        receive(db, v, location, 10, None).await;
    }
    Fr {
        location,
        dates,
        basket,
        fresh,
        gift,
        dates_product,
        terms,
    }
}

const PARIS: &str = r#"{ "first_name": "Claire", "last_name": "Martin", "line1": "12 rue Oberkampf", "city": "Paris",
                         "postal_code": "75011", "country_code": "FR", "phone": "06 12 34 56 78" }"#;

async fn ship_to(v: &mut Visitor<'_>, address: Value) -> (StatusCode, Value) {
    v.call(
        Method::PUT,
        "/v1/shop/cart/fulfillment",
        Some(json!({ "method": "shipping", "shipping_address": address })),
        &[],
    )
    .await
}

async fn order_fr(v: &mut Visitor<'_>, key: &str, extra: Value) -> (StatusCode, Value) {
    let mut body = json!({ "payment_method": "bank_transfer", "email": "claire@example.com", "accept_terms": true });
    for (k, val) in extra.as_object().cloned().unwrap_or_default() {
        body[k] = val;
    }
    v.call(
        Method::POST,
        "/v1/shop/checkout",
        Some(body),
        &[("idempotency-key", key.to_string())],
    )
    .await
}

fn mailer(db: &PgPool, sender: &Arc<RecordingSender>) -> Mailer {
    Mailer::new(
        db.clone(),
        sender.clone(),
        EmailSettings {
            public_site_url: "https://mouzoun.test".into(),
            media_base_url: "https://cdn.test".into(),
            link_secret: JWT_SECRET.into(),
            reply_to: None,
        },
    )
}

/// Runs every due outbox event through the shop handlers, emails recorded by `sender`.
async fn drain_with_emails(db: &PgPool, sender: &Arc<RecordingSender>) {
    let r = registry(db).register(Arc::new(ShopEmailHandler::new(
        db.clone(),
        mailer(db, sender),
        JWT_SECRET.into(),
    )));
    let at = Utc::now() + Duration::seconds(5);
    while process_batch(db, &r, 50, 8, at).await.unwrap().claimed > 0 {}
}

async fn pay_and_deliver(app: &TestApp, id: Uuid, total: i64) {
    let (s, e) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/payments"),
            Some(json!({ "provider": "bank_transfer", "amount_minor": total })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{e}");
    let (s, d) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/shop/orders/{id}/fulfillments"),
            Some(json!({})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let fid = d["created_fulfillment_id"].as_str().unwrap().to_string();
    for action in [
        json!({ "action": "ship", "carrier": "colissimo", "tracking_number": "6A123" }),
        json!({ "action": "deliver" }),
    ] {
        let (s, e) = app
            .admin(
                Method::PATCH,
                &format!("/v1/admin/shop/fulfillments/{fid}"),
                Some(action),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{e}");
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn a_french_order_records_the_terms_the_vat_and_sends_the_documents(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.dates, 2).await;
    v.add(s.basket, 1).await;
    let (st, cart) = ship_to(&mut v, serde_json::from_str(PARIS).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{cart}");
    assert_eq!(
        cart["shipping_address"]["phone"], "+33612345678",
        "phone stored in E.164"
    );

    // The terms box is required (never pre-ticked), and must match the published revision.
    let (st, e) = order_fr(&mut v, "fr-1", json!({ "accept_terms": false })).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert_eq!(e["error"]["details"]["fields"]["accept_terms"], "required");
    let (st, e) = order_fr(
        &mut v,
        "fr-2",
        json!({ "terms_revision_id": Uuid::now_v7() }),
    )
    .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("terms_changed"))
    );
    let (st, out) = order_fr(
        &mut v,
        "fr-3",
        json!({ "terms_revision_id": s.terms, "marketing_consent": false }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{out}");
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    let (rev, entity, breakdown, total, tax, accepted): (Option<Uuid>, String, Value, i64, i64, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT o.terms_page_revision_id, e.code::text, o.vat_breakdown, o.total_minor, o.tax_minor, o.terms_accepted_at
         FROM orders o JOIN legal_entities e ON e.id = o.legal_entity_id WHERE o.id = $1",
    )
    .bind(id)
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(rev, Some(s.terms));
    assert!(accepted.is_some());
    assert_eq!(entity, "mouzoun_fr");
    // 2 × 12.90 at 5.5 % + 24.00 at 20 % + 7.90 shipping split between both rates by value.
    assert_eq!(total, 2580 + 2400 + 790);
    let buckets = breakdown.as_array().unwrap();
    assert_eq!(buckets.len(), 2, "{breakdown}");
    let sum = |k: &str| buckets.iter().map(|b| b[k].as_i64().unwrap()).sum::<i64>();
    assert_eq!(sum("gross_minor"), total);
    assert_eq!(sum("vat_minor"), tax);
    assert_eq!(sum("base_minor") + sum("vat_minor"), total);

    // Paid by transfer: the invoice is issued by the French entity (prefix F), sealed, immutable;
    // the confirmation carries the terms of sale and the withdrawal form.
    let sender = Arc::new(RecordingSender::default());
    pay_and_deliver(&app, id, total).await;
    drain_with_emails(&db, &sender).await;
    let (number, sha, vat): (String, Option<String>, Value) =
        sqlx::query_as("SELECT number, pdf_sha256, vat_breakdown FROM invoices WHERE order_id = $1 AND kind = 'invoice'")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(number.starts_with('F'), "{number}");
    assert_eq!(sha.map(|h| h.len()), Some(64));
    assert_eq!(vat, breakdown);
    let changed = sqlx::query("UPDATE invoices SET total_minor = 1 WHERE order_id = $1")
        .bind(id)
        .execute(&db)
        .await;
    assert!(changed.is_err(), "an issued invoice cannot be changed");
    assert!(
        sqlx::query("DELETE FROM invoices WHERE order_id = $1")
            .bind(id)
            .execute(&db)
            .await
            .is_err()
    );
    let sent = sender.sent();
    let confirmation = sent
        .iter()
        .find(|m| !m.attachments.is_empty())
        .expect("confirmation with attachments");
    let names: Vec<&str> = confirmation
        .attachments
        .iter()
        .map(|a| a.filename.as_str())
        .collect();
    assert_eq!(names, vec!["CGV-v1.pdf", "formulaire-retractation.pdf"]);
    assert!(
        confirmation
            .attachments
            .iter()
            .all(|a| a.bytes.starts_with(b"%PDF"))
    );
    assert!(
        confirmation.html.contains("Droit de rétractation"),
        "seller and withdrawal notice in the email"
    );
    let withdrawal_deadline: Option<NaiveDate> =
        sqlx::query_scalar("SELECT withdrawal_deadline FROM orders WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
    assert!(withdrawal_deadline.is_some_and(|d| d >= Utc::now().date_naive() + Duration::days(13)));
}

#[sqlx::test(migrations = "./migrations")]
async fn the_right_of_withdrawal_follows_the_consumer_code(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.dates, 1).await;
    v.add(s.fresh, 1).await;
    v.add(s.basket, 1).await;
    ship_to(&mut v, serde_json::from_str(PARIS).unwrap()).await;
    let (_, out) = order_fr(&mut v, "w-1", json!({})).await;
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    let total: i64 = sqlx::query_scalar("SELECT total_minor FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    pay_and_deliver(&app, id, total).await;
    let items: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, sku::text FROM order_items WHERE order_id = $1")
            .bind(id)
            .fetch_all(&db)
            .await
            .unwrap();
    let item = |sku: &str| items.iter().find(|i| i.1 == sku).unwrap().0;
    let policy = orders::ReturnPolicy {
        withdrawal_days: 14,
        refund_deadline_days: 14,
        guarantee_years: 2,
    };
    let customer = orders::ActorRef::customer(mouzoun_api::services::audit::Actor::customer());
    let ask = |items: Value, at: chrono::DateTime<Utc>| {
        let db = db.clone();
        let customer = customer.clone();
        async move {
            let req: orders::ReturnRequest =
                serde_json::from_value(json!({ "reason": "withdrawal", "items": items })).unwrap();
            let mut tx = db.begin().await.unwrap();
            let r = orders::request_return(&mut tx, id, &req, Some(policy), &customer, at).await;
            tx.commit().await.unwrap();
            r
        }
    };
    let code = |r: Result<Uuid, mouzoun_api::error::AppError>| match r {
        Err(e) => format!("{e:?}"),
        Ok(_) => "ok".into(),
    };
    let now = Utc::now();
    // Perishable food: no withdrawal.
    assert!(
        code(
            ask(
                json!([{ "order_item_id": item("FR-FRESH"), "quantity": 1 }]),
                now
            )
            .await
        )
        .contains("withdrawal_excluded")
    );
    // Hygiene-sensitive goods: only while sealed.
    assert!(
        code(
            ask(
                json!([{ "order_item_id": item("FR-DATES-500"), "quantity": 1 }]),
                now
            )
            .await
        )
        .contains("withdrawal_unsealed")
    );
    // After 14 days: refused.
    assert!(
        code(
            ask(
                json!([{ "order_item_id": item("FR-BASKET"), "quantity": 1 }]),
                now + Duration::days(15)
            )
            .await
        )
        .contains("withdrawal_period_over")
    );
    let ok = ask(
        json!([{ "order_item_id": item("FR-DATES-500"), "quantity": 1, "still_sealed": true }, { "order_item_id": item("FR-BASKET"), "quantity": 1 }]),
        now,
    )
    .await
    .unwrap();
    let due: Option<NaiveDate> =
        sqlx::query_scalar("SELECT refund_due_by FROM order_returns WHERE id = $1")
            .bind(ok)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(
        due,
        Some(mouzoun_api::camp_time::local(now).date() + Duration::days(14))
    );
    // Acknowledged by email; the near-deadline list shows it to staff.
    let sender = Arc::new(RecordingSender::default());
    drain_with_emails(&db, &sender).await;
    assert!(
        sender
            .sent()
            .iter()
            .any(|m| m.subject.contains("rétractation")),
        "{:?}",
        sender.sent().iter().map(|m| &m.subject).collect::<Vec<_>>()
    );
    let (st, list) = app
        .admin(Method::GET, "/v1/admin/shop/withdrawals", None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list["data"][0]["days_left"], 14);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_french_seller_has_no_cash_no_camp_delivery_and_opens_only_when_compliant(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    let (_, config) = app
        .request(Method::GET, "/v1/shop/config", None, None)
        .await;
    assert_eq!(
        config["fulfillment_methods"],
        json!(["shipping", "relay_point"])
    );
    assert_eq!(config["stay_delivery_enabled"], false);
    assert_eq!(
        config["checkout_button_label"],
        "Commander avec obligation de paiement"
    );
    assert!(
        !config["payment_methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m == "cash_on_delivery" || m == "pay_at_pickup")
    );
    for (key, code) in [
        ("payments.cod.enabled", "cash_locked_fr"),
        ("payments.pay_at_pickup.enabled", "cash_locked_fr"),
        ("shop.stay_delivery_enabled", "stay_delivery_unavailable"),
    ] {
        let (st, e) = app
            .admin(
                Method::PATCH,
                "/v1/admin/shop/settings",
                Some(json!({ key: true })),
            )
            .await;
        assert_eq!(
            (st, e["error"]["code"].as_str()),
            (StatusCode::CONFLICT, Some(code)),
            "{key}"
        );
    }
    let (st, e) = app
        .admin(
            Method::PATCH,
            "/v1/admin/shop/settings",
            Some(json!({ "shop.checkout_button_label": { "fr": "Payer" } })),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let mut v = Visitor::new(&app);
    v.add(s.dates, 1).await;
    for method in ["pickup", "stay_delivery"] {
        let (st, e) = v
            .call(
                Method::PUT,
                "/v1/shop/cart/fulfillment",
                Some(json!({ "method": method })),
                &[],
            )
            .await;
        assert_eq!(
            (st, e["error"]["code"].as_str()),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Some("fulfillment_unavailable")
            ),
            "{method}"
        );
    }
    // Corsica-style rules: French addresses need a 5-digit postal code and a valid phone.
    let (st, e) = ship_to(&mut v, json!({ "first_name": "A", "last_name": "B", "line1": "x", "city": "Paris", "country_code": "FR", "phone": "12" })).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(e["error"]["details"]["fields"]["postal_code"], "required");
    assert_eq!(e["error"]["details"]["fields"]["phone"], "invalid");

    // Placeholder identity, unconfirmed rates, no mediator: the shop cannot open.
    setting(&db, "shop.status", json!("catalog_only")).await;
    let (st, e) = app
        .admin(
            Method::PATCH,
            "/v1/admin/shop/settings",
            Some(json!({ "shop.status": "open" })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("compliance_checklist")),
        "{e}"
    );
    let failing: Vec<&str> = e["error"]["details"]["failing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["code"].as_str().unwrap())
        .collect();
    for c in [
        "seller_identity",
        "vat_rates_confirmed",
        "consumer_mediator",
        "card_payment",
    ] {
        assert!(failing.contains(&c), "{failing:?}");
    }
    let (st, c) = app
        .admin(Method::GET, "/v1/admin/shop/compliance", None)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(c["ok"], false);
    let _ = s.location;
}

#[sqlx::test(migrations = "./migrations")]
async fn eu_vat_follows_oss_and_the_gross_price_policy(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    setting(&db, "shop.ships_to_countries", json!(["FR", "DE"])).await;
    sqlx::query("INSERT INTO shipping_zones (id, code, country_codes) VALUES ($1, 'de', '{DE}')")
        .bind(Uuid::now_v7())
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("UPDATE tax_rates SET is_active = true WHERE country_code = 'DE'")
        .execute(&db)
        .await
        .unwrap();
    let berlin = json!({ "first_name": "Jonas", "last_name": "Weber", "line1": "Torstr. 1", "city": "Berlin", "postal_code": "10119",
                         "country_code": "DE", "phone": "+4930123456" });
    let mut v = Visitor::new(&app);
    v.add(s.dates, 1).await;
    let rate_of = |cart: &Value| {
        cart["totals"]["vat_breakdown"][0]["rate"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };
    // OSS off (under the threshold): French VAT.
    let (_, cart) = ship_to(&mut v, berlin.clone()).await;
    assert_eq!(rate_of(&cart), "5.5", "{cart}");
    // OSS on: German VAT (7 % on food), same price incl. VAT by default.
    let (st, e) = app
        .admin(
            Method::PATCH,
            "/v1/admin/shop/settings",
            Some(json!({ "tax.oss_enabled": true })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{e}");
    let (_, cart) = ship_to(&mut v, berlin.clone()).await;
    assert_eq!(rate_of(&cart), "7");
    assert_eq!(cart["items"][0]["unit_price_minor"], 1290);
    // Net price constant instead: the gross follows the German rate (12.90 × 107 / 105.5).
    let (st, e) = app
        .admin(
            Method::PATCH,
            "/v1/admin/shop/settings",
            Some(json!({ "tax.keep_gross_price_constant": false })),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{e}");
    let (_, cart) = ship_to(&mut v, berlin).await;
    assert_eq!(cart["items"][0]["unit_price_minor"], 1308);
    let (_, rates) = app
        .admin(Method::GET, "/v1/admin/shop/tax-rates", None)
        .await;
    assert_eq!(rates["oss"]["level"], "ok");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_gift_box_splits_its_vat_and_relay_points_are_priced(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    let mut v = Visitor::new(&app);
    v.add(s.gift, 1).await;
    let (_, points) = v
        .call(
            Method::GET,
            "/v1/shop/relay-points?postal_code=75011",
            None,
            &[],
        )
        .await;
    let point = points["data"][0]["id"].as_str().unwrap().to_string();
    let (st, e) = v
        .call(
            Method::PUT,
            "/v1/shop/cart/fulfillment",
            Some(
                json!({ "method": "relay_point", "relay_point_id": "mondial_relay:7501-1",
              "shipping_address": serde_json::from_str::<Value>(PARIS).unwrap() }),
            ),
            &[],
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let (st, cart) = v
        .call(
            Method::PUT,
            "/v1/shop/cart/fulfillment",
            Some(json!({ "method": "relay_point", "relay_point_id": point,
              "shipping_address": serde_json::from_str::<Value>(PARIS).unwrap() })),
            &[],
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{cart}");
    assert_eq!(cart["shipping_lines"][0]["amount_minor"], 450);
    assert_eq!(cart["relay_point"]["id"], point);
    let rates: Vec<&str> = cart["totals"]["vat_breakdown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["rate"].as_str().unwrap())
        .collect();
    assert_eq!(rates, vec!["20", "5.5"]);
    // 50.00 + 4.50: 60 % / 40 % for the goods and the shipping alike.
    let gross: Vec<i64> = cart["totals"]["vat_breakdown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["gross_minor"].as_i64().unwrap())
        .collect();
    assert_eq!(gross, vec![2000 + 180, 3000 + 270]);
    let (st, out) = order_fr(&mut v, "relay-1", json!({})).await;
    assert_eq!(st, StatusCode::OK, "{out}");
    let relay: Value = sqlx::query_scalar("SELECT relay_point FROM orders WHERE number = $1")
        .bind(out["order"]["number"].as_str().unwrap())
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(relay["id"], point);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_reduction_refers_to_the_lowest_price_of_the_last_30_days(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    let uri = format!("/v1/admin/shop/products/{}/variants", s.dates_product);
    // No earlier price: a strike-through price cannot be shown.
    let (st, e) = app
        .admin(
            Method::PATCH,
            &uri,
            Some(json!([{ "id": s.dates, "compare_at_price_minor": 1500 }])),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("compare_at_above_reference")
        ),
        "{e}"
    );
    // Price lowered from 12.90: the reference is 12.90, not more.
    let (st, e) = app
        .admin(
            Method::PATCH,
            &uri,
            Some(json!([{ "id": s.dates, "price_minor": 990, "compare_at_price_minor": 1500 }])),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    let (st, e) = app
        .admin(
            Method::PATCH,
            &uri,
            Some(json!([{ "id": s.dates, "price_minor": 990 }])),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{e}");
    let (st, p) = app
        .admin(
            Method::PATCH,
            &uri,
            Some(json!([{ "id": s.dates, "compare_at_price_minor": 1290 }])),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{p}");
    let v = p["variants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == json!(s.dates))
        .unwrap()
        .clone();
    assert_eq!(v["reference_price_minor"], 1290);
    assert_eq!(v["lowest_30d_minor"], 990);
}

// ---- Launch: legal pages, seller identity from the environment, placeholder rates -------------

async fn install_pages(app: &TestApp) {
    let email = format!("lawyer-{}@mouzoun.test", Uuid::now_v7());
    crate::common::create_user(&app.db, &email, "x", true, Some("super_admin")).await;
    let staff = mouzoun_api::services::shop::launch::staff_for(&app.db, Some(&email))
        .await
        .unwrap();
    mouzoun_api::services::shop::launch::install_legal_pages(
        &app.db,
        &app.state.sections,
        &staff,
        false,
    )
    .await
    .unwrap();
}

async fn page_body(app: &TestApp, slug: &str) -> String {
    let (st, page) = app
        .request(
            Method::GET,
            &format!("/v1/pages/by-slug/{slug}?locale=fr"),
            None,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{page}");
    page["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["content"]["body"].as_str().unwrap_or("").to_string())
        .collect()
}

/// The withdrawal page lists the exempt products from the data (perishable, unsealed), never
/// the others; tokens are filled; the cookie and delivery tables come from the data.
#[sqlx::test(migrations = "./migrations")]
async fn the_legal_pages_render_from_the_data(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let _s = shop_fr(&db).await;
    install_pages(&app).await;
    let body = page_body(&app, "droit-de-retractation").await;
    assert!(!body.contains("{{"), "every token is filled");
    let perishable = body.find("détériorer").expect("perishable exemption");
    let unsealed = body.find("descellés").expect("unsealed exemption");
    assert!(body[perishable..].contains("Dattes fraiches"));
    assert!(body[unsealed..].contains("Dattes Medjool"));
    assert!(!body.contains("Panier osier") && !body.contains("Coffret cadeau"));
    assert!(body.contains("withdrawal-form.pdf"));
    assert!(body.contains("Formulaire") || body.contains("formulaire"));
    // The cookie policy is the generated inventory.
    let cookies = page_body(&app, "cookies").await;
    assert!(cookies.contains("__Host-mz_shop_cart") && cookies.contains("mz-consent"));
    // The delivery page shows the rates of the data.
    let shipping = page_body(&app, "livraison-et-retours").await;
    assert!(
        shipping.contains("<table>") && shipping.contains("7,90"),
        "{shipping}"
    );
}

/// The builder's picker lists exactly the tokens the renderer knows.
#[sqlx::test(migrations = "./migrations")]
async fn the_token_picker_offers_known_tokens(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (st, body) = app
        .admin(Method::GET, "/v1/admin/pages/legal-tokens", None)
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let tokens: Vec<String> = serde_json::from_value(body["tokens"].clone()).unwrap();
    assert!(
        tokens.contains(&"seller.legal_name".to_string())
            && tokens.contains(&"mediator.url".to_string())
    );
    let known = mouzoun_api::services::shop::legal_pages::KNOWN_TOKENS;
    assert!(tokens.iter().all(|t| known.contains(&t.as_str())));
}

/// A legal page cannot be published with a token the renderer does not know.
#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_token_fails_publication(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    install_pages(&app).await;
    let page: Uuid = sqlx::query_scalar(
        "SELECT p.id FROM pages p JOIN page_translations t ON t.page_id = p.id WHERE t.slug = 'mentions-legales'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let (st, tree) = app
        .admin(
            Method::GET,
            &format!("/v1/admin/pages/{page}/builder"),
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{tree}");
    let mut blocks = tree["blocks"].clone();
    blocks[0]["content"]["fr"]["body"] =
        json!("<p>Éditeur : {{seller.legal_name}}, surnom {{seller.nickname}}</p>");
    let (st, _, saved) = app
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
    assert!(st.is_success(), "save: {st} {saved}");
    let (st, e) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/pages/{page}/publish"),
            Some(json!({})),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{e}");
    assert_eq!(e["error"]["code"], "unknown_tokens");
    assert_eq!(e["error"]["details"]["tokens"], json!(["seller.nickname"]));
}

/// Placeholder rates are never offered, do not count for the checklist, and are confirmed only
/// all together once each was seen.
#[sqlx::test(migrations = "./migrations")]
async fn placeholder_rates_are_not_offered_until_confirmed(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    sqlx::query("DELETE FROM shipping_rates")
        .execute(&db)
        .await
        .unwrap();
    let launch = mouzoun_api::services::shop::launch::seed_placeholder_shipping;
    assert_eq!(launch(&db).await.unwrap(), 10);
    assert_eq!(launch(&db).await.unwrap(), 0, "idempotent");

    let mut v = Visitor::new(&app);
    v.add(s.dates, 1).await;
    let (st, cart) = ship_to(&mut v, serde_json::from_str(PARIS).unwrap()).await;
    assert!(
        !cart.to_string().contains("fr_colissimo_0_1kg"),
        "a placeholder is never offered ({st}): {cart}"
    );
    let shipping_item = |c: &Value| {
        c["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["code"] == "shipping_rates_france")
            .cloned()
            .unwrap()
    };
    let (_, c) = app
        .admin(Method::GET, "/v1/admin/shop/compliance", None)
        .await;
    let item = shipping_item(&c);
    assert_eq!(item["ok"], json!(false));
    assert_eq!(item["detail"]["placeholders"], json!(10));

    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM shipping_rates WHERE is_placeholder ORDER BY code")
            .fetch_all(&db)
            .await
            .unwrap();
    let (st, e) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/shipping/rates/confirm-placeholders",
            Some(json!({ "ids": &ids[..9] })),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("placeholders_unseen"))
    );
    let (st, r) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/shipping/rates/confirm-placeholders",
            Some(json!({ "ids": ids })),
        )
        .await;
    assert_eq!((st, r["confirmed"].clone()), (StatusCode::OK, json!(10)));
    let (_, c) = app
        .admin(Method::GET, "/v1/admin/shop/compliance", None)
        .await;
    assert_eq!(shipping_item(&c)["ok"], json!(true));
    let (_, cart) = ship_to(&mut v, serde_json::from_str(PARIS).unwrap()).await;
    assert!(cart.to_string().contains("fr_colissimo_0_1kg"), "{cart}");
}

/// The rate editor offers the carriers of the shop settings; another one is refused, except the
/// one a rate already carries (a carrier removed from the list since).
#[sqlx::test(migrations = "./migrations")]
async fn rate_carriers_come_from_the_settings(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    shop_fr(&db).await;
    setting(&db, "shop.carriers", json!(["colissimo", "dpd"])).await;
    let (st, s) = app
        .admin(Method::GET, "/v1/admin/shop/shipping", None)
        .await;
    assert_eq!(
        (st, s["carriers"].clone()),
        (StatusCode::OK, json!(["colissimo", "dpd"]))
    );
    let relay = s["rates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == "mondial_relay")
        .cloned()
        .unwrap();
    let body = |carrier: &str| {
        json!({ "zone_id": relay["zone_id"], "profile_id": relay["profile_id"], "code": "new_rate", "kind": "flat",
                "carrier": carrier, "amount_minor": 500 })
    };
    let (st, _) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/shipping/rates",
            Some(body("dpd")),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (st, e) = app
        .admin(
            Method::POST,
            "/v1/admin/shop/shipping/rates",
            Some(body("amana")),
        )
        .await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid"))
    );
    // "mondial_relay" left the list but stays valid on the rate that uses it.
    let mut keep = body("mondial_relay");
    keep["code"] = json!("mondial_relay");
    let path = format!(
        "/v1/admin/shop/shipping/rates/{}",
        relay["id"].as_str().unwrap()
    );
    let (st, _) = app.admin(Method::PUT, &path, Some(keep)).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = app.admin(Method::PUT, &path, Some(body("amana"))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---- Admin › Settings › Legal entity ------------------------------------------------------------

fn company(name: &str) -> Value {
    json!({
        "legal_name": name, "legal_form": "SAS", "share_capital_minor": 250_000,
        "address": { "line1": "9 rue de l'Essai", "line2": "", "postal_code": "75011", "city": "Paris" },
        "country_code": "FR", "email": "boutique@essai.test", "phone": "+33 1 23 45 67 89",
        "registration": { "siren": "732829320", "siret": "73282932000074", "rcs_city": "Paris",
                          "vat_number": "FR44732829320", "ape_code": "01.25Z" },
        "publication_director": "Camille Essai",
        "mediator": { "name": "Médiateur Essai", "url": "https://mediateur.essai.test", "address": "1 place de l'Essai, 75001 Paris" },
    })
}

const FORM: &str = "/v1/admin/settings/legal-entity";

/// Wrong identifiers come back as field errors (rule codes, never the values); a valid form is
/// saved with an audit row naming the fields; the checklist lines pass and link to the form;
/// staff without the settings permission can neither read nor edit it.
#[sqlx::test(migrations = "./migrations")]
async fn the_legal_entity_form_validates_and_saves(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (st, before) = app.admin(Method::GET, FORM, None).await;
    assert_eq!(st, StatusCode::OK, "{before}");
    let checklist = |c: &Value, code: &str| {
        c["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["code"] == code)
            .cloned()
            .unwrap()
    };
    let (_, c) = app
        .admin(Method::GET, "/v1/admin/shop/compliance", None)
        .await;
    let identity = checklist(&c, "seller_identity");
    assert_eq!(identity["ok"], json!(false));
    assert_eq!(identity["detail"]["link"], "/admin/settings/legal-entity");
    assert!(
        identity["detail"]["missing"]
            .as_array()
            .unwrap()
            .contains(&json!("email"))
    );
    assert_eq!(
        checklist(&c, "consumer_mediator")["detail"]["missing"],
        json!(["mediator.name", "mediator.url"])
    );

    let mut bad = company("ESSAI ALPHA SAS");
    bad["registration"]["siren"] = json!("123456789");
    bad["registration"]["siret"] = json!("12345678900011");
    bad["registration"]["vat_number"] = json!("FR00123456789");
    bad["address"]["postal_code"] = json!("7501");
    let (st, e) = app.admin(Method::PUT, FORM, Some(bad)).await;
    assert_eq!(
        (st, e["error"]["code"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_fields")),
        "{e}"
    );
    let fields = &e["error"]["details"]["fields"];
    assert_eq!(fields["registration.siren"], "luhn");
    assert_eq!(fields["registration.siret"], "luhn");
    assert!(fields["registration.vat_number"].is_string());
    assert_eq!(fields["address.postal_code"], "postal_code");
    assert!(
        !e.to_string().contains("123456789"),
        "no value in the answer"
    );

    let (st, saved) = app
        .admin(Method::PUT, FORM, Some(company("ESSAI ALPHA SAS")))
        .await;
    assert_eq!(st, StatusCode::OK, "{saved}");
    assert!(
        saved["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("registration.siren"))
    );
    assert_eq!(saved["missing"], json!([]));
    let audits: Vec<Value> =
        sqlx::query_scalar("SELECT changes FROM audit_logs WHERE action = 'legal_entity.updated'")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(audits.len(), 1, "one row for the valid save only");
    let audit = audits[0].to_string();
    assert!(audit.contains("mediator.url") && audit.contains("legal_name"));
    for value in [
        "ESSAI ALPHA SAS",
        "732829320",
        "boutique@essai.test",
        "Médiateur Essai",
    ] {
        assert!(!audit.contains(value), "a value in the audit log");
    }
    // Saving the same form again changes nothing.
    let (_, again) = app
        .admin(Method::PUT, FORM, Some(company("ESSAI ALPHA SAS")))
        .await;
    assert_eq!(again["changed"], json!([]));

    let (_, c) = app
        .admin(Method::GET, "/v1/admin/shop/compliance", None)
        .await;
    assert_eq!(checklist(&c, "seller_identity")["ok"], json!(true));
    assert_eq!(checklist(&c, "consumer_mediator")["ok"], json!(true));
    assert!(
        c["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i.get("severity").is_none()),
        "no review step left in the checklist"
    );

    let reception =
        crate::common::create_user(&db, "desk@mouzoun.test", "x", true, Some("reception")).await;
    let reception =
        mouzoun_api::auth::jwt::issue(crate::common::JWT_SECRET, reception, true, 60).unwrap();
    let (st, _) = app.request(Method::GET, FORM, None, Some(&reception)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = app
        .request(
            Method::PUT,
            FORM,
            Some(company("PIRATE SAS")),
            Some(&reception),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// A new company name shows at once on the legal pages and in the footer data, while an invoice
/// already issued keeps the identity it was issued with.
#[sqlx::test(migrations = "./migrations")]
async fn a_company_change_reaches_the_pages_but_not_issued_invoices(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let s = shop_fr(&db).await;
    let (st, _) = app
        .admin(Method::PUT, FORM, Some(company("ANCIEN NOM SAS")))
        .await;
    assert_eq!(st, StatusCode::OK);
    // An invoice issued under the old name.
    let mut v = Visitor::new(&app);
    v.add(s.dates, 1).await;
    ship_to(&mut v, serde_json::from_str(PARIS).unwrap()).await;
    let (st, out) = order_fr(&mut v, "rename-1", json!({ "terms_revision_id": s.terms })).await;
    assert_eq!(st, StatusCode::OK, "{out}");
    let id = order_id(&db, out["order"]["number"].as_str().unwrap()).await;
    let total: i64 = sqlx::query_scalar("SELECT total_minor FROM orders WHERE id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
    pay_and_deliver(&app, id, total).await;
    drain_with_emails(&db, &Arc::new(RecordingSender::default())).await;
    let snapshot_name = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT seller_snapshot->>'legal_name' FROM invoices WHERE order_id = $1 AND kind = 'invoice'",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap()
    };
    assert_eq!(snapshot_name().await, "ANCIEN NOM SAS");

    install_pages(&app).await;
    assert!(
        page_body(&app, "mentions-legales")
            .await
            .contains("ANCIEN NOM SAS")
    );
    let (st, _) = app
        .admin(Method::PUT, FORM, Some(company("NOUVEAU NOM SAS")))
        .await;
    assert_eq!(st, StatusCode::OK);

    let notice = page_body(&app, "mentions-legales").await;
    assert!(
        notice.contains("NOUVEAU NOM SAS") && !notice.contains("ANCIEN NOM SAS"),
        "the cached page was dropped"
    );
    let (_, config) = app
        .request(Method::GET, "/v1/shop/config", None, None)
        .await;
    assert_eq!(
        config["seller"]["legal_name"], "NOUVEAU NOM SAS",
        "footer data"
    );
    assert_eq!(
        snapshot_name().await,
        "ANCIEN NOM SAS",
        "an issued invoice never changes"
    );
}
