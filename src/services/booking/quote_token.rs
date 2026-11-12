//! Signed quotes: the total the guest saw, bound to the exact request and an expiry. Checkout
//! recomputes the price and compares it with this total: the browser never sends a price the
//! server trusts, and a changed price is answered with `price_changed` instead of charging a
//! different amount silently.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::trip::TripRequest;

/// How long a quote can be paid without a fresh one.
pub const VALIDITY_MINUTES: i64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedQuote {
    /// SHA-256 of the canonical request.
    pub h: String,
    pub total_minor: i64,
    pub currency: String,
    pub exp: i64,
}

fn request_hash(request: &TripRequest) -> String {
    let canonical = serde_json::to_vec(request).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(Sha256::digest(&canonical))
}

fn mac(secret: &str, payload: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(b"mouzoun:quote:");
    mac.update(payload.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub fn sign(
    secret: &str,
    request: &TripRequest,
    total_minor: i64,
    currency: &str,
    now: DateTime<Utc>,
) -> String {
    let quote = SignedQuote {
        h: request_hash(request),
        total_minor,
        currency: currency.to_string(),
        exp: (now + Duration::minutes(VALIDITY_MINUTES)).timestamp(),
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&quote).unwrap_or_default());
    format!(
        "{payload}.{}",
        URL_SAFE_NO_PAD.encode(mac(secret, &payload))
    )
}

/// The total of a valid, unexpired token issued for exactly `request`.
pub fn verify(secret: &str, token: &str, request: &TripRequest, now: DateTime<Utc>) -> Option<i64> {
    let (payload, signature) = token.split_once('.')?;
    let expected = mac(secret, payload);
    let given = URL_SAFE_NO_PAD.decode(signature).ok()?;
    if given.len() != expected.len()
        || given
            .iter()
            .zip(&expected)
            .fold(0u8, |a, (x, y)| a | (x ^ y))
            != 0
    {
        return None;
    }
    let quote: SignedQuote = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (quote.exp > now.timestamp() && quote.h == request_hash(request)).then_some(quote.total_minor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_bind_the_request_the_total_and_expire() {
        let now = Utc::now();
        let request = TripRequest::default();
        let token = sign("s", &request, 12_300, "EUR", now);
        assert_eq!(verify("s", &token, &request, now), Some(12_300));
        assert_eq!(verify("other", &token, &request, now), None);
        let changed = TripRequest {
            promo_code: Some("X".into()),
            ..TripRequest::default()
        };
        assert_eq!(verify("s", &token, &changed, now), None);
        assert_eq!(
            verify("s", &token, &request, now + Duration::minutes(61)),
            None
        );
        let (payload, sig) = token.split_once('.').unwrap();
        let mut tampered: SignedQuote =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        tampered.total_minor = 1;
        let forged = format!(
            "{}.{sig}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&tampered).unwrap())
        );
        assert_eq!(verify("s", &forged, &request, now), None);
    }
}
