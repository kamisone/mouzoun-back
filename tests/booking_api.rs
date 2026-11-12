//! The public booking API end to end (real router, Postgres, Redis; fake payment provider).

mod common;

use axum::http::{Method, StatusCode};
use chrono::Utc;
use common::{TestApp, fixtures::*};
use mouzoun_api::services::{
    booking,
    payments::{fake, stripe},
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn open_booking(db: &PgPool) {
    sqlx::query("UPDATE settings SET value = 'true' WHERE key = 'booking.enabled'")
        .execute(db)
        .await
        .unwrap();
}

async fn rate_plan(db: &PgPool) -> Uuid {
    let policy: Uuid = sqlx::query_scalar(
        "INSERT INTO cancellation_policies (code, rules) VALUES ('flex', $1) RETURNING id",
    )
    .bind(json!([
        {"min_hours_before": 168, "refund_percent": 100},
        {"min_hours_before": 0, "refund_percent": 0}
    ]))
    .fetch_one(db)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO rate_plans (code, cancellation_policy_id, payment_mode, deposit_type, deposit_percent)
         VALUES ('flexible', $1, 'optional_online', 'percent', 30) RETURNING id",
    )
    .bind(policy)
    .fetch_one(db)
    .await
    .unwrap()
}

fn stay(product: Uuid, adult: Uuid) -> Value {
    json!({ "kind": "stay", "product_id": product, "arrival": "2027-02-10", "departure": "2027-02-12",
            "guests": [{ "guest_category_id": adult, "count": 2 }] })
}

fn customer() -> Value {
    json!({ "first_name": "Amina", "last_name": "Test", "email": "amina@example.com", "phone": "+212 600 000 000",
            "country_code": "MA" })
}

