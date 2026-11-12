//! Guest accounts against the real router, Postgres and Redis.

mod common;

use axum::http::{HeaderMap, Method, StatusCode};
use common::{TestApp, create_user, fixtures::*};
use mouzoun_api::services::accounts::totp;
use serde_json::{Value, json};
use sqlx::PgPool;

/// `name=value` pairs of every Set-Cookie (empty values = removal).
fn cookies(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn cookie_header(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn last_code(db: &PgPool) -> (String, String) {
    let payload: Value = sqlx::query_scalar(
        "SELECT payload FROM outbox_events WHERE event_type = 'auth.login_code' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(db)
    .await
    .unwrap();
    (
        payload["secret"]["code"].as_str().unwrap().to_string(),
        payload["secret"]["token"].as_str().unwrap().to_string(),
    )
}

async fn identify(app: &TestApp, email: &str) -> (StatusCode, Value) {
    app.request(
        Method::POST,
        "/v1/account/auth/identify",
        Some(json!({ "email": email })),
        None,
    )
    .await
}

/// Signs in by email code; returns the session cookies.
async fn sign_in(
    app: &TestApp,
    email: &str,
    extra_cookie: Option<&str>,
) -> (Value, Vec<(String, String)>) {
    let (status, _) = identify(app, email).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (code, _) = last_code(&app.db).await;
    let headers: Vec<(&str, String)> = extra_cookie
        .map(|c| vec![("cookie", c.to_string())])
        .unwrap_or_default();
    let (status, h, body) = app
        .send(
            Method::POST,
            "/v1/account/auth/code",
            Some(json!({ "email": email, "code": code })),
            &headers,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (body, cookies(&h))
}

#[sqlx::test(migrations = "./migrations")]
async fn identify_never_reveals_accounts_and_codes_are_single_use_with_five_tries(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    create_user(&db, "known@example.com", "x", false, None).await;
    let (s1, known) = identify(&app, "known@example.com").await;
    let (s2, unknown) = identify(&app, "nobody@example.com").await;
    assert_eq!((s1, s2), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    assert_eq!(
        known.as_object().unwrap().keys().collect::<Vec<_>>(),
        unknown.as_object().unwrap().keys().collect::<Vec<_>>()
    );
    assert_eq!(unknown["masked_email"], "n***@example.com");
    let users: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'nobody@example.com'")
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(users, 0, "no account before the address is verified");

    // Five wrong codes burn the code; the right one no longer works.
    let (code, _) = last_code(&db).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for left in (1..=4).rev() {
        let (status, body) = app
            .request(
                Method::POST,
                "/v1/account/auth/code",
                Some(json!({ "email": "nobody@example.com", "code": wrong })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["details"]["attempts_left"], left);
    }
    let (status, body) = app
        .request(
            Method::POST,
            "/v1/account/auth/code",
            Some(json!({ "email": "nobody@example.com", "code": wrong })),
            None,
        )
        .await;
    assert_eq!(
        (status, body["error"]["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("too_many_attempts"))
    );
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/account/auth/code",
            Some(json!({ "email": "nobody@example.com", "code": code })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::GONE);

    // A fresh code signs in once (single use), creating the account.
    let (body, jar) = sign_in(&app, "nobody@example.com", None).await;
    assert_eq!(body["created"], true);
    assert_eq!(body["needs_profile"], true);
    assert!(
        jar.iter().any(|(k, _)| k == "__Host-mz_at")
            && jar.iter().any(|(k, _)| k == "__Host-mz_rt")
    );
    let (code, _) = last_code(&db).await;
    let (status, _) = app
        .request(
            Method::POST,
            "/v1/account/auth/code",
            Some(json!({ "email": "nobody@example.com", "code": code })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::GONE, "single use");
}

#[sqlx::test(migrations = "./migrations")]
async fn guest_bookings_join_the_account_only_through_a_verified_email(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 3).await;
    for email in ["amina@example.com", "other@example.com"] {
        let mut req_meta = meta(email);
        req_meta.rate_plan_id = None;
        mouzoun_api::services::booking::book_accommodation(
            &db,
            &mouzoun_api::services::booking::AccommodationRequest {
                product_id: tent,
                arrival: d(2027, 1, 10),
                departure: d(2027, 1, 12),
                quantity: 1,
                guests: adults(&guests, 2),
                meta: req_meta,
            },
            now(),
            today(),
        )
        .await
        .unwrap();
    }
    let (body, jar) = sign_in(&app, "Amina@Example.com", None).await;
    assert_eq!(body["needs_profile"], false, "names come from the booking");
    assert_eq!(body["account"]["first_name"], "Amina");
    let cookie = cookie_header(&jar);
    let (_, _, trips) = app
        .send(
            Method::GET,
            "/v1/account/trips?tab=upcoming",
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    let (_, _, past) = app
        .send(
            Method::GET,
            "/v1/account/trips?tab=past",
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    let refs: Vec<&str> = trips["trips"]
        .as_array()
        .unwrap()
        .iter()
        .chain(past["trips"].as_array().unwrap())
        .filter_map(|t| t["reference"].as_str())
        .collect();
    assert_eq!(
        refs.len(),
        1,
        "only the booking of the verified address: {refs:?}"
    );

    // The other guest's booking: 404 (not 403) even when signed in.
    let other: String = sqlx::query_scalar(
        "SELECT b.reference FROM bookings b JOIN customers c ON c.id = b.customer_id WHERE c.email = 'other@example.com'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    let (status, _, _) = app
        .send(
            Method::GET,
            &format!("/v1/bookings/{other}"),
            None,
            &[("cookie", cookie.clone())],
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, view) = app
        .send(
            Method::GET,
            &format!("/v1/bookings/{}", refs[0]),
            None,
            &[("cookie", cookie)],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "own booking without the email token"
    );
    assert_eq!(view["reference"], refs[0]);
}

#[sqlx::test(migrations = "./migrations")]
async fn refresh_tokens_rotate_and_a_reused_one_revokes_the_session(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (_, jar) = sign_in(&app, "rotate@example.com", None).await;
    let rt = jar
        .iter()
        .find(|(k, _)| k == "__Host-mz_rt")
        .unwrap()
        .1
        .clone();
    let old = format!("__Host-mz_rt={rt}");
    let (status, h, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/refresh",
            None,
            &[("cookie", old.clone())],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let new = cookies(&h);
    let new_rt = new
        .iter()
        .find(|(k, _)| k == "__Host-mz_rt")
        .unwrap()
        .1
        .clone();
    assert_ne!(new_rt, rt);

    // The old token again, after the grace window: theft → the whole session is revoked.
    sqlx::query("UPDATE user_sessions SET last_used_at = now() - interval '1 minute'")
        .execute(&db)
        .await
        .unwrap();
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/refresh",
            None,
            &[("cookie", old)],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/refresh",
            None,
            &[("cookie", format!("__Host-mz_rt={new_rt}"))],
        )
        .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the new token died with the session"
    );
    let reason: String = sqlx::query_scalar("SELECT revoked_reason FROM user_sessions")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(reason, "refresh_token_reuse");
}

#[sqlx::test(migrations = "./migrations")]
async fn sign_out_everywhere_revokes_every_session_at_once(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (_, phone) = sign_in(&app, "many@example.com", None).await;
    let (_, laptop) = sign_in(&app, "many@example.com", None).await;
    let (status, _, me) = app
        .send(
            Method::GET,
            "/v1/account/me",
            None,
            &[("cookie", cookie_header(&laptop))],
        )
        .await;
    assert_eq!(
        (status, me["email"].as_str()),
        (StatusCode::OK, Some("many@example.com"))
    );
    let (status, _, body) = app
        .send(
            Method::POST,
            "/v1/account/auth/logout-all",
            None,
            &[("cookie", cookie_header(&phone))],
        )
        .await;
    assert_eq!(
        (status, body["revoked"].as_i64()),
        (StatusCode::OK, Some(2))
    );
    // The laptop's access token is still within its 15 minutes, yet refused immediately.
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/account/me",
            None,
            &[("cookie", cookie_header(&laptop))],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_expired_access_token_is_refreshed_transparently(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (_, jar) = sign_in(&app, "silent@example.com", None).await;
    let rt = jar
        .iter()
        .find(|(k, _)| k == "__Host-mz_rt")
        .unwrap()
        .1
        .clone();
    // Only the refresh cookie (access cookie expired and dropped by the browser).
    let (status, h, me) = app
        .send(
            Method::GET,
            "/v1/account/me",
            None,
            &[("cookie", format!("__Host-mz_rt={rt}"))],
        )
        .await;
    assert_eq!(
        (status, me["email"].as_str()),
        (StatusCode::OK, Some("silent@example.com"))
    );
    assert!(
        cookies(&h)
            .iter()
            .any(|(k, v)| k == "__Host-mz_at" && !v.is_empty()),
        "new cookies sent"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn the_basket_follows_the_guest_when_signing_in(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let guests = guest_ids(&db).await;
    let tent = accommodation(&db, "pool", 2).await;
    let item = json!({ "kind": "stay", "product_id": tent, "arrival": "2027-02-10", "departure": "2027-02-12",
                       "guests": [{ "guest_category_id": guests.adult, "count": 2 }] });
    let (_, h, _) = app
        .send(Method::POST, "/v1/cart/items", Some(item.clone()), &[])
        .await;
    let anon = cookie_header(&cookies(&h));
    // Signed in once before with the same stay already in the account's basket.
    let (_, first) = sign_in(&app, "basket@example.com", None).await;
    app.send(
        Method::POST,
        "/v1/cart/items",
        Some(item),
        &[("cookie", cookie_header(&first))],
    )
    .await;
    // Signing in on this device merges the anonymous basket, without a duplicate.
    let (_, jar) = sign_in(&app, "basket@example.com", Some(&anon)).await;
    let (_, _, cart) = app
        .send(
            Method::GET,
            "/v1/cart",
            None,
            &[("cookie", cookie_header(&jar))],
        )
        .await;
    assert_eq!(cart["count"], 1);
    assert_eq!(cart["quote"]["total_minor"], 6_000);
}

#[sqlx::test(migrations = "./migrations")]
async fn guests_never_reach_the_back_office_and_staff_need_a_second_factor(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let (_, jar) = sign_in(&app, "guest@example.com", None).await;
    let (status, _, _) = app
        .send(
            Method::GET,
            "/v1/admin/bookings",
            None,
            &[("cookie", cookie_header(&jar))],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let hash = mouzoun_api::auth::password::hash("Staff-password-1".into())
        .await
        .unwrap();
    create_user(&db, "staff@example.com", &hash, true, Some("reception")).await;
    let (status, h, body) = app
        .send(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": "staff@example.com", "password": "Staff-password-1" })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["mfa_enrollment_required"], true);
    assert!(
        cookies(&h).is_empty(),
        "no session before the second factor"
    );
    let secret = totp::base32_decode(body["secret"].as_str().unwrap()).unwrap();
    let wrong = app
        .send(
            Method::POST,
            "/v1/auth/mfa",
            Some(json!({ "mfa_token": body["mfa_token"], "code": "000000" })),
            &[],
        )
        .await;
    assert_eq!(wrong.0, StatusCode::UNPROCESSABLE_ENTITY);
    let code = (0..1_000_000)
        .map(|n| format!("{n:06}"))
        .find(|c| totp::verify(&secret, c, chrono::Utc::now().timestamp()).is_some())
        .unwrap();
    let (status, h, _) = app
        .send(
            Method::POST,
            "/v1/auth/mfa",
            Some(json!({ "mfa_token": body["mfa_token"], "code": code })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        cookies(&h)
            .iter()
            .any(|(k, _)| k == "__Host-mouzoun_session")
    );
    // A guest password cannot open the back office either.
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/auth/login",
            Some(json!({ "email": "guest@example.com", "password": "anything-long" })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
