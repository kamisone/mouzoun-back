//! Guest register for the Moroccan police form ("fiche de police"), filled online before arrival
//! or by staff. ID document numbers are encrypted by the application (AES-256-GCM, key
//! `GUEST_DATA_KEY`), shown only with `guests.read_identity`, and purged after the legal
//! retention period.

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng},
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::{BookingError, state};
use crate::{
    db::bookings::{BookingStatus, IdDocumentType},
    services::audit::{self, Actor},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestEntry {
    pub first_name: String,
    pub last_name: String,
    #[serde(default)]
    pub nationality: Option<String>,
    #[serde(default)]
    pub date_of_birth: Option<NaiveDate>,
    #[serde(default)]
    pub id_document_type: Option<IdDocumentType>,
    /// Write-only: never returned in clear except to `guests.read_identity`.
    #[serde(default)]
    pub id_document_number: Option<String>,
    #[serde(default)]
    pub guest_category_id: Option<Uuid>,
    #[serde(default)]
    pub is_lead: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuestView {
    pub id: Uuid,
    pub first_name: String,
    pub last_name: String,
    pub nationality: Option<String>,
    pub date_of_birth: Option<NaiveDate>,
    pub id_document_type: Option<IdDocumentType>,
    /// `•••• 1234`, or the full number when revealed.
    pub id_document_number: Option<String>,
    pub has_id_document: bool,
    pub guest_category_id: Option<Uuid>,
    pub is_lead: bool,
}

/// Encrypts with a fresh random nonce: `nonce (12 bytes) ‖ ciphertext ‖ tag`.
pub fn encrypt(key: &[u8; 32], plain: &str) -> Result<Vec<u8>, EncryptionError> {
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let mut out = nonce.to_vec();
    out.extend(
        cipher
            .encrypt(&nonce, plain.as_bytes())
            .map_err(|_| EncryptionError)?,
    );
    Ok(out)
}

#[derive(Debug, thiserror::Error)]
#[error("encryption failed")]
pub struct EncryptionError;

impl From<EncryptionError> for crate::error::AppError {
    fn from(e: EncryptionError) -> Self {
        crate::error::AppError::Internal(e.into())
    }
}

pub fn decrypt(key: &[u8; 32], data: &[u8]) -> Option<String> {
    if data.len() < 13 {
        return None;
    }
    let (nonce, cipher_text) = data.split_at(12);
    let cipher = Aes256Gcm::new(key.into());
    let plain = cipher.decrypt(Nonce::from_slice(nonce), cipher_text).ok()?;
    String::from_utf8(plain).ok()
}

fn mask(number: &str) -> String {
    let tail: String = number
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("•••• {tail}")
}

/// Replaces the register of a booking. Numbers left empty keep the stored ones (the form never
/// receives them back in clear).
pub async fn save(
    pool: &PgPool,
    key: Option<&[u8; 32]>,
    booking_id: Uuid,
    entries: &[GuestEntry],
    actor: &Actor,
    now: DateTime<Utc>,
) -> Result<bool, BookingError> {
    if entries.is_empty() || entries.len() > 50 {
        return Err(BookingError::Invalid("one to fifty guests".into()));
    }
    if entries
        .iter()
        .any(|e| e.first_name.trim().is_empty() || e.last_name.trim().is_empty())
    {
        return Err(BookingError::Invalid(
            "first and last names are required".into(),
        ));
    }
    if entries.iter().filter(|e| e.is_lead).count() > 1 {
        return Err(BookingError::Invalid("only one lead guest".into()));
    }
    if entries.iter().any(|e| {
        e.nationality
            .as_deref()
            .is_some_and(|n| n.len() != 2 || !n.chars().all(|c| c.is_ascii_alphabetic()))
    }) {
        return Err(BookingError::Invalid(
            "nationality is a 2-letter country code".into(),
        ));
    }
    let wants_numbers = entries.iter().any(|e| {
        e.id_document_number
            .as_deref()
            .is_some_and(|n| !n.trim().is_empty())
    });
    if wants_numbers && key.is_none() {
        return Err(BookingError::Invalid(
            "ID numbers cannot be stored: GUEST_DATA_KEY is not configured".into(),
        ));
    }

    let mut tx = pool.begin().await?;
    let status = state::lock(&mut tx, booking_id).await?;
    if !matches!(
        status,
        BookingStatus::Pending
            | BookingStatus::AwaitingPayment
            | BookingStatus::Confirmed
            | BookingStatus::CheckedIn
    ) {
        return Err(BookingError::InvalidTransition(status));
    }
    let previous = sqlx::query!(
        "SELECT first_name, last_name, id_document_number_enc FROM booking_guests WHERE booking_id = $1",
        booking_id
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM booking_guests WHERE booking_id = $1",
        booking_id
    )
    .execute(&mut *tx)
    .await?;
    for entry in entries {
        let number = entry
            .id_document_number
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let encrypted = match (number, key) {
            (Some(n), Some(k)) => {
                Some(encrypt(k, n).map_err(|e| BookingError::Invalid(e.to_string()))?)
            }
            // Keep the stored number of the same person when the form left it blank.
            _ => previous
                .iter()
                .find(|p| {
                    p.first_name.eq_ignore_ascii_case(entry.first_name.trim())
                        && p.last_name.eq_ignore_ascii_case(entry.last_name.trim())
                })
                .and_then(|p| p.id_document_number_enc.clone()),
        };
        sqlx::query!(
            "INSERT INTO booking_guests (booking_id, guest_category_id, is_lead, first_name, last_name, nationality,
                                         date_of_birth, id_document_type, id_document_number_enc)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            booking_id,
            entry.guest_category_id,
            entry.is_lead,
            entry.first_name.trim(),
            entry.last_name.trim(),
            entry.nationality.as_deref().map(str::to_ascii_uppercase),
            entry.date_of_birth,
            entry.id_document_type.map(IdDocumentType::as_str),
            encrypted
        )
        .execute(&mut *tx)
        .await?;
    }
    // Complete when every guest counted in the booking is registered with nationality and date
    // of birth, and every adult with an ID document.
    let complete = sqlx::query_scalar!(
        r#"SELECT (SELECT count(*) FROM booking_guests WHERE booking_id = $1) >= (b.adults + b.children)
              AND NOT EXISTS (SELECT 1 FROM booking_guests g WHERE g.booking_id = $1
                              AND (g.nationality IS NULL OR g.date_of_birth IS NULL
                                   OR (g.date_of_birth <= (CURRENT_DATE - interval '18 years')
                                       AND (g.id_document_type IS NULL OR g.id_document_number_enc IS NULL))))
              AS "complete!"
           FROM bookings b WHERE b.id = $1"#,
        booking_id
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE bookings SET precheckin_completed_at = CASE WHEN $2 THEN COALESCE(precheckin_completed_at, $3) END
         WHERE id = $1",
        booking_id,
        complete,
        now
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        actor,
        "booking.register_updated",
        "booking",
        &booking_id.to_string(),
        // Never the numbers themselves.
        json!({ "guests": entries.len(), "complete": complete }),
    )
    .await?;
    tx.commit().await?;
    Ok(complete)
}

pub async fn load(
    pool: &PgPool,
    key: Option<&[u8; 32]>,
    booking_id: Uuid,
    reveal: bool,
) -> Result<Vec<GuestView>, BookingError> {
    let rows = sqlx::query!(
        r#"SELECT id, first_name, last_name, nationality, date_of_birth,
                  id_document_type AS "id_document_type: IdDocumentType", id_document_number_enc,
                  guest_category_id, is_lead
           FROM booking_guests WHERE booking_id = $1 ORDER BY is_lead DESC, created_at, id"#,
        booking_id
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let clear = match (&r.id_document_number_enc, key) {
                (Some(data), Some(k)) => decrypt(k, data),
                _ => None,
            };
            GuestView {
                id: r.id,
                first_name: r.first_name,
                last_name: r.last_name,
                nationality: r.nationality,
                date_of_birth: r.date_of_birth,
                id_document_type: r.id_document_type,
                has_id_document: r.id_document_number_enc.is_some(),
                id_document_number: clear.map(|n| if reveal { n } else { mask(&n) }),
                guest_category_id: r.guest_category_id,
                is_lead: r.is_lead,
            }
        })
        .collect())
}

/// Erases ID numbers of stays that ended more than `retention_days` ago. Returns the count.
pub async fn purge_identity_numbers(
    pool: &PgPool,
    retention_days: i64,
    today: NaiveDate,
) -> sqlx::Result<u64> {
    let cutoff = today - Duration::days(retention_days);
    Ok(sqlx::query!(
        "UPDATE booking_guests g SET id_document_number_enc = NULL
         FROM bookings b
         WHERE b.id = g.booking_id AND g.id_document_number_enc IS NOT NULL
           AND COALESCE(b.departure_date, b.arrival_date) < $1",
        cutoff
    )
    .execute(pool)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip_and_are_masked() {
        let key = [3u8; 32];
        let a = encrypt(&key, "AB123456").unwrap();
        let b = encrypt(&key, "AB123456").unwrap();
        assert_ne!(a, b, "random nonce");
        assert_eq!(decrypt(&key, &a).as_deref(), Some("AB123456"));
        assert_eq!(decrypt(&[4u8; 32], &a), None, "wrong key");
        assert_eq!(mask("AB123456"), "•••• 3456");
    }
}
