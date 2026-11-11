mod common;

use axum::http::{HeaderMap, Method, StatusCode, header};
use common::{ALLOWED_ORIGIN, TestApp};
use mouzoun_api::auth::password;
use serde_json::{Value, json};
use sqlx::PgPool;

const COOKIE: &str = "__Host-mouzoun_session";

async fn create_user(app: &TestApp, email: &str, pass: &str, role: &str) {
    let hash = password::hash(pass.to_string()).await.unwrap();
    sqlx::query(
        "INSERT INTO users (email, password_hash, full_name, role) VALUES ($1, $2, 'U', $3::user_role)",
    )
    .bind(email)
    .bind(hash)
    .bind(role)
    .execute(&app.db)
    .await
    .unwrap();
}

fn set_cookie(headers: &HeaderMap) -> String {
    headers
        .get(header::SET_COOKIE)
        .expect("Set-Cookie header")
        .to_str()
        .unwrap()
        .to_string()
}

/// Logs in and returns the `name=value` pair to send back as `Cookie`.
async fn login(app: &TestApp, email: &str, pass: &str) -> (StatusCode, Option<String>, Value) {
    let (status, headers, body) = app
        .send(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": email, "password": pass })),
            &[],
        )
        .await;
    let cookie = headers
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string());
    (status, cookie, body)
}

#[sqlx::test(migrations = "./migrations")]
async fn login_sets_a_hardened_session_cookie_and_never_exposes_the_token(db: PgPool) {
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", "admin").await;

    let (status, headers, body) = app
        .send(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": " Boss@Mouzoun.com ", "password": "abc123" })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cookie = set_cookie(&headers);
    for attribute in [
        &format!("{COOKIE}="),
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=3600",
    ] {
        assert!(
            cookie.contains(attribute),
            "missing {attribute} in {cookie}"
        );
    }
    assert!(
        !cookie.contains("Domain="),
        "__Host- cookies must not set a Domain"
    );
    assert!(
        body.get("access_token").is_none(),
        "token must never reach JavaScript: {body}"
    );
    assert_eq!(body["user"]["role"], "admin");
    assert_eq!(body["expires_in"], 3600);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_cookie_authenticates_and_logout_ends_the_session(db: PgPool) {
    // Regression: login used to reject passwords < 8 chars with 422, locking out such admins.
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", "admin").await;
    let (_, cookie, _) = login(&app, "boss@mouzoun.com", "abc123").await;
    let cookie = cookie.unwrap();

    let (status, _, me) = app
        .send(
            Method::GET,
            "/v1/auth/me",
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["email"], "boss@mouzoun.com");
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/admin/activities",
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "admin routes accept the session cookie"
    );

    let (status, headers, _) = app
        .send(Method::POST, "/v1/auth/logout", None, &[("cookie", cookie)])
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cleared = set_cookie(&headers);
    assert!(
        cleared.starts_with(&format!("{COOKIE}=;")) && cleared.contains("Max-Age=0"),
        "{cleared}"
    );

    let (status, _, _) = app.send(Method::GET, "/v1/auth/me", None, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn a_customer_session_cannot_reach_the_back_office(db: PgPool) {
    let app = TestApp::new(db).await;
    create_user(&app, "client@mouzoun.com", "abc123", "customer").await;
    let (status, cookie, _) = login(&app, "client@mouzoun.com", "abc123").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/admin/activities",
            None,
            &[("cookie", cookie.unwrap())],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn state_changing_requests_from_foreign_origins_are_rejected(db: PgPool) {
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", "admin").await;
    let (_, cookie, _) = login(&app, "boss@mouzoun.com", "abc123").await;
    let cookie = cookie.unwrap();
    let activity = json!({
        "slug": "csrf", "title_fr": "Titre", "title_en": "Title", "description_fr": "",
        "description_en": "", "price_mad": 1, "duration_minutes": 1, "cover_media_id": null,
        "is_published": false,
    });

    let evil = [
        ("cookie", cookie.clone()),
        ("origin", "https://evil.example".to_string()),
    ];
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/admin/activities",
            Some(activity.clone()),
            &evil,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-site write");
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": "boss@mouzoun.com", "password": "abc123" })),
            &[("origin", "https://evil.example".to_string())],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "login CSRF");

    let ours = [
        ("cookie", cookie.clone()),
        ("origin", ALLOWED_ORIGIN.to_string()),
    ];
    let (status, _, body) = app
        .send(Method::POST, "/v1/admin/activities", Some(activity), &ours)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/admin/activities",
            None,
            &[
                ("cookie", cookie),
                ("origin", "https://evil.example".to_string()),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "reads are not state-changing");
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
        let (status, cookie, body) = login(&app, email, pass).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{email}/{pass}: {body}");
        assert!(cookie.is_none(), "no cookie on failure");
    }
    let (status, _, _) = login(&app, "", "").await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "empty fields are a malformed request"
    );
}
