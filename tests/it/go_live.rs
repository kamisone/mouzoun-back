//! Booking opens only when the go-live checklist passes; closing it always works.

use crate::common::{JWT_SECRET, TestApp, create_user, fixtures::*};
use axum::http::{Method, StatusCode};
use chrono::Utc;
use mouzoun_api::{auth::jwt, camp_time, services::inventory};
use serde_json::{Value, json};
use sqlx::PgPool;

async fn manager(db: &PgPool) -> String {
    let id = create_user(db, "manager@mouzoun.test", "x", true, Some("manager")).await;
    jwt::issue(JWT_SECRET, id, true, 60).unwrap()
}

fn failing(body: &Value) -> Vec<String> {
    body["error"]["details"]["failing"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[sqlx::test(migrations = "./migrations")]
async fn booking_opens_only_when_every_required_check_passes(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let token = manager(&db).await;
    let open = |enabled: bool| json!({ "enabled": enabled });

    // A stay priced and open, but only in French and without any rate plan.
    let tent = accommodation(&db, "pool", 3).await;
    inventory::generate_inventory_calendar(&db, Some(tent), camp_time::today(Utc::now()), 3)
        .await
        .unwrap();
    let (status, body) = app
        .request(
            Method::PUT,
            "/v1/admin/booking/enabled",
            Some(open(true)),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let fails = failing(&body);
    assert!(fails.contains(&"rate_plans".to_string()), "{fails:?}");
    assert!(fails.contains(&"content".to_string()), "{fails:?}");
    assert!(!fails.contains(&"prices".to_string()), "{fails:?}");
    assert!(!fails.contains(&"availability".to_string()), "{fails:?}");

    let (_, list) = app
        .request(Method::GET, "/v1/admin/booking/go-live", None, Some(&token))
        .await;
    let content = list["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "content")
        .unwrap();
    let missing: Vec<&str> = content["details"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["locale"].as_str())
        .collect();
    assert!(
        missing.contains(&"en") && missing.contains(&"ar"),
        "{content}"
    );

    // Complete the catalogue: texts in every language, a public rate plan with its policy.
    sqlx::query("UPDATE product_translations SET short_description = 'Sous les étoiles' WHERE product_id = $1")
        .bind(tent)
        .execute(&db)
        .await
        .unwrap();
    for (locale, name) in [("en", "Nomad tent"), ("ar", "خيمة رحّل")] {
        sqlx::query(
            "INSERT INTO product_translations (product_id, locale, name, slug, short_description) VALUES ($1, $2, $3, $4, $3)",
        )
        .bind(tent)
        .bind(locale)
        .bind(name)
        .bind(format!("tent-{locale}-{tent}"))
        .execute(&db)
        .await
        .unwrap();
    }
    let policy: uuid::Uuid = sqlx::query_scalar(
        r#"INSERT INTO cancellation_policies (code, rules) VALUES ('flex', '[{"min_hours_before":168,"refund_percent":100}]') RETURNING id"#,
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let plan: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO rate_plans (code, cancellation_policy_id, payment_mode) VALUES ('standard', $1, 'optional_online') RETURNING id",
    )
    .bind(policy)
    .fetch_one(&db)
    .await
    .unwrap();
    for locale in ["fr", "en", "ar"] {
        sqlx::query("INSERT INTO cancellation_policy_translations (cancellation_policy_id, locale, name) VALUES ($1, $2, 'Flexible')")
            .bind(policy)
            .bind(locale)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO rate_plan_translations (rate_plan_id, locale, name) VALUES ($1, $2, 'Standard')")
            .bind(plan)
            .bind(locale)
            .execute(&db)
            .await
            .unwrap();
    }

    let (status, body) = app
        .request(
            Method::PUT,
            "/v1/admin/booking/enabled",
            Some(open(true)),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
    let (_, config) = app
        .request(Method::GET, "/v1/booking/config", None, None)
        .await;
    assert_eq!(config["enabled"], true, "the public funnel sees it at once");

    // Closing never needs the checklist; reception cannot touch the switch.
    let reception = create_user(&db, "desk@mouzoun.test", "x", true, Some("reception")).await;
    let reception = jwt::issue(JWT_SECRET, reception, true, 60).unwrap();
    let (status, _) = app
        .request(
            Method::PUT,
            "/v1/admin/booking/enabled",
            Some(open(false)),
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = app
        .request(
            Method::PUT,
            "/v1/admin/booking/enabled",
            Some(open(false)),
            Some(&token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
}

#[sqlx::test(migrations = "./migrations")]
async fn without_bot_check_keys_the_site_asks_for_no_token(db: PgPool) {
    let app = TestApp::new(db).await;
    let (_, config) = app
        .request(Method::GET, "/v1/booking/config", None, None)
        .await;
    assert_eq!(config["captcha"], false);
    assert_eq!(config["captcha_site_key"], Value::Null);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_site_gets_the_bot_check_key_from_the_api(db: PgPool) {
    let app = TestApp::with_config(db, |c| {
        c.booking.turnstile_secret = Some("0x4AAAAAAAsecret".into());
        c.booking.turnstile_site_key = Some("0x4AAAAAAAsite".into());
    })
    .await;
    let (_, config) = app
        .request(Method::GET, "/v1/booking/config", None, None)
        .await;
    assert_eq!(config["captcha"], true);
    assert_eq!(config["captcha_site_key"], "0x4AAAAAAAsite");
}