async fn quote(app: &TestApp, items: Value) -> Value {
    let (status, body) = app
        .request(
            Method::POST,
            "/v1/quote",
            Some(json!({ "items": items })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn checkout(
    app: &TestApp,
    items: Value,
    token: &str,
    option: &str,
    key: &str,
) -> (StatusCode, Value) {
    let (status, _, body) = app
        .send(
            Method::POST,
            "/v1/checkout",
            Some(json!({
                "items": items, "quote_token": token, "customer": customer(), "payment_option": option,
                "accept_terms": true,
            })),
            &[("idempotency-key", key.to_string())],
        )
        .await;
    (status, body)
}

#[sqlx::test(migrations = "./migrations")]
async fn deposit_checkout_payment_webhook_view_and_cancellation(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    open_booking(&db).await;
    rate_plan(&db).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 1).await;
    let items = json!([stay(tent, guests.adult)]);

    let q = quote(&app, items.clone()).await;
    assert_eq!(q["quote"]["total_minor"], 6_000);
    let token = q["quote_token"].as_str().unwrap().to_string();

    let (status, first) = checkout(&app, items.clone(), &token, "deposit", "key-1").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["booking"]["status"], "awaiting_payment");
    assert_eq!(first["booking"]["due_now_minor"], 1_800);
    assert!(
        first["payment"]["client_secret"]
            .as_str()
            .unwrap()
            .starts_with("fake_pi_")
    );
    // A double click (same Idempotency-Key) never books twice.
    let (_, again) = checkout(&app, items.clone(), &token, "deposit", "key-1").await;
    assert_eq!(again["booking"]["reference"], first["booking"]["reference"]);
    let bookings: i64 = sqlx::query_scalar("SELECT count(*) FROM bookings")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(bookings, 1);

    let reference = first["booking"]["reference"].as_str().unwrap();
    let access = first["access_token"].as_str().unwrap();
    let (status, s) = app
        .request(
            Method::GET,
            &format!("/v1/bookings/{reference}/status?t={access}"),
            None,
            None,
        )
        .await;
    assert_eq!(
        (status, s["status"].as_str()),
        (StatusCode::OK, Some("awaiting_payment"))
    );
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/v1/bookings/{reference}?t=wrong"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The provider confirms the payment (fake provider = same webhook processing).
    let payment_id = first["payment"]["payment_id"].as_str().unwrap();
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/payments/fake/{payment_id}/succeed"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (_, view) = app
        .request(
            Method::GET,
            &format!("/v1/bookings/{reference}?t={access}"),
            None,
            None,
        )
        .await;
    assert_eq!(view["status"], "confirmed");
    assert_eq!(view["totals"]["paid_minor"], 1_800);
    assert_eq!(view["totals"]["balance_minor"], 4_200);
    assert_eq!(view["can"]["pay"], true);

    // A duplicate webhook delivery is processed once.
    let provider_payment_id: String =
        sqlx::query_scalar("SELECT provider_payment_id FROM payments WHERE id = $1::uuid")
            .bind(payment_id)
            .fetch_one(&db)
            .await
            .unwrap();
    let event = json!({ "id": "evt_dup", "type": "payment_intent.succeeded",
                        "data": { "object": { "id": provider_payment_id, "amount_received": 1800 } } });
    let body = serde_json::to_vec(&event).unwrap();
    for _ in 0..2 {
        let signature = stripe::sign(&body, fake::WEBHOOK_SECRET, Utc::now().timestamp());
        let (status, _, _) = app
            .send(
                Method::POST,
                "/v1/payments/webhooks/fake",
                Some(event.clone()),
                &[("stripe-signature", signature)],
            )
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (events, confirmed): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM payment_webhook_events WHERE event_id = 'evt_dup' AND processed_at IS NOT NULL),
                (SELECT count(*) FROM booking_status_history WHERE to_status = 'confirmed')",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!((events, confirmed), (1, 1));
    // A forged signature is refused.
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/payments/webhooks/fake",
            Some(event),
            &[("stripe-signature", "t=1,v1=00".into())],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Cancelling more than 7 days ahead: everything paid comes back, shown before confirming.
    let (_, preview) = app
        .request(
            Method::GET,
            &format!("/v1/bookings/{reference}/cancellation?t={access}"),
            None,
            None,
        )
        .await;
    assert_eq!(
        (preview["allowed"].clone(), preview["refund_minor"].clone()),
        (json!(true), json!(1_800))
    );
    let (status, cancelled) = app
        .request(
            Method::POST,
            &format!("/v1/bookings/{reference}/cancel?t={access}"),
            Some(json!({ "reason": "plans changed" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["refund_minor"], 1_800);
    assert_eq!(cancelled["refunds"][0]["status"], "succeeded");
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 2, 10), d(2027, 2, 12)).await,
        vec![0, 0]
    );
    let (_, view) = app
        .request(
            Method::GET,
            &format!("/v1/bookings/{reference}?t={access}"),
            None,
            None,
        )
        .await;
    assert_eq!(
        (
            view["status"].as_str(),
            view["totals"]["paid_minor"].as_i64()
        ),
        (Some("cancelled"), Some(0))
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn stale_quotes_closed_booking_and_pay_at_camp(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    rate_plan(&db).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 2).await;
    let items = json!([stay(tent, guests.adult)]);
    let token = quote(&app, items.clone()).await["quote_token"]
        .as_str()
        .unwrap()
        .to_string();

    // booking.enabled = false (seeded default): the funnel is closed, never broken.
    let (status, body) = checkout(&app, items.clone(), &token, "on_site", "k1").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, Some("booking_closed"))
    );
    open_booking(&db).await;
    app.state.cache.delete(["settings:booking"]).await;

    // A token for another basket (or tampered) is not trusted: the current price comes back.
    let other = json!([{ "kind": "stay", "product_id": tent, "arrival": "2027-02-10", "departure": "2027-02-11",
                         "guests": [{ "guest_category_id": guests.adult, "count": 2 }] }]);
    let (status, body) = checkout(&app, other, &token, "on_site", "k2").await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("quote_expired"))
    );
    assert_eq!(body["error"]["details"]["quote"]["total_minor"], 3_000);

    // Pay at the camp: confirmed at once, no payment.
    let (status, body) = checkout(&app, items, &token, "on_site", "k3").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["booking"]["status"], "confirmed");
    assert!(body["payment"].is_null());
}

