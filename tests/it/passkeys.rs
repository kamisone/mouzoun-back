//! Passkeys against a software authenticator (P-256 key made with ring).

use crate::common::{ALLOWED_ORIGIN, TestApp};
use axum::http::{HeaderMap, Method, StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

struct Authenticator {
    key: EcdsaKeyPair,
    credential_id: Vec<u8>,
    count: u32,
}

impl Authenticator {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        Self {
            key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                .unwrap(),
            credential_id: uuid::Uuid::now_v7().as_bytes().to_vec(),
            count: 0,
        }
    }

    fn auth_data(&mut self, attested: bool) -> Vec<u8> {
        self.count += 1;
        let mut d = Sha256::digest(b"mouzoun.test").to_vec();
        d.push(if attested { 0x45 } else { 0x05 });
        d.extend_from_slice(&self.count.to_be_bytes());
        if attested {
            d.extend_from_slice(&[0; 16]);
            d.extend_from_slice(&(self.credential_id.len() as u16).to_be_bytes());
            d.extend_from_slice(&self.credential_id);
            d.extend_from_slice(&[0xa0]); // COSE key: not read by the server
        }
        d
    }

    fn client_data(kind: &str, challenge: &str) -> Vec<u8> {
        serde_json::to_vec(
            &json!({ "type": kind, "challenge": challenge, "origin": ALLOWED_ORIGIN }),
        )
        .unwrap()
    }

    fn create(&mut self, challenge: &str) -> Value {
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(self.key.public_key().as_ref());
        json!({
            "id": URL_SAFE_NO_PAD.encode(&self.credential_id),
            "response": {
                "clientDataJSON": URL_SAFE_NO_PAD.encode(Self::client_data("webauthn.create", challenge)),
                "authenticatorData": URL_SAFE_NO_PAD.encode(self.auth_data(true)),
                "publicKey": URL_SAFE_NO_PAD.encode(spki),
                "publicKeyAlgorithm": -7,
                "transports": ["internal"]
            }
        })
    }

    fn get(&mut self, challenge: &str) -> Value {
        let client = Self::client_data("webauthn.get", challenge);
        let auth = self.auth_data(false);
        let mut signed = auth.clone();
        signed.extend_from_slice(&Sha256::digest(&client));
        let sig = self.key.sign(&SystemRandom::new(), &signed).unwrap();
        json!({
            "id": URL_SAFE_NO_PAD.encode(&self.credential_id),
            "response": {
                "clientDataJSON": URL_SAFE_NO_PAD.encode(client),
                "authenticatorData": URL_SAFE_NO_PAD.encode(auth),
                "signature": URL_SAFE_NO_PAD.encode(sig.as_ref())
            }
        })
    }
}

fn cookies(headers: &HeaderMap) -> String {
    headers
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok()?.split(';').next().map(str::to_string))
        .filter(|kv| !kv.ends_with('='))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn sign_in_by_code(app: &TestApp, email: &str) -> String {
    app.request(
        Method::POST,
        "/v1/account/auth/identify",
        Some(json!({ "email": email })),
        None,
    )
    .await;
    let code: String = sqlx::query_scalar(
        "SELECT payload->'secret'->>'code' FROM outbox_events WHERE event_type = 'auth.login_code' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    let (_, h, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/code",
            Some(json!({ "email": email, "code": code })),
            &[],
        )
        .await;
    cookies(&h)
}

#[sqlx::test(migrations = "./migrations")]
async fn a_passkey_signs_in_and_a_cloned_or_replayed_one_does_not(db: PgPool) {
    let app = TestApp::new(db.clone()).await;
    let cookie = sign_in_by_code(&app, "amina@example.com").await;
    let me = vec![("cookie", cookie)];
    let mut device = Authenticator::new();

    let (status, _, options) = app
        .send(Method::POST, "/v1/account/passkeys/options", None, &me)
        .await;
    assert_eq!(status, StatusCode::OK, "{options}");
    assert_eq!(options["rp"]["id"], "mouzoun.test");
    let credential = device.create(options["challenge"].as_str().unwrap());
    let (status, _, added) = app
        .send(
            Method::POST,
            "/v1/account/passkeys",
            Some(json!({ "name": "Phone", "credential": credential })),
            &me,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    // The same challenge cannot register twice.
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/account/passkeys",
            Some(json!({ "credential": credential })),
            &me,
        )
        .await;
    assert_eq!(status, StatusCode::GONE);

    // Username-less sign-in.
    let (_, options) = app
        .request(Method::POST, "/v1/account/auth/passkey/options", None, None)
        .await;
    let challenge = options["challenge"].as_str().unwrap().to_string();
    let assertion = device.get(&challenge);
    let (status, h, body) = app
        .send(
            Method::POST,
            "/v1/account/auth/passkey",
            Some(json!({ "credential": assertion })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"]["email"], "amina@example.com");
    assert!(cookies(&h).contains("mz_at"));

    // Replay of the same assertion: the challenge is spent.
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/passkey",
            Some(json!({ "credential": assertion })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::GONE);

    // A clone with an older counter is refused.
    let (_, options) = app
        .request(Method::POST, "/v1/account/auth/passkey/options", None, None)
        .await;
    device.count = 0;
    let stale = device.get(options["challenge"].as_str().unwrap());
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/passkey",
            Some(json!({ "credential": stale })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A forged signature (other key, same credential id) is refused.
    let mut forger = Authenticator::new();
    forger.credential_id = device.credential_id.clone();
    forger.count = 50;
    let (_, options) = app
        .request(Method::POST, "/v1/account/auth/passkey/options", None, None)
        .await;
    let forged = forger.get(options["challenge"].as_str().unwrap());
    let (status, _, _) = app
        .send(
            Method::POST,
            "/v1/account/auth/passkey",
            Some(json!({ "credential": forged })),
            &[],
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let added_notice: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM outbox_events WHERE event_type = 'auth.passkey_added'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(added_notice, 1);
}
