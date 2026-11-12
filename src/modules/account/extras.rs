//! Wishlists, saved travellers and "Add a booking to my account".

use std::time::Duration as StdDuration;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::GuestUser,
    db::bookings::IdDocumentType,
    error::{AppError, AppResult},
    modules::booking::catalog,
    services::{
        accounts::{
            sessions, signin,
            tokens::{normalize_email, random_token, sha256},
        },
        audit::{self, Actor},
        booking::register,
        outbox::{self, Event},
    },
    state::AppState,
    web::client::{ClientInfo, RequestLocale},
};

// ---- Wishlists --------------------------------------------------------------------------------

async fn default_list(state: &AppState, user_id: Uuid) -> AppResult<Uuid> {
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM wishlists WHERE user_id = $1 AND is_default",
        user_id
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query_scalar!(
        "INSERT INTO wishlists (user_id, name, is_default) VALUES ($1, 'Saved', true)
         ON CONFLICT (user_id) WHERE is_default DO UPDATE SET name = wishlists.name RETURNING id",
        user_id
    )
    .fetch_one(&state.db)
    .await?)
}

async fn owned(state: &AppState, user_id: Uuid, list: Uuid) -> AppResult<()> {
    let ok = sqlx::query_scalar!(
        "SELECT user_id = $2 AS \"ok!\" FROM wishlists WHERE id = $1",
        list,
        user_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if ok { Ok(()) } else { Err(AppError::NotFound) }
}

async fn list_view(
    state: &AppState,
    locale: &str,
    lists: Vec<(Uuid, String, bool, Option<String>)>,
) -> AppResult<Vec<Value>> {
    let mut out = Vec::new();
    for (id, name, is_default, share) in lists {
        let items = sqlx::query!(
            "SELECT product_id, note, created_at FROM wishlist_items WHERE wishlist_id = $1 ORDER BY created_at DESC",
            id
        )
        .fetch_all(&state.db)
        .await?;
        let ids: Vec<Uuid> = items.iter().map(|i| i.product_id).collect();
        let cards = if ids.is_empty() {
            vec![]
        } else {
            catalog::cards(state, locale, None, false, Some(&ids)).await?
        };
        let products: Vec<Value> = items
            .iter()
            .filter_map(|i| {
                cards
                    .iter()
                    .find(|c| c.id == i.product_id)
                    .map(|c| json!({ "product": c, "note": i.note, "added_at": i.created_at }))
            })
            .collect();
        out.push(json!({ "id": id, "name": name, "is_default": is_default, "share_token": share, "items": products }));
    }
    Ok(out)
}

pub async fn wishlists(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    guest: GuestUser,
) -> AppResult<Json<Value>> {
    default_list(&state, guest.user_id).await?;
    let lists = sqlx::query!(
        "SELECT id, name, is_default, share_token FROM wishlists WHERE user_id = $1 ORDER BY is_default DESC, created_at",
        guest.user_id
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|l| (l.id, l.name, l.is_default, l.share_token))
    .collect();
    Ok(Json(
        json!({ "lists": list_view(&state, locale.as_str(), lists).await? }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct NameBody {
    pub name: String,
}

fn valid_name(name: &str) -> AppResult<String> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > 80 {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
            "name: 1 to 80 characters",
        ));
    }
    Ok(n.to_string())
}

pub async fn create_wishlist(
    State(state): State<AppState>,
    guest: GuestUser,
    Json(body): Json<NameBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let count = sqlx::query_scalar!(
        "SELECT count(*) AS \"n!\" FROM wishlists WHERE user_id = $1",
        guest.user_id
    )
    .fetch_one(&state.db)
    .await?;
    if count >= 30 {
        return Err(AppError::BadRequest("too many lists".into()));
    }
    let id = sqlx::query_scalar!(
        "INSERT INTO wishlists (user_id, name) VALUES ($1, $2) RETURNING id",
        guest.user_id,
        valid_name(&body.name)?
    )
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn rename_wishlist(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
    Json(body): Json<NameBody>,
) -> AppResult<StatusCode> {
    owned(&state, guest.user_id, id).await?;
    sqlx::query!(
        "UPDATE wishlists SET name = $2 WHERE id = $1",
        id,
        valid_name(&body.name)?
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_wishlist(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    owned(&state, guest.user_id, id).await?;
    let deleted = sqlx::query!("DELETE FROM wishlists WHERE id = $1 AND NOT is_default", id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "default_list",
            "the default list cannot be deleted",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, Default)]
pub struct ItemBody {
    #[serde(default)]
    pub note: Option<String>,
    /// Move from this list.
    #[serde(default)]
    pub from: Option<Uuid>,
}

pub async fn put_item(
    State(state): State<AppState>,
    guest: GuestUser,
    Path((id, product)): Path<(Uuid, Uuid)>,
    body: Option<Json<ItemBody>>,
) -> AppResult<StatusCode> {
    owned(&state, guest.user_id, id).await?;
    let body = body.map(|b| b.0).unwrap_or_default();
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO wishlist_items (wishlist_id, product_id, note) SELECT $1, $2, $3
         WHERE EXISTS (SELECT 1 FROM products WHERE id = $2 AND status = 'published' AND deleted_at IS NULL)
         ON CONFLICT (wishlist_id, product_id) DO UPDATE SET note = COALESCE(EXCLUDED.note, wishlist_items.note)",
        id,
        product,
        body.note.as_deref().map(|n| n.chars().take(500).collect::<String>())
    )
    .execute(&mut *tx)
    .await?;
    if let Some(from) = body.from.filter(|f| *f != id) {
        sqlx::query!(
            "DELETE FROM wishlist_items WHERE wishlist_id = $1 AND product_id = $2
             AND wishlist_id IN (SELECT id FROM wishlists WHERE user_id = $3)",
            from,
            product,
            guest.user_id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_item(
    State(state): State<AppState>,
    guest: GuestUser,
    Path((id, product)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    owned(&state, guest.user_id, id).await?;
    sqlx::query!(
        "DELETE FROM wishlist_items WHERE wishlist_id = $1 AND product_id = $2",
        id,
        product
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn share(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    owned(&state, guest.user_id, id).await?;
    let token = sqlx::query_scalar!(
        "UPDATE wishlists SET share_token = COALESCE(share_token, $2) WHERE id = $1 RETURNING share_token AS \"t!\"",
        id,
        random_token()
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({ "share_token": token })))
}

pub async fn unshare(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    owned(&state, guest.user_id, id).await?;
    sqlx::query!("UPDATE wishlists SET share_token = NULL WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Read-only shared list (no personal data: list name and products only).
pub async fn shared(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    Path(token): Path<String>,
) -> AppResult<Json<Value>> {
    let l = sqlx::query!(
        "SELECT id, name, is_default FROM wishlists WHERE share_token = $1",
        token
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let mut lists = list_view(
        &state,
        locale.as_str(),
        vec![(l.id, l.name, l.is_default, None)],
    )
    .await?;
    let mut list = lists.remove(0);
    list["id"] = Value::Null;
    Ok(Json(list))
}

/// Product ids saved in any list (hearts on cards).
pub async fn saved_ids(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let ids = sqlx::query_scalar!(
        "SELECT DISTINCT i.product_id FROM wishlist_items i JOIN wishlists w ON w.id = i.wishlist_id WHERE w.user_id = $1",
        guest.user_id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "product_ids": ids })))
}

/// Heart: saves in the default list.
pub async fn save(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(product): Path<Uuid>,
) -> AppResult<StatusCode> {
    let list = default_list(&state, guest.user_id).await?;
    put_item(State(state), guest, Path((list, product)), None).await
}

/// Heart off: removes from every list.
pub async fn unsave(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(product): Path<Uuid>,
) -> AppResult<StatusCode> {
    sqlx::query!(
        "DELETE FROM wishlist_items WHERE product_id = $1 AND wishlist_id IN (SELECT id FROM wishlists WHERE user_id = $2)",
        product,
        guest.user_id
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct MergeBody {
    pub product_ids: Vec<Uuid>,
}

/// Hearts saved on this device before signing in join the default list.
pub async fn merge_saved(
    State(state): State<AppState>,
    guest: GuestUser,
    Json(body): Json<MergeBody>,
) -> AppResult<Json<Value>> {
    let list = default_list(&state, guest.user_id).await?;
    let ids: Vec<Uuid> = body.product_ids.into_iter().take(200).collect();
    sqlx::query!(
        "INSERT INTO wishlist_items (wishlist_id, product_id)
         SELECT $1, p.id FROM products p WHERE p.id = ANY($2) AND p.status = 'published' AND p.deleted_at IS NULL
         ON CONFLICT DO NOTHING",
        list,
        &ids
    )
    .execute(&state.db)
    .await?;
    saved_ids(State(state), guest).await
}

// ---- Saved travellers --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TravellerBody {
    pub first_name: String,
    pub last_name: String,
    #[serde(default)]
    pub guest_category_id: Option<Uuid>,
    #[serde(default)]
    pub date_of_birth: Option<NaiveDate>,
    #[serde(default)]
    pub nationality: Option<String>,
    #[serde(default)]
    pub id_document_type: Option<IdDocumentType>,
    /// Write-only; requires `id_consent = true`.
    #[serde(default)]
    pub id_document_number: Option<String>,
    #[serde(default)]
    pub id_consent: bool,
}

fn traveller_json(t: &TravellerRow) -> Value {
    json!({
        "id": t.id, "first_name": t.first_name, "last_name": t.last_name, "guest_category_id": t.guest_category_id,
        "date_of_birth": t.date_of_birth, "nationality": t.nationality, "id_document_type": t.id_document_type,
        "has_id_document": t.id_document_number_enc.is_some(), "id_consent_at": t.id_consent_at,
    })
}

struct TravellerRow {
    id: Uuid,
    first_name: String,
    last_name: String,
    guest_category_id: Option<Uuid>,
    date_of_birth: Option<NaiveDate>,
    nationality: Option<String>,
    id_document_type: Option<String>,
    id_document_number_enc: Option<Vec<u8>>,
    id_consent_at: Option<chrono::DateTime<Utc>>,
}

pub async fn travellers(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let rows = sqlx::query_as!(
        TravellerRow,
        r#"SELECT id, first_name, last_name, guest_category_id, date_of_birth, nationality::text AS "nationality?",
                  id_document_type, id_document_number_enc, id_consent_at
           FROM saved_travellers WHERE user_id = $1 ORDER BY created_at"#,
        guest.user_id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "travellers": rows.iter().map(traveller_json).collect::<Vec<_>>(), "id_numbers_enabled": state.config.booking.guest_data_key.is_some() }),
    ))
}

fn validate_traveller(b: &TravellerBody) -> AppResult<()> {
    if b.first_name.trim().is_empty()
        || b.last_name.trim().is_empty()
        || b.first_name.len() > 80
        || b.last_name.len() > 80
    {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
            "names required",
        ));
    }
    if b.nationality.as_deref().is_some_and(|n| n.len() != 2) {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_field",
            "nationality: 2 letters",
        ));
    }
    if b.id_document_number
        .as_deref()
        .is_some_and(|n| !n.trim().is_empty())
        && !b.id_consent
    {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "consent_required",
            "keeping an ID number needs your consent",
        ));
    }
    Ok(())
}

fn encrypt_number(state: &AppState, b: &TravellerBody) -> AppResult<Option<Vec<u8>>> {
    match b
        .id_document_number
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        None => Ok(None),
        Some(n) => {
            let key = state
                .config
                .booking
                .guest_data_key
                .as_ref()
                .ok_or_else(|| {
                    AppError::domain(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "id_numbers_disabled",
                        "ID numbers cannot be stored",
                    )
                })?;
            Ok(Some(register::encrypt(key, n)))
        }
    }
}

