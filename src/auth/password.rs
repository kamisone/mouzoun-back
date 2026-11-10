use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};

use crate::error::{AppError, AppResult};

/// Argon2id hashing is CPU-heavy: run it on the blocking pool so it never stalls the async runtime.
pub async fn hash(password: String) -> AppResult<String> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|h| h.to_string())
            .map_err(|e| AppError::Internal(anyhow::anyhow!("hash error: {e}")))
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?
}

pub async fn verify(password: String, hash: String) -> AppResult<bool> {
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&hash)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid hash: {e}")))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    })
    .await
    .map_err(|e| AppError::Internal(e.into()))?
}
