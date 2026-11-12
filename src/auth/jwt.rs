use chrono::{Duration, Utc};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};

/// Session claims. Fine-grained permissions are read from the database (roles can change during
/// a session); the token only carries what the back-office guard needs.
#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    /// `users.is_staff`: may enter the back-office.
    pub staff: bool,
    pub iat: i64,
    pub exp: i64,
    /// Guest sessions: the `user_sessions` row (checked on every request, so a sign-out of all
    /// devices is immediate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<Uuid>,
}

pub fn issue(secret: &str, user_id: Uuid, staff: bool, ttl_minutes: i64) -> AppResult<String> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id,
        staff,
        iat: now.timestamp(),
        exp: (now + Duration::minutes(ttl_minutes)).timestamp(),
        sid: None,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(e.into()))
}

/// Short-lived access token of a guest session (never staff).
pub fn issue_guest(secret: &str, user_id: Uuid, sid: Uuid, ttl_minutes: i64) -> AppResult<String> {
    let now = Utc::now();
    let claims = Claims {
        sub: user_id,
        staff: false,
        iat: now.timestamp(),
        exp: (now + Duration::minutes(ttl_minutes)).timestamp(),
        sid: Some(sid),
    };
    // A different key derivation from staff tokens: a guest token can never pass as a staff one.
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(guest_key(secret).as_slice()),
    )
    .map_err(|e| AppError::Internal(e.into()))
}

pub fn verify_guest(secret: &str, token: &str) -> AppResult<Claims> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(guest_key(secret).as_slice()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .ok()
    .filter(|c| !c.staff && c.sid.is_some())
    .ok_or(AppError::Unauthorized)
}

fn guest_key(secret: &str) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    Sha256::digest(format!("mouzoun:guest-session:{secret}").as_bytes()).to_vec()
}

pub fn verify(secret: &str, token: &str) -> AppResult<Claims> {
    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map(|data| data.claims)
    .map_err(|_| AppError::Unauthorized)
}
