//! Customer CRM in the back office.

mod common;

use axum::http::{Method, StatusCode};
use common::{JWT_SECRET, TestApp, create_user};
use mouzoun_api::auth::jwt;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

async fn token_for(db: &PgPool, email: &str, role: &str) -> String {
    let id = create_user(db, email, "x", true, Some(role)).await;
    jwt::issue(JWT_SECRET, id, true, 60).unwrap()
}

async fn customer(db: &PgPool, email: &str, user: Option<Uuid>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO customers (email, first_name, user_id) VALUES ($1, 'Amina', $2) RETURNING id",
    )
    .bind(email)
    .bind(user)
    .fetch_one(db)
    .await
    .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn staff_manage_customers_within_their_permissions(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let manager = token_for(&db, "manager@mouzoun.test", "manager").await;
    let reception = token_for(&db, "reception@mouzoun.test", "reception").await;
    let user = create_user(&db, "amina@example.com", "x", false, None).await;
    let with_account = customer(&db, "amina@example.com", Some(user)).await;
    let duplicate = customer(&db, "amina.b@example.com", None).await;

    let (status, list) = app
        .request(
            Method::GET,
            "/v1/admin/crm/customers?q=amina&account=with",
            None,
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1);
    assert_eq!(list["customers"][0]["has_account"], true);

    // Merging and viewing as the customer are manager actions.
    let merge = format!("/v1/admin/crm/customers/{duplicate}/merge");
    let (status, _) = app
        .request(
            Method::POST,
            &merge,
            Some(json!({ "into": with_account })),
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = app
        .request(
            Method::POST,
            &merge,
            Some(json!({ "into": with_account })),
            Some(&manager),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = app
        .request(
            Method::GET,
            &format!("/v1/admin/crm/customers/{duplicate}"),
            None,
            Some(&manager),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the duplicate is gone");

    let (status, view): (StatusCode, Value) = app
        .request(
            Method::GET,
            &format!("/v1/admin/crm/customers/{with_account}/view-as"),
            None,
            Some(&manager),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["read_only"], true);

    // Suspension blocks sign-in until reactivated; every action is in the history.
    let (status, _) = app
        .request(
            Method::POST,
            &format!("/v1/admin/crm/customers/{with_account}/suspend"),
            None,
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let account_status: String = sqlx::query_scalar("SELECT status FROM users WHERE id = $1")
        .bind(user)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(account_status, "suspended");
    let (_, detail) = app
        .request(
            Method::GET,
            &format!("/v1/admin/crm/customers/{with_account}"),
            None,
            Some(&reception),
        )
        .await;
    let actions: Vec<&str> = detail["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|h| h["action"].as_str())
        .collect();
    assert!(actions.contains(&"customer.suspended"), "{actions:?}");
    assert!(actions.contains(&"customer.viewed_as"), "{actions:?}");

    let (status, report) = app
        .request(
            Method::GET,
            "/v1/admin/crm/reports/accounts",
            None,
            Some(&reception),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{report}");
}
