//! URLs placed in emails. Booking pages are reached with a signed token (`t`): the reference
//! alone is guessable, the token is not (HMAC-SHA256 of the booking id).

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use uuid::Uuid;

pub fn booking_token(secret: &str, booking_id: Uuid) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    let mut mac = ring::hmac::Context::with_key(&key);
    mac.update(b"mouzoun:booking-link:");
    mac.update(booking_id.as_bytes());
    URL_SAFE_NO_PAD.encode(&mac.sign().as_ref()[..16])
}

pub fn booking_url(base: &str, locale: &str, reference: &str, token: &str) -> String {
    format!("{base}/{locale}/booking/{reference}?t={token}")
}

/// "Find my booking" link: the booking page exchanges `m` for its access token.
pub fn magic_url(base: &str, locale: &str, reference: &str, token: &str) -> String {
    format!("{base}/{locale}/booking/{reference}?m={}", encode(token))
}

pub fn review_url(base: &str, locale: &str, reference: &str, token: &str) -> String {
    format!("{base}/{locale}/review/{reference}?t={token}")
}

pub fn admin_url(base: &str, path: &str) -> String {
    format!("{base}/admin/{path}")
}

/// Account links carry the one-time token the user received.
pub fn token_url(base: &str, path: &str, token: &str) -> String {
    format!("{base}/{path}?token={}", encode(token))
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booking_tokens_are_stable_and_secret_dependent() {
        let id = Uuid::from_u128(42);
        assert_eq!(booking_token("a", id), booking_token("a", id));
        assert_ne!(booking_token("a", id), booking_token("b", id));
        assert_eq!(booking_token("a", id).len(), 22);
        assert_eq!(
            token_url("https://x", "fr/r", "a+b/c"),
            "https://x/fr/r?token=a%2Bb%2Fc"
        );
    }
}
