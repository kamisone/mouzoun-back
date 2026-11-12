//! Random tokens, hashes and short codes. Raw tokens are only ever sent to the user (cookie,
//! email); the database and Redis keep their SHA-256.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::{Rng, RngCore};
use sha2::{Digest, Sha256};

/// 256-bit random token, URL-safe.
pub fn random_token() -> String {
    let mut raw = [0u8; 32];
    rand::rng().fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// Six digits, uniformly drawn ("042917").
pub fn six_digit_code() -> String {
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

pub fn sha256(value: &str) -> Vec<u8> {
    Sha256::digest(value.as_bytes()).to_vec()
}

pub fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// HMAC-SHA256 of `parts` under `secret` and a domain label (never reused across purposes).
pub fn mac(secret: &str, label: &str, parts: &[&[u8]]) -> Vec<u8> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let mut m = ring::hmac::Context::with_key(&key);
    m.update(b"mouzoun:");
    m.update(label.as_bytes());
    m.update(b":");
    for p in parts {
        m.update(p);
        m.update(b"\0");
    }
    m.sign().as_ref().to_vec()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `m***@gmail.com`: shown on the code screen ("We sent a code to …").
pub fn mask_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            format!("{first}***@{domain}")
        }
        None => "***".into(),
    }
}

/// Canonical form used for lookups and rate-limit keys.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(six_digit_code().len(), 6);
        assert_ne!(random_token(), random_token());
        assert_eq!(mask_email("amina@gmail.com"), "a***@gmail.com");
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert_ne!(mac("k", "a", &[b"x"]), mac("k", "b", &[b"x"]));
    }
}