#[sqlx::test(migrations = "./migrations")]
async fn a_payment_after_the_hold_expired_is_refunded(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    open_booking(&db).await;
    rate_plan(&db).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 1).await;
    let items = json!([stay(tent, guests.adult)]);
    let token = quote(&app, items.clone()).await["quote_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, created) = checkout(&app, items, &token, "full", "late").await;
    let payment_id = created["payment"]["payment_id"]
        .as_str()
        .unwrap()
        .to_string();

    sqlx::query("UPDATE bookings SET hold_expires_at = now() - interval '1 minute'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(booking::expire_holds(&db, Utc::now(), 10).await.unwrap(), 1);
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 2, 10), d(2027, 2, 12)).await,
        vec![0, 0]
    );

    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/payments/fake/{payment_id}/succeed"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (booking_status, paid, refund, task): (String, i64, String, i64) = sqlx::query_as(
        "SELECT b.status, b.amount_paid_minor,
                (SELECT status FROM refunds LIMIT 1),
                (SELECT count(*) FROM staff_tasks WHERE kind = 'payment_after_expiry')
         FROM bookings b",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        (booking_status.as_str(), paid, refund.as_str(), task),
        ("expired", 0, "succeeded", 1)
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_basket_reprices_and_reports_changes(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    rate_plan(&db).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 2).await;
    let (status, headers, body) = app
        .send(
            Method::POST,
            "/v1/cart/items",
            Some(stay(tent, guests.adult)),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["quote"]["total_minor"], 6_000);
    let cookie = headers
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(cookie.starts_with("__Host-mz_cart="), "{cookie}");

    sqlx::query("UPDATE prices SET amount_minor = 3500 WHERE product_id = $1")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    let (_, _, body) = app
        .send(Method::GET, "/v1/cart", None, &[("cookie", cookie.clone())])
        .await;
    assert_eq!(body["quote"]["total_minor"], 7_000);
    assert_eq!(body["notices"][0]["code"], "price_changed");
    assert_eq!(
        (
            body["notices"][0]["old_minor"].as_i64(),
            body["notices"][0]["new_minor"].as_i64()
        ),
        (Some(6_000), Some(7_000))
    );
    // Seen once: no notice the next time.
    let (_, _, body) = app
        .send(Method::GET, "/v1/cart", None, &[("cookie", cookie.clone())])
        .await;
    assert_eq!(body["notices"], json!([]));

    let item = body["items"][0]["id"].as_str().unwrap().to_string();
    let (_, _, body) = app
        .send(
            Method::DELETE,
            &format!("/v1/cart/items/{item}"),
            None,
            &[("cookie", cookie)],
        )
        .await;
    assert_eq!(body["count"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn find_my_booking_sends_a_single_use_link(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    open_booking(&db).await;
    rate_plan(&db).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 2).await;
    let items = json!([stay(tent, guests.adult)]);
    let token = quote(&app, items.clone()).await["quote_token"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, created) = checkout(&app, items, &token, "on_site", "find").await;
    let reference = created["booking"]["reference"].as_str().unwrap();

    // Wrong email: same answer, no email.
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/bookings/find",
            Some(json!({ "reference": reference, "email": "x@y.zz" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/bookings/find",
            Some(json!({ "reference": reference.to_lowercase(), "email": "AMINA@example.com" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let links: Vec<Value> = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE event_type = 'booking.access_requested'",
    )
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(links.len(), 1);
    let magic = links[0]["secret"]["token"].as_str().unwrap();

    let (status, body) = app
        .request(
            Method::POST,
            &format!("/v1/bookings/{reference}/access"),
            Some(json!({ "token": magic })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["access_token"], created["access_token"]);
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/v1/bookings/{reference}/access"),
            Some(json!({ "token": magic })),
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::GONE, Some("link_expired"))
    );
}
