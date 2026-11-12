//! Passkeys (WebAuthn level 2+), without attestation (`"none"`): what matters is that the key
//! signs our challenge, not who made the authenticator.
//!
//! The browser sends the JSON serialisation of the credential (`PublicKeyCredential.toJSON()`),
//! which carries the public key as SubjectPublicKeyInfo DER: no CBOR parsing is needed. Only
//! ES256 (P-256) keys are requested, supported by every platform authenticator.
//!
//! Checks: client data type, challenge (single use, 5 minutes, in Redis), origin; authenticator
//! data RP id hash and "user present" flag; signature over `authenticatorData ‖ SHA-256(clientDataJSON)`;
//! signature counter going backwards (a cloned key) refuses the sign-in.

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use redis::AsyncCommands;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::tokens::{random_token, sha256, sha256_hex};
use crate::{
    error::{AppError, AppResult},
    infra::cache::Cache,
};

const CHALLENGE_SECS: u64 = 300;
pub const ES256: i64 = -7;
const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_BACKED_UP: u8 = 0x10;
const FLAG_ATTESTED: u8 = 0x40;

fn invalid() -> AppError {
    AppError::domain(
        StatusCode::UNAUTHORIZED,
        "passkey_invalid",
        "this passkey could not be verified",
    )
}

fn b64(raw: &str) -> AppResult<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(raw.trim_end_matches('='))
        .map_err(|_| invalid())
}

fn challenge_key(cache: &Cache, purpose: &str, challenge: &str) -> String {
    cache.key(&format!("webauthn:{purpose}:{}", sha256_hex(challenge)))
}

/// A new challenge bound to a purpose (`reg:<user>` or `auth`), single use.
pub async fn new_challenge(cache: &Cache, purpose: &str) -> AppResult<String> {
    let challenge = random_token();
    let mut conn = cache.connection();
    let _: () = conn
        .set_ex(
            challenge_key(cache, purpose, &challenge),
            "1",
            CHALLENGE_SECS,
        )
        .await?;
    Ok(challenge)
}

async fn take_challenge(cache: &Cache, purpose: &str, challenge: &str) -> AppResult<()> {
    let mut conn = cache.connection();
    let found: Option<String> = redis::cmd("GETDEL")
        .arg(challenge_key(cache, purpose, challenge))
        .query_async(&mut conn)
        .await?;
    found.map(|_| ()).ok_or_else(|| {
        AppError::domain(
            StatusCode::GONE,
            "passkey_expired",
            "this request expired: try again",
        )
    })
}

#[derive(Debug, Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
}

/// Relying party settings.
#[derive(Debug, Clone)]
pub struct Rp {
    pub id: String,
    pub origin: String,
}

fn check_client_data(rp: &Rp, raw: &[u8], kind: &str) -> AppResult<String> {
    let c: ClientData = serde_json::from_slice(raw).map_err(|_| invalid())?;
    if c.kind != kind || c.origin != rp.origin {
        return Err(invalid());
    }
    Ok(c.challenge)
}

struct AuthData<'a> {
    flags: u8,
    sign_count: u32,
    /// Credential id of attested credential data (registration only).
    credential_id: Option<&'a [u8]>,
}

fn parse_auth_data<'a>(rp: &Rp, data: &'a [u8]) -> AppResult<AuthData<'a>> {
    if data.len() < 37 || data[..32] != sha256(&rp.id)[..] {
        return Err(invalid());
    }
    let flags = data[32];
    if flags & FLAG_USER_PRESENT == 0 {
        return Err(invalid());
    }
    let sign_count = u32::from_be_bytes([data[33], data[34], data[35], data[36]]);
    let credential_id = if flags & FLAG_ATTESTED != 0 {
        // aaguid (16) then credential id length (2) then credential id.
        let len_at = 37 + 16;
        let len = data
            .get(len_at..len_at + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
            .ok_or_else(invalid)?;
        Some(data.get(len_at + 2..len_at + 2 + len).ok_or_else(invalid)?)
    } else {
        None
    };
    Ok(AuthData {
        flags,
        sign_count,
        credential_id,
    })
}

/// The uncompressed P-256 point of a SubjectPublicKeyInfo (its last 65 bytes, `04 ‖ x ‖ y`).
fn p256_point(spki: &[u8]) -> AppResult<&[u8]> {
    // id-ecPublicKey + prime256v1 SPKI is always 91 bytes.
    if spki.len() != 91 || spki[26] != 0x04 {
        return Err(invalid());
    }
    Ok(&spki[26..])
}

