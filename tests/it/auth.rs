use crate::common::{ALLOWED_ORIGIN, TestApp};
use axum::http::{HeaderMap, Method, StatusCode, header};
use mouzoun_api::auth::password;
use serde_json::{Value, json};
use sqlx::PgPool;

const COOKIE: &str = "__Host-mouzoun_session";

/// These tests check the password step: the second factor (on by default, tested in
/// tests/accounts.rs) is switched off.
async fn without_2fa(app: &TestApp) {
    sqlx::query("UPDATE settings SET value = 'false' WHERE key = 'auth.staff_2fa_required'")
        .execute(&app.db)
        .await
        .unwrap();
}

async fn create_user(app: &TestApp, email: &str, pass: &str, staff: bool) -> uuid::Uuid {
    without_2fa(app).await;
    let hash = password::hash(pass.to_string()).await.unwrap();
    crate::common::create_user(&app.db, email, &hash, staff, staff.then_some("manager")).await
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
    create_user(&app, "boss@mouzoun.com", "abc123", true).await;

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
    assert_eq!(body["user"]["is_staff"], true);
    assert_eq!(body["user"]["roles"], json!(["manager"]));
    let permissions = body["user"]["permissions"].as_array().unwrap();
    assert!(
        permissions.contains(&json!("bookings.write")),
        "manager can write bookings"
    );
    assert!(
        !permissions.contains(&json!("users.manage")),
        "manager cannot manage users"
    );
    assert_eq!(body["expires_in"], 3600);
}

#[sqlx::test(migrations = "./migrations")]
async fn the_cookie_authenticates_and_logout_ends_the_session(db: PgPool) {
    // Regression: login used to reject passwords < 8 chars with 422, locking out such admins.
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", true).await;
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
            "/v1/admin/media",
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
async fn a_non_staff_session_cannot_reach_the_back_office(db: PgPool) {
    let app = TestApp::new(db).await;
    create_user(&app, "client@mouzoun.com", "abc123", false).await;
    // A guest account cannot even open a back-office session...
    let (status, cookie, _) = login(&app, "client@mouzoun.com", "abc123").await;
    assert_eq!((status, cookie), (StatusCode::UNAUTHORIZED, None));
    // ...and a forged non-staff token is refused by the guard.
    let token =
        mouzoun_api::auth::jwt::issue(crate::common::JWT_SECRET, uuid::Uuid::now_v7(), false, 5)
            .unwrap();
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/admin/media",
            None,
            &[("authorization", format!("Bearer {token}"))],
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrations = "./migrations")]
async fn state_changing_requests_from_foreign_origins_are_rejected(db: PgPool) {
    let app = TestApp::new(db).await;
    create_user(&app, "boss@mouzoun.com", "abc123", true).await;
    let (_, cookie, _) = login(&app, "boss@mouzoun.com", "abc123").await;
    let cookie = cookie.unwrap();
    let upload = json!({ "filename": "dune.png", "content_type": "image/png", "size_bytes": 1024 });

    let evil = [
        ("cookie", cookie.clone()),
        ("origin", "https://evil.example".to_string()),
    ];
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/admin/media/uploads",
            Some(upload.clone()),
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
        .send(Method::POST, "/v1/admin/media/uploads", Some(upload), &ours)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/admin/media",
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
    create_user(&app, "boss@mouzoun.com", "abc123", true).await;
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

#[sqlx::test(migrations = "./migrations")]
async fn invited_and_suspended_users_cannot_log_in(db: PgPool) {
    let app = TestApp::new(db).await;
    let id = create_user(&app, "new@mouzoun.com", "abc123", true).await;
    for status in ["invited", "suspended"] {
        sqlx::query("UPDATE users SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(status)
            .execute(&app.db)
            .await
            .unwrap();
        let (code, cookie, _) = login(&app, "new@mouzoun.com", "abc123").await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{status}");
        assert!(cookie.is_none());
    }
}