pub async fn create_traveller(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(b): Json<TravellerBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    validate_traveller(&b)?;
    let enc = encrypt_number(&state, &b)?;
    let id = sqlx::query_scalar!(
        "INSERT INTO saved_travellers (user_id, first_name, last_name, guest_category_id, date_of_birth, nationality,
                                       id_document_type, id_document_number_enc, id_consent_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, CASE WHEN $9 THEN now() END) RETURNING id",
        guest.user_id,
        b.first_name.trim(),
        b.last_name.trim(),
        b.guest_category_id,
        b.date_of_birth,
        b.nationality.as_deref().map(str::to_ascii_uppercase),
        b.id_document_type.map(IdDocumentType::as_str),
        enc,
        b.id_consent && enc.is_some()
    )
    .fetch_one(&state.db)
    .await?;
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "account.traveller_saved",
        "user",
        &guest.user_id.to_string(),
        json!({ "traveller_id": id, "id_document": enc.is_some() }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn update_traveller(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
    Json(b): Json<TravellerBody>,
) -> AppResult<StatusCode> {
    validate_traveller(&b)?;
    let enc = encrypt_number(&state, &b)?;
    let updated = sqlx::query!(
        "UPDATE saved_travellers SET first_name = $3, last_name = $4, guest_category_id = $5, date_of_birth = $6,
                nationality = $7, id_document_type = $8,
                id_document_number_enc = CASE WHEN NOT $10 THEN NULL ELSE COALESCE($9, id_document_number_enc) END,
                id_consent_at = CASE WHEN $10 THEN COALESCE(id_consent_at, now()) END
         WHERE id = $1 AND user_id = $2",
        id,
        guest.user_id,
        b.first_name.trim(),
        b.last_name.trim(),
        b.guest_category_id,
        b.date_of_birth,
        b.nationality.as_deref().map(str::to_ascii_uppercase),
        b.id_document_type.map(IdDocumentType::as_str),
        enc,
        b.id_consent
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_traveller(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let deleted = sqlx::query!(
        "DELETE FROM saved_travellers WHERE id = $1 AND user_id = $2",
        id,
        guest.user_id
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Reveals a saved ID number (fresh authentication required, audited).
pub async fn reveal_document(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    sessions::require_recent_auth(&state.db, guest.session_id, Utc::now()).await?;
    let enc = sqlx::query_scalar!(
        "SELECT id_document_number_enc FROM saved_travellers WHERE id = $1 AND user_id = $2",
        id,
        guest.user_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let number = match (enc, state.config.booking.guest_data_key.as_ref()) {
        (Some(data), Some(key)) => register::decrypt(key, &data),
        _ => None,
    };
    let mut conn = state.db.acquire().await?;
    audit::record(
        &mut conn,
        &client.actor(Actor::customer()),
        "account.document_revealed",
        "user",
        &guest.user_id.to_string(),
        json!({ "traveller_id": id }),
    )
    .await?;
    Ok(Json(json!({ "id_document_number": number })))
}

// ---- Booking claim ----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ClaimBody {
    pub reference: String,
    pub email: String,
}

/// "Add a booking to my account": a link goes to the booking's email address; nothing is
/// attached until it is used. Same answer whether or not the booking exists.
pub async fn claim(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<ClaimBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    crate::web::limits::rate_limit(
        &state.cache,
        "acct_claim",
        &guest.user_id.to_string(),
        10,
        StdDuration::from_secs(3600),
    )
    .await?;
    crate::web::limits::rate_limit(
        &state.cache,
        "acct_claim_ip",
        &client.key(),
        20,
        StdDuration::from_secs(3600),
    )
    .await?;
    let booking = sqlx::query!(
        "SELECT b.id, c.user_id FROM bookings b JOIN customers c ON c.id = b.customer_id
         WHERE b.reference = $1 AND c.email = $2::text::citext",
        body.reference.trim().to_ascii_uppercase(),
        normalize_email(&body.email)
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(b) = booking.filter(|b| b.user_id != Some(guest.user_id)) {
        let token = random_token();
        let mut tx = state.db.begin().await?;
        sqlx::query!(
            "INSERT INTO auth_tokens (user_id, purpose, token_hash, expires_at, data) VALUES ($1, 'booking_claim', $2, $3, $4)",
            guest.user_id,
            sha256(&token),
            Utc::now() + Duration::hours(24),
            json!({ "booking_id": b.id })
        )
        .execute(&mut *tx)
        .await?;
        outbox::enqueue(
            &mut tx,
            &Event::BookingClaimRequested {
                booking_id: b.id,
                user_id: guest.user_id,
                token,
            },
        )
        .await?;
        tx.commit().await?;
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "sent": true }))))
}

#[derive(Debug, Deserialize)]
pub struct TokenBody {
    pub token: String,
}

/// Uses the claim link: the booking moves to the account's customer record (history kept).
pub async fn confirm_claim(
    State(state): State<AppState>,
    client: ClientInfo,
    guest: GuestUser,
    Json(body): Json<TokenBody>,
) -> AppResult<Json<Value>> {
    let now = Utc::now();
    let mut tx = state.db.begin().await?;
    let token = sqlx::query!(
        "UPDATE auth_tokens SET used_at = $3 WHERE token_hash = $1 AND purpose = 'booking_claim' AND user_id = $2
           AND used_at IS NULL AND expires_at > $3
         RETURNING data",
        sha256(&body.token),
        guest.user_id,
        now
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| AppError::domain(StatusCode::GONE, "link_expired", "this link expired or was already used"))?;
    let booking_id: Uuid = token.data["booking_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(AppError::NotFound)?;
    let email = sqlx::query_scalar!(
        r#"SELECT email::text AS "e!" FROM users WHERE id = $1"#,
        guest.user_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let mine = signin::link_customer(
        &mut tx,
        guest.user_id,
        &email,
        &crate::db::types::Locale::new("fr"),
    )
    .await?;
    let before = sqlx::query!(
        "UPDATE bookings b SET customer_id = $2 FROM bookings old WHERE b.id = $1 AND old.id = $1 RETURNING old.customer_id, b.reference",
        booking_id,
        mine
    )
    .fetch_one(&mut *tx)
    .await?;
    audit::record(&mut tx, &client.actor(Actor::customer()), "booking.claimed", "booking", &booking_id.to_string(),
        json!({ "before": { "customer_id": before.customer_id }, "after": { "customer_id": mine } })).await?;
    outbox::enqueue(
        &mut tx,
        &Event::BookingClaimed {
            booking_id,
            user_id: guest.user_id,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "reference": before.reference })))
}

// ---- QR ticket --------------------------------------------------------------------------------

/// The ticket of a confirmed trip, to keep offline (no personal data in the QR code).
pub async fn ticket(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(reference): Path<String>,
) -> AppResult<Json<Value>> {
    let b = sqlx::query!(
        r#"SELECT b.id, b.reference, b.status, b.arrival_date, b.departure_date, b.adults, b.children, b.infants,
                  c.first_name, c.last_name,
                  COALESCE((SELECT jsonb_agg(jsonb_build_object('name', i.product_snapshot->>'name', 'kind', i.product_kind,
                              'starts_at', i.starts_at, 'quantity', i.quantity) ORDER BY i.starts_at NULLS FIRST)
                            FROM booking_items i WHERE i.booking_id = b.id AND i.status = 'active' AND i.parent_item_id IS NULL),
                           '[]') AS "items!"
           FROM bookings b JOIN customers c ON c.id = b.customer_id
           WHERE b.reference = $1 AND c.user_id = $2"#,
        reference.to_ascii_uppercase(),
        guest.user_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if !["confirmed", "checked_in"].contains(&b.status.as_str()) {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "no_ticket",
            "tickets are available once the booking is confirmed",
        ));
    }
    let last_day = b
        .departure_date
        .or(b.arrival_date)
        .unwrap_or_else(|| crate::camp_time::today(Utc::now()));
    let expires = (last_day + Duration::days(1))
        .and_hms_opt(23, 59, 0)
        .map_or_else(Utc::now, |d| d.and_utc());
    Ok(Json(json!({
        "token": crate::services::accounts::tickets::issue(&state.config.jwt_secret, b.id, expires),
        "expires_at": expires,
        "reference": b.reference, "status": b.status,
        "arrival_date": b.arrival_date, "departure_date": b.departure_date,
        "guests": { "adults": b.adults, "children": b.children, "infants": b.infants },
        "lead": format!("{} {}", b.first_name, b.last_name.unwrap_or_default()).trim(),
        "items": b.items,
    })))
}
