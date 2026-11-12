//! "Continue with Google / Apple": ID token checks and account linking rules.

use chrono::Utc;
use jsonwebtoken::{Algorithm, EncodingKey, Header, jwk::JwkSet};
use mouzoun_api::{
    config::AppleClient,
    db::{identity::IdentityProvider, types::Locale},
    services::{
        accounts::oidc::{self, VerifiedIdentity},
        audit::Actor,
    },
};
use serde_json::json;
use sqlx::PgPool;

/// Throwaway P-256 key generated for these tests only (never used anywhere else).
const TEST_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgcHvKxiuLBSl1y1J5
jtApgzYNyoXE50Tob2RN8mJYdN6hRANCAATSACXo7jNmlPUblYk2/LkOZx3m5WUl
GBC+2lfr3M1B5v+uhT0XF/X9RwtXVLAQNvSiiGaGb29iLkmfAuBSuzZN
-----END PRIVATE KEY-----";

fn keys() -> JwkSet {
    serde_json::from_value(json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig",
        "x": "0gAl6O4zZpT1G5WJNvy5Dmcd5uVlJRgQvtpX69zNQeY",
        "y": "_66FPRcX9f1HC1dUsBA29KKIZoZvb2IuSZ8C4FK7Nk0"
    }]}))
    .unwrap()
}

fn id_token(claims: serde_json::Value) -> String {
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some("k1".into());
    jsonwebtoken::encode(
        &header,
        &claims,
        &EncodingKey::from_ec_pem(TEST_KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}

fn claims(nonce: &str, aud: &str) -> serde_json::Value {
    let now = Utc::now().timestamp();
    json!({
        "iss": "https://accounts.google.com", "aud": aud, "sub": "g-123", "nonce": nonce,
        "iat": now, "exp": now + 600, "email": "Amina@Example.com", "email_verified": true,
        "given_name": "Amina"
    })
}

#[test]
fn id_tokens_are_checked_for_signature_audience_and_nonce() {
    let verify = |token: &str, nonce: &str| {
        oidc::verify_id_token(
            IdentityProvider::Google,
            token,
            &keys(),
            "https://accounts.google.com",
            "client-1",
            nonce,
        )
    };
    let ok = verify(&id_token(claims("n1", "client-1")), "n1").unwrap();
    assert_eq!(ok.subject, "g-123");
    assert_eq!(ok.email.as_deref(), Some("amina@example.com"));
    assert!(ok.email_verified);
    assert!(
        verify(&id_token(claims("n1", "client-1")), "other").is_err(),
        "nonce"
    );
    assert!(
        verify(&id_token(claims("n1", "client-2")), "n1").is_err(),
        "audience"
    );
    let mut expired = claims("n1", "client-1");
    expired["exp"] = json!(Utc::now().timestamp() - 3600);
    assert!(verify(&id_token(expired), "n1").is_err(), "expiry");
    let mut tampered = id_token(claims("n1", "client-1"));
    tampered.push('x');
    assert!(verify(&tampered, "n1").is_err(), "signature");
}

#[test]
fn apple_client_secret_is_an_es256_jwt() {
    let secret = oidc::apple_client_secret(
        &AppleClient {
            client_id: "com.mouzoun.web".into(),
            team_id: "TEAM".into(),
            key_id: "KEY".into(),
            private_key: TEST_KEY.into(),
        },
        Utc::now(),
    )
    .unwrap();
    let header = jsonwebtoken::decode_header(&secret).unwrap();
    assert_eq!(header.alg, Algorithm::ES256);
    assert_eq!(header.kid.as_deref(), Some("KEY"));
}

#[test]
fn pkce_challenge_matches_rfc_7636() {
    assert_eq!(
        oidc::pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

fn identity(subject: &str, email: &str, verified: bool) -> VerifiedIdentity {
    VerifiedIdentity {
        provider: IdentityProvider::Google,
        subject: subject.into(),
        email: Some(email.into()),
        email_verified: verified,
        given_name: Some("Amina".into()),
        family_name: Some("B".into()),
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn provider_identities_link_only_through_verified_emails(db: PgPool) {
    let now = Utc::now();
    let fr = Locale::new("fr");
    let actor = Actor::customer();
    // Unverified address: refused, nothing created.
    let err = oidc::resolve(
        &db,
        &identity("s1", "amina@example.com", false),
        None,
        &fr,
        &actor,
        now,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("email_unverified"), "{err}");
    let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE NOT is_staff")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(users, 0);

    // Verified: account created, names from the provider, identity stored.
    let first = oidc::resolve(
        &db,
        &identity("s1", "amina@example.com", true),
        None,
        &fr,
        &actor,
        now,
    )
    .await
    .unwrap();
    assert!(first.created);
    assert!(!first.needs_profile);
    // Same subject again: same user.
    let again = oidc::resolve(
        &db,
        &identity("s1", "changed@example.com", false),
        None,
        &fr,
        &actor,
        now,
    )
    .await
    .unwrap();
    assert_eq!(again.user_id, first.user_id);
    assert!(!again.created);

    // Another subject, same verified email: linked to the same account, with a security email.
    let apple = VerifiedIdentity {
        provider: IdentityProvider::Apple,
        ..identity("a1", "amina@example.com", true)
    };
    let linked = oidc::resolve(&db, &apple, None, &fr, &actor, now)
        .await
        .unwrap();
    assert_eq!(linked.user_id, first.user_id);
    let notices: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE event_type = 'auth.identity_linked'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(notices, 1);

    // An identity of one account cannot be linked to another.
    let other = oidc::resolve(
        &db,
        &identity("s2", "other@example.com", true),
        None,
        &fr,
        &actor,
        now,
    )
    .await
    .unwrap();
    let err = oidc::resolve(
        &db,
        &identity("s1", "amina@example.com", true),
        Some(other.user_id),
        &fr,
        &actor,
        now,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("identity_in_use"), "{err}");
}