/// Registration response (`PublicKeyCredential.toJSON()` with attestation "none").
#[derive(Debug, Deserialize)]
pub struct RegistrationResponse {
    pub id: String,
    pub response: RegistrationInner,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationInner {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    pub authenticator_data: String,
    pub public_key: String,
    pub public_key_algorithm: i64,
    #[serde(default)]
    pub transports: Vec<String>,
}

pub struct NewCredential {
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub sign_count: u32,
    pub backed_up: bool,
    pub transports: Vec<String>,
}

pub async fn verify_registration(
    cache: &Cache,
    rp: &Rp,
    user_id: Uuid,
    r: &RegistrationResponse,
) -> AppResult<NewCredential> {
    let client_data = b64(&r.response.client_data_json)?;
    let challenge = check_client_data(rp, &client_data, "webauthn.create")?;
    take_challenge(cache, &format!("reg:{user_id}"), &challenge).await?;
    let auth = b64(&r.response.authenticator_data)?;
    let parsed = parse_auth_data(rp, &auth)?;
    let credential_id = b64(&r.id)?;
    if parsed.credential_id != Some(&credential_id[..]) || r.response.public_key_algorithm != ES256
    {
        return Err(invalid());
    }
    let public_key = b64(&r.response.public_key)?;
    p256_point(&public_key)?;
    Ok(NewCredential {
        credential_id,
        public_key,
        sign_count: parsed.sign_count,
        backed_up: parsed.flags & FLAG_BACKED_UP != 0,
        transports: r
            .response
            .transports
            .iter()
            .take(6)
            .map(|t| t.chars().take(20).collect())
            .collect(),
    })
}

/// Sign-in response (`PublicKeyCredential.toJSON()` of `navigator.credentials.get`).
#[derive(Debug, Deserialize)]
pub struct AssertionResponse {
    pub id: String,
    pub response: AssertionInner,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssertionInner {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    pub authenticator_data: String,
    pub signature: String,
}

impl AssertionResponse {
    pub fn credential_id(&self) -> AppResult<Vec<u8>> {
        b64(&self.id)
    }
}

/// Verifies an assertion against a stored key; returns the new signature counter.
pub async fn verify_assertion(
    cache: &Cache,
    rp: &Rp,
    purpose: &str,
    a: &AssertionResponse,
    public_key: &[u8],
    stored_count: i64,
) -> AppResult<(u32, bool)> {
    let client_data = b64(&a.response.client_data_json)?;
    let challenge = check_client_data(rp, &client_data, "webauthn.get")?;
    take_challenge(cache, purpose, &challenge).await?;
    let auth = b64(&a.response.authenticator_data)?;
    let parsed = parse_auth_data(rp, &auth)?;
    let mut signed = auth.clone();
    signed.extend_from_slice(ring::digest::digest(&ring::digest::SHA256, &client_data).as_ref());
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, p256_point(public_key)?)
        .verify(&signed, &b64(&a.response.signature)?)
        .map_err(|_| invalid())?;
    if parsed.sign_count != 0 && i64::from(parsed.sign_count) <= stored_count {
        tracing::warn!(
            alert = true,
            "passkey signature counter went backwards: possible cloned key"
        );
        return Err(invalid());
    }
    Ok((parsed.sign_count, parsed.flags & FLAG_BACKED_UP != 0))
}

/// `PublicKeyCredentialCreationOptions` (JSON form, for `PublicKeyCredential.parseCreationOptionsFromJSON`).
pub fn creation_options(
    rp: &Rp,
    site: &str,
    user_id: Uuid,
    email: &str,
    name: &str,
    challenge: &str,
    exclude: &[Vec<u8>],
) -> Value {
    json!({
        "challenge": challenge,
        "rp": { "id": rp.id, "name": site },
        "user": { "id": URL_SAFE_NO_PAD.encode(user_id.as_bytes()), "name": email, "displayName": if name.is_empty() { email } else { name } },
        "pubKeyCredParams": [{ "type": "public-key", "alg": ES256 }],
        "timeout": CHALLENGE_SECS * 1000,
        "attestation": "none",
        "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "preferred" },
        "excludeCredentials": exclude.iter().map(|id| json!({ "type": "public-key", "id": URL_SAFE_NO_PAD.encode(id) })).collect::<Vec<_>>(),
    })
}

/// `PublicKeyCredentialRequestOptions` for a discoverable (username-less) sign-in.
pub fn request_options(rp: &Rp, challenge: &str, allow: &[Vec<u8>]) -> Value {
    json!({
        "challenge": challenge,
        "rpId": rp.id,
        "timeout": CHALLENGE_SECS * 1000,
        "userVerification": "preferred",
        "allowCredentials": allow.iter().map(|id| json!({ "type": "public-key", "id": URL_SAFE_NO_PAD.encode(id) })).collect::<Vec<_>>(),
    })
}
