//! The signed-in customer's shop pages: orders (next to their stays) and saved addresses.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::GuestUser,
    error::{AppError, AppResult},
    services::shop::cart::{Address, field_errors},
    state::AppState,
};

async fn customer_of(state: &AppState, guest: &GuestUser) -> AppResult<Uuid> {
    sqlx::query_scalar!(
        "SELECT id FROM customers WHERE user_id = $1 AND deleted_at IS NULL",
        guest.user_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn orders(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let Ok(customer) = customer_of(&state, &guest).await else {
        return Ok(Json(json!({ "data": [] })));
    };
    let rows = sqlx::query!(
        r#"SELECT o.number, o.status, o.payment_status, o.fulfillment_status, o.total_minor, o.currency::text AS "currency!", o.placed_at,
                  (SELECT count(*) FROM order_items WHERE order_id = o.id) AS "lines!",
                  (SELECT product_snapshot->>'image_url' FROM order_items WHERE order_id = o.id ORDER BY sort_order LIMIT 1) AS image_url
           FROM orders o WHERE o.customer_id = $1 AND o.status <> 'draft' ORDER BY COALESCE(o.placed_at, o.created_at) DESC LIMIT 100"#,
        customer
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "data": rows.iter().map(|o| json!({ "number": o.number, "status": o.status, "payment_status": o.payment_status,
        "fulfillment_status": o.fulfillment_status, "total_minor": o.total_minor, "currency": o.currency, "placed_at": o.placed_at,
        "line_count": o.lines, "image_url": o.image_url })).collect::<Vec<_>>() }),
    ))
}

pub async fn addresses(State(state): State<AppState>, guest: GuestUser) -> AppResult<Json<Value>> {
    let Ok(customer) = customer_of(&state, &guest).await else {
        return Ok(Json(json!({ "data": [] })));
    };
    let rows = sqlx::query!(
        r#"SELECT a.id, a.label, a.first_name, a.last_name, a.company, a.line1, a.line2, a.city, a.region, a.postal_code,
                  a.country_code::text AS "country_code!", a.phone, a.delivery_notes,
                  (c.default_shipping_address_id = a.id) AS "is_default?"
           FROM customer_addresses a JOIN customers c ON c.id = a.customer_id
           WHERE a.customer_id = $1 AND a.deleted_at IS NULL ORDER BY a.created_at"#,
        customer
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "data": rows.iter().map(|a| json!({ "id": a.id, "label": a.label, "first_name": a.first_name,
        "last_name": a.last_name, "company": a.company, "line1": a.line1, "line2": a.line2, "city": a.city, "region": a.region,
        "postal_code": a.postal_code, "country_code": a.country_code, "phone": a.phone, "delivery_notes": a.delivery_notes,
        "is_default": a.is_default.unwrap_or(false) })).collect::<Vec<_>>() }),
    ))
}

#[derive(Debug, serde::Deserialize)]
pub struct AddressInput {
    pub label: Option<String>,
    #[serde(flatten)]
    pub address: Address,
    #[serde(default)]
    pub is_default: bool,
}

async fn save(
    state: &AppState,
    guest: &GuestUser,
    id: Option<Uuid>,
    b: &AddressInput,
) -> AppResult<Uuid> {
    let errors = b.address.errors(&[]);
    if !errors.is_empty() {
        return Err(field_errors(&errors));
    }
    let customer = customer_of(state, guest).await?;
    let a = &b.address;
    let id = id.unwrap_or_else(Uuid::now_v7);
    let mut tx = state.db.begin().await?;
    let n = sqlx::query!(
        "INSERT INTO customer_addresses (id, customer_id, label, first_name, last_name, company, line1, line2, city, region, postal_code,
                                         country_code, phone, delivery_notes)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
         ON CONFLICT (id) DO UPDATE SET label = $3, first_name = $4, last_name = $5, company = $6, line1 = $7, line2 = $8, city = $9,
             region = $10, postal_code = $11, country_code = $12, phone = $13, delivery_notes = $14
         WHERE customer_addresses.customer_id = $2 AND customer_addresses.deleted_at IS NULL",
        id,
        customer,
        b.label.as_deref(),
        a.first_name.trim(),
        a.last_name.trim(),
        a.company.as_deref(),
        a.line1.trim(),
        a.line2.as_deref(),
        a.city.trim(),
        a.region.as_deref(),
        a.postal_code.as_deref(),
        a.country_code.trim().to_uppercase(),
        a.phone.trim(),
        a.delivery_notes.as_deref()
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    if b.is_default {
        sqlx::query!(
            "UPDATE customers SET default_shipping_address_id = $2, default_billing_address_id = COALESCE(default_billing_address_id, $2) WHERE id = $1",
            customer,
            id
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn create_address(
    State(state): State<AppState>,
    guest: GuestUser,
    Json(b): Json<AddressInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = save(&state, &guest, None, &b).await?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn update_address(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
    Json(b): Json<AddressInput>,
) -> AppResult<Json<Value>> {
    save(&state, &guest, Some(id), &b).await?;
    Ok(Json(json!({ "id": id })))
}

pub async fn delete_address(
    State(state): State<AppState>,
    guest: GuestUser,
    Path(id): Path<Uuid>,
) -> AppResult<StatusCode> {
    let customer = customer_of(&state, &guest).await?;
    let n = sqlx::query!(
        "UPDATE customer_addresses SET deleted_at = now() WHERE id = $1 AND customer_id = $2 AND deleted_at IS NULL",
        id,
        customer
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
