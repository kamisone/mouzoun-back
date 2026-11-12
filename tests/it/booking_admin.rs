//! Back-office booking API: manual bookings, request decisions, stay lifecycle, permissions.

use crate::common::{JWT_SECRET, TestApp, create_user, fixtures::*};
use axum::http::{Method, StatusCode};
use mouzoun_api::auth::jwt;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn plan(db: &PgPool, mode: &str) {
    sqlx::query(
        "INSERT INTO rate_plans (code, payment_mode, deposit_type, deposit_percent) VALUES ('std', $1, 'percent', 30)",
    )
    .bind(mode)
    .execute(db)
    .await
    .unwrap();
}

fn stay(product: Uuid, adult: Uuid, arrival: &str, departure: &str) -> Value {
    json!({ "kind": "stay", "product_id": product, "arrival": arrival, "departure": departure,
            "guests": [{ "guest_category_id": adult, "count": 2 }] })
}

async fn token_for(db: &PgPool, email: &str, role: &str) -> String {
    let id = create_user(db, email, "x", true, Some(role)).await;
    jwt::issue(JWT_SECRET, id, true, 60).unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn phone_booking_with_cash_payment_and_a_manual_discount(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    plan(&db, "optional_online").await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 2).await;
    let (status, body) = app
        .admin(Method::POST, "/v1/admin/bookings", Some(json!({
            "items": [stay(tent, guests.adult, "2027-02-01", "2027-02-04")],
            "customer": { "first_name": "Hassan", "last_name": "Ait", "phone": "+212 611 111 111" },
            "source": "phone",
            "payment_option": "on_site",
            "skip_guest_emails": true,
            "price_override": { "total": "80.00", "reason": "Regular guest" },
            "payment": { "provider": "cash", "amount": "20" },
            "internal_note": "Arrives late",
        })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap();
    let (_, detail) = app
        .admin(Method::GET, &format!("/v1/admin/bookings/{id}"), None)
        .await;
    assert_eq!(detail["source"], "phone");
    assert_eq!(detail["status"], "confirmed");
    assert_eq!(
        detail["totals"]["total_minor"], 8_000,
        "90 € quoted, 80 € agreed"
    );
    assert_eq!(detail["totals"]["paid_minor"], 2_000);
    assert_eq!(detail["totals"]["balance_minor"], 6_000);
    assert_eq!(detail["skip_guest_emails"], true);
    assert_eq!(detail["notes"][0]["body"], "Arrives late");
    assert!(
        detail["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "booking.price_overridden")
    );

    // Check-in then check-out.
    for status in ["checked_in", "completed"] {
        let (code, body) = app
            .admin(
                Method::POST,
                &format!("/v1/admin/bookings/{id}/status"),
                Some(json!({ "status": status })),
            )
            .await;
        assert_eq!(code, StatusCode::OK, "{body}");
    }
    // A completed stay cannot be cancelled.
    let (code, body) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/bookings/{id}/cancel"),
            Some(json!({})),
        )
        .await;
    assert_eq!(
        (code, body["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("invalid_transition"))
    );

    // List, filters and CSV.
    let (_, list) = app
        .admin(
            Method::GET,
            "/v1/admin/bookings?q=hassan&status=completed",
            None,
        )
        .await;
    assert_eq!(list["total"], 1);
    let (_, list) = app
        .admin(Method::GET, "/v1/admin/bookings?status=pending", None)
        .await;
    assert_eq!(list["total"], 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_request_accepted_by_staff_waits_for_payment_then_confirms(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    plan(&db, "online_required").await;
    sqlx::query("UPDATE settings SET value = 'true' WHERE key = 'booking.enabled'")
        .execute(&db)
        .await
        .unwrap();
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "unit", 1).await;
    sqlx::query("UPDATE products SET booking_mode = 'on_request' WHERE id = $1")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    let items = json!([stay(tent, guests.adult, "2027-02-10", "2027-02-12")]);
    let (_, q) = app
        .request(
            Method::POST,
            "/v1/quote",
            Some(json!({ "items": items })),
            None,
        )
        .await;
    assert_eq!(q["quote"]["is_request"], true);
    let (status, created) = app
        .request(Method::POST, "/v1/checkout", Some(json!({
            "items": items, "quote_token": q["quote_token"], "payment_option": "full", "accept_terms": true,
            "customer": { "first_name": "Lea", "last_name": "M", "email": "lea@example.com" },
        })), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["booking"]["status"], "pending");
    assert!(created["payment"].is_null(), "a request is never charged");
    assert_eq!(
        reserved_nights(&db, tent, d(2027, 2, 10), d(2027, 2, 12)).await,
        vec![1, 1],
        "held while waiting"
    );

    let id: Uuid = sqlx::query_scalar("SELECT id FROM bookings")
        .fetch_one(&db)
        .await
        .unwrap();
    let (_, dash) = app
        .admin(Method::GET, "/v1/admin/dashboard/today", None)
        .await;
    assert_eq!(dash["requests"][0]["id"], json!(id));

    let (status, body) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/bookings/{id}/accept"),
            None,
        )
        .await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::OK, Some("awaiting_payment"))
    );
    let event: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE event_type = 'booking.request_accepted'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(event, 1, "the guest gets the payment link");

    // The guest pays from the link (booking page).
    let reference = created["booking"]["reference"].as_str().unwrap();
    let access = created["access_token"].as_str().unwrap();
    let (status, pay) = app
        .request(
            Method::POST,
            &format!("/v1/bookings/{reference}/pay?t={access}"),
            Some(json!({})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{pay}");
    assert_eq!(pay["payment"]["amount_minor"], 6_000);
    let payment_id = pay["payment"]["payment_id"].as_str().unwrap();
    app.request(
        Method::POST,
        &format!("/v1/payments/fake/{payment_id}/succeed"),
        None,
        None,
    )
    .await;
    let (_, view) = app
        .request(
            Method::GET,
            &format!("/v1/bookings/{reference}?t={access}"),
            None,
            None,
        )
        .await;
    assert_eq!(view["status"], "confirmed");

    // Declining is only for requests.
    let (status, _) = app
        .admin(
            Method::POST,
            &format!("/v1/admin/bookings/{id}/decline"),
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = "./migrations")]
async fn permissions_are_checked_per_endpoint(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    plan(&db, "pay_on_site").await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 2).await;
    let reception = token_for(&db, "reception@test.ma", "reception").await;
    let guide = token_for(&db, "guide@test.ma", "guide").await;
    let body = json!({
        "items": [stay(tent, guests.adult, "2027-02-01", "2027-02-02")],
        "customer": { "first_name": "Walk", "last_name": "In" },
        "source": "walk_in", "payment_option": "on_site",
    });
    let (status, created) = app
        .request(
            Method::POST,
            "/v1/admin/bookings",
            Some(body.clone()),
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let id = created["id"].as_str().unwrap();

    // Reception cannot override prices nor refund; the guide cannot write.
    let mut overridden = body.clone();
    overridden["price_override"] = json!({ "total": "1.00", "reason": "x" });
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/admin/bookings",
            Some(overridden),
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/admin/bookings/{id}/cancel"),
            Some(json!({ "refund": "custom", "amount": "5" })),
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(Method::POST, "/v1/admin/bookings", Some(body), Some(&guide))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/v1/admin/bookings/{id}"),
            None,
            Some(&guide),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "guides read bookings");

    // Reception may cancel without refund.
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/admin/bookings/{id}/cancel"),
            Some(json!({ "refund": "none", "reason": "no show call" })),
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
}
