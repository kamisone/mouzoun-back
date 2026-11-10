mod common;

use axum::http::{Method, StatusCode};
use common::TestApp;
use mouzoun_api::auth::password;
use serde_json::json;
use sqlx::PgPool;

async fn create_user(app: &TestApp, email: &str, pass: &str, role: &str) {
    let hash = password::hash(pass.to_string()).await.unwrap();
    sqlx::query("INSERT INTO users (email, password_hash, full_name, role) VALUES ($1, $2, 'U', $3::user_role)")
        .bind(email)
        .bind(hash)
        .bind(role)
        .execute(&app.db)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn admin_with_a_short_password_can_log_in(db: PgPool) {
    // Regression: login used to reject passwords < 8 chars with 422, locking out such admins.
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", "admin").await;

    let (status, body) = app
        .request(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": " Boss@Mouzoun.com ", "password": "abc123" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["user"]["role"], "admin");
    let token = body["access_token"].as_str().unwrap().to_string();

    let (status, me) = app
        .request(Method::GET, "/v1/auth/me", None, Some(&token))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["email"], "boss@mouzoun.com");
}

#[sqlx::test(migrations = "./migrations")]
async fn wrong_or_malformed_credentials_are_401_never_422(db: PgPool) {
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", "admin").await;
    for (email, pass) in [
        ("boss@mouzoun.com", "nope"),
        ("admin", "abc123"),
        ("nobody@x.com", "1234567"),
    ] {
        let (status, body) = app
            .request(
                Method::POST,
                "/v1/auth/login",
                Some(json!({ "email": email, "password": pass })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{email}/{pass}: {body}");
    }
    // Empty fields are a malformed request, not a credential check.
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": "", "password": "" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}
