//! QR tickets: a signed token naming a booking, shown offline in the guest's account and scanned
//! at the camp. It proves the booking, nothing else (no personal data inside): the scanner asks
//! the API for the details.
//!
//! Format: `MZT1.<booking id>.<expiry unix>.<HMAC-SHA256 b64url>`; valid until the day after
//! departure.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::tokens::{constant_time_eq, mac};

const LABEL: &str = "ticket";

pub fn issue(secret: &str, booking_id: Uuid, expires: DateTime<Utc>) -> String {
    let exp = expires.timestamp().to_string();
    let sig = mac(secret, LABEL, &[booking_id.as_bytes(), exp.as_bytes()]);
    format!(
        "MZT1.{}.{exp}.{}",
        booking_id.simple(),
        URL_SAFE_NO_PAD.encode(sig)
    )
}

/// The booking of a valid, unexpired ticket.
pub fn verify(secret: &str, token: &str, now: DateTime<Utc>) -> Option<Uuid> {
    let mut parts = token.trim().split('.');
    if parts.next()? != "MZT1" {
        return None;
    }
    let booking_id = Uuid::parse_str(parts.next()?).ok()?;
    let exp = parts.next()?;
    let sig = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
    if parts.next().is_some() {
        return None;
    }
    let expected = mac(secret, LABEL, &[booking_id.as_bytes(), exp.as_bytes()]);
    if !constant_time_eq(&sig, &expected) || exp.parse::<i64>().ok()? < now.timestamp() {
        return None;
    }
    Some(booking_id)
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    #[test]
    fn tickets_are_signed_and_expire() {
        let now = Utc::now();
        let id = Uuid::now_v7();
        let t = issue("s", id, now + Duration::days(1));
        assert_eq!(verify("s", &t, now), Some(id));
        assert_eq!(verify("other", &t, now), None);
        assert_eq!(verify("s", &t, now + Duration::days(2)), None);
        let forged = t.replace(
            &id.simple().to_string(),
            &Uuid::now_v7().simple().to_string(),
        );
        assert_eq!(verify("s", &forged, now), None);
    }
}
