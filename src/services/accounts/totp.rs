//! TOTP (RFC 6238: HMAC-SHA1, 30-second steps, 6 digits) for staff two-factor sign-in. The
//! secret is stored encrypted (AES-256-GCM, key derived from JWT_SECRET) in `users.totp_secret_enc`.

use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;
use sha2::{Digest, Sha256};

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn new_secret() -> Vec<u8> {
    let mut raw = vec![0u8; 20];
    rand::rng().fill_bytes(&mut raw);
    raw
}

pub fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for b in bytes {
        buffer = (buffer << 8) | u32::from(*b);
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.trim_end_matches('=').bytes() {
        let v = ALPHABET.iter().position(|a| *a == c.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buffer >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

fn hotp(secret: &[u8], counter: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let offset = (h[19] & 0x0f) as usize;
    let bin = (u32::from(h[offset]) & 0x7f) << 24
        | u32::from(h[offset + 1]) << 16
        | u32::from(h[offset + 2]) << 8
        | u32::from(h[offset + 3]);
    bin % 1_000_000
}

/// The step the code matches (current, previous or next 30 s window), if any.
pub fn verify(secret: &[u8], code: &str, unix_seconds: i64) -> Option<i64> {
    let code: u32 = code.trim().replace(' ', "").parse().ok()?;
    let step = unix_seconds.div_euclid(30);
    (step - 1..=step + 1).find(|s| *s >= 0 && hotp(secret, *s as u64) == code)
}

pub fn otpauth_uri(secret: &[u8], account: &str, issuer: &str) -> String {
    let enc = |s: &str| {
        s.replace('%', "%25")
            .replace(' ', "%20")
            .replace(':', "%3A")
            .replace('@', "%40")
    };
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        enc(issuer),
        enc(account),
        base32(secret),
        enc(issuer)
    )
}

/// Encryption key of TOTP secrets.
pub fn key(jwt_secret: &str) -> [u8; 32] {
    Sha256::digest(format!("mouzoun:totp-key:{jwt_secret}").as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6238_vectors() {
        // RFC 6238 appendix B, SHA-1 secret "12345678901234567890", last 6 digits.
        let secret = b"12345678901234567890";
        assert_eq!(hotp(secret, 59 / 30), 287_082);
        assert_eq!(hotp(secret, 1_111_111_109 / 30), 81_804);
        assert_eq!(verify(secret, "287082", 59), Some(1));
        assert_eq!(
            verify(secret, "287082", 59 + 30),
            Some(1),
            "previous window accepted"
        );
        assert_eq!(verify(secret, "287082", 59 + 90), None);
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
    }
}
