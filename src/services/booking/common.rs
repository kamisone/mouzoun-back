//! Pieces shared by every booking flow.

use chrono::{DateTime, NaiveDate, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::BookingError;
use crate::{
    db::{
        bookings::{BookingSource, BookingStatus},
        pricing::{Deposit, DepositType, PaymentMode},
        types::{CurrencyCode, DateRange, Locale, percent_of},
    },
    services::pricing::{GuestCount, Quote, engine::PromotionRule},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomerInput {
    pub email: Option<String>,
    pub first_name: String,
    pub last_name: Option<String>,
    pub phone: Option<String>,
    pub country_code: Option<String>,
    pub preferred_locale: Option<Locale>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CustomerRef {
    Existing(Uuid),
    /// Matched by email when an active customer already has it, created otherwise.
    New(CustomerInput),
}

/// Fields common to every booking request.
#[derive(Debug, Clone)]
pub struct BookingRequestMeta {
    pub customer: CustomerRef,
    pub locale: Locale,
    pub source: BookingSource,
    pub rate_plan_id: Option<Uuid>,
    pub promo_code: Option<String>,
    pub special_requests: Option<String>,
    pub created_by: Option<Uuid>,
    pub ip_address: Option<IpNetwork>,
    pub user_agent: Option<String>,
    /// How long a pending booking holds the inventory (`booking.hold_minutes`).
    pub hold: chrono::Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookingCreated {
    pub booking_id: Uuid,
    pub reference: String,
    pub status: BookingStatus,
    pub currency: CurrencyCode,
    pub total_minor: i64,
    pub deposit_due_minor: i64,
    pub hold_expires_at: Option<DateTime<Utc>>,
    pub item_id: Uuid,
    /// Allocated accommodation units / assigned resources.
    pub allocated_ids: Vec<Uuid>,
}

pub(super) async fn resolve_customer(
    conn: &mut PgConnection,
    customer: &CustomerRef,
) -> Result<Uuid, BookingError> {
    match customer {
        CustomerRef::Existing(id) => sqlx::query_scalar!(
            "SELECT id FROM customers WHERE id = $1 AND deleted_at IS NULL",
            id
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| BookingError::Invalid("unknown customer".into())),
        CustomerRef::New(input) => {
            if input.first_name.trim().is_empty() {
                return Err(BookingError::Invalid("first name is required".into()));
            }
            let email = input
                .email
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty());
            if let Some(email) = email
                && let Some(id) = sqlx::query_scalar!(
                    "SELECT id FROM customers WHERE email = $1::text::citext AND deleted_at IS NULL",
                    email
                )
                .fetch_optional(&mut *conn)
                .await?
            {
                return Ok(id);
            }
            let id = Uuid::now_v7();
            sqlx::query!(
                "INSERT INTO customers (id, email, first_name, last_name, phone, country_code, preferred_locale)
                 VALUES ($1, $2::text::citext, $3, $4, $5, $6, $7)",
                id,
                email,
                input.first_name.trim(),
                input.last_name.as_deref(),
                input.phone.as_deref(),
                input.country_code.as_deref(),
                input.preferred_locale.as_ref().map(Locale::as_str),
            )
            .execute(&mut *conn)
            .await?;
            Ok(id)
        }
    }
}

pub(super) struct RatePlanTerms {
    pub payment_mode: PaymentMode,
    pub deposit: Deposit,
    pub cancellation_rules: Option<Value>,
}

pub(super) async fn rate_plan_terms(
    conn: &mut PgConnection,
    id: Option<Uuid>,
) -> Result<Option<RatePlanTerms>, BookingError> {
    let Some(id) = id else { return Ok(None) };
    let plan = sqlx::query!(
        r#"SELECT rp.payment_mode AS "payment_mode: PaymentMode", rp.deposit_type AS "deposit_type: DepositType",
                  rp.deposit_percent, rp.deposit_amount_minor,
                  rp.deposit_currency AS "deposit_currency: CurrencyCode", cp.rules AS "rules?"
           FROM rate_plans rp
           LEFT JOIN cancellation_policies cp ON cp.id = rp.cancellation_policy_id
           WHERE rp.id = $1 AND rp.is_active"#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| BookingError::Invalid("unknown or inactive rate plan".into()))?;
    let deposit = Deposit::from_columns(
        plan.deposit_type,
        plan.deposit_percent,
        plan.deposit_amount_minor,
        plan.deposit_currency,
    )
    .ok_or_else(|| BookingError::Invalid("rate plan has an inconsistent deposit".into()))?;
    Ok(Some(RatePlanTerms {
        payment_mode: plan.payment_mode,
        deposit,
        cancellation_rules: plan.rules,
    }))
}

pub(super) fn deposit_due(
    terms: Option<&RatePlanTerms>,
    total_minor: i64,
    currency: &CurrencyCode,
) -> Result<i64, BookingError> {
    let Some(terms) = terms else { return Ok(0) };
    Ok(match &terms.deposit {
        Deposit::None => 0,
        Deposit::Full => total_minor,
        Deposit::Percent { percent } => percent_of(total_minor, *percent)
            .map_err(|e| BookingError::Invalid(format!("deposit: {e}")))?,
        Deposit::Fixed { amount } => {
            if &amount.currency != currency {
                return Err(BookingError::Invalid(format!(
                    "the rate plan's fixed deposit is in {}, the booking in {}",
                    amount.currency.as_str(),
                    currency.as_str()
                )));
            }
            amount.amount_minor.min(total_minor)
        }
    })
}

/// `(adults, children, infants)` summary of the guests, after checking every category exists.
pub(super) async fn guest_summary(
    conn: &mut PgConnection,
    guests: &[GuestCount],
) -> Result<(i16, i16, i16), BookingError> {
    if guests.is_empty()
        || guests.iter().any(|g| g.count < 0)
        || guests.iter().all(|g| g.count == 0)
    {
        return Err(BookingError::Invalid(
            "at least one guest is required".into(),
        ));
    }
    let categories =
        sqlx::query!(r#"SELECT id, code::text AS "code!" FROM guest_categories WHERE is_active"#)
            .fetch_all(&mut *conn)
            .await?;
    let (mut adults, mut children, mut infants) = (0_i32, 0_i32, 0_i32);
    for guest in guests {
        let code = categories
            .iter()
            .find(|c| c.id == guest.guest_category_id)
            .map(|c| c.code.as_str())
            .ok_or_else(|| BookingError::Invalid("unknown guest category".into()))?;
        match code {
            "adult" => adults += guest.count,
            "child" => children += guest.count,
            "infant" => infants += guest.count,
            _ => adults += guest.count,
        }
    }
    let to_i16 =
        |n: i32| i16::try_from(n).map_err(|_| BookingError::Invalid("too many guests".into()));
    Ok((to_i16(adults)?, to_i16(children)?, to_i16(infants)?))
}

/// Enforces `max_redemptions` / `max_per_customer` under a row lock on the promotion, then
/// records the redemption.
pub(super) async fn redeem_promotion(
    conn: &mut PgConnection,
    promotion: &PromotionRule,
    booking_id: Uuid,
    customer_id: Uuid,
    discount_minor: i64,
) -> Result<(), BookingError> {
    let limits = sqlx::query!(
        "SELECT max_redemptions, max_per_customer FROM promotions WHERE id = $1 FOR UPDATE",
        promotion.id
    )
    .fetch_one(&mut *conn)
    .await?;
    let used = sqlx::query!(
        r#"SELECT count(*) AS "total!", count(*) FILTER (WHERE r.customer_id = $2) AS "by_customer!"
           FROM promotion_redemptions r
           JOIN bookings b ON b.id = r.booking_id
           WHERE r.promotion_id = $1 AND b.status NOT IN ('cancelled', 'expired')"#,
        promotion.id,
        customer_id
    )
    .fetch_one(&mut *conn)
    .await?;
    if limits
        .max_redemptions
        .is_some_and(|m| used.total >= i64::from(m))
    {
        return Err(BookingError::Invalid("promotion fully redeemed".into()));
    }
    if limits
        .max_per_customer
        .is_some_and(|m| used.by_customer >= i64::from(m))
    {
        return Err(BookingError::Invalid(
            "promotion already used by this customer".into(),
        ));
    }
    sqlx::query!(
        "INSERT INTO promotion_redemptions (id, promotion_id, booking_id, customer_id, discount_minor)
         VALUES ($1, $2, $3, $4, $5)",
        Uuid::now_v7(),
        promotion.id,
        booking_id,
        customer_id,
        discount_minor
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub(super) async fn record_status(
    conn: &mut PgConnection,
    booking_id: Uuid,
    from: Option<BookingStatus>,
    to: BookingStatus,
    changed_by: Option<Uuid>,
    note: Option<&str>,
) -> Result<(), BookingError> {
    sqlx::query!(
        "INSERT INTO booking_status_history (booking_id, from_status, to_status, changed_by, note)
         VALUES ($1, $2, $3, $4, $5)",
        booking_id,
        from.map(BookingStatus::as_str),
        to.as_str(),
        changed_by,
        note
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Frozen description of the product at booking time (name in the default locale).
pub(super) async fn product_snapshot(
    conn: &mut PgConnection,
    product_id: Uuid,
) -> Result<Value, BookingError> {
    let p = sqlx::query!(
        r#"SELECT p.kind, p.sku::text AS "sku", pt.name AS "name?", pt.slug AS "slug?"
           FROM products p
           LEFT JOIN product_translations pt
                  ON pt.product_id = p.id AND pt.locale = (SELECT code FROM locales WHERE is_default)
           WHERE p.id = $1"#,
        product_id
    )
    .fetch_one(conn)
    .await?;
    Ok(json!({ "kind": p.kind, "sku": p.sku, "name": p.name, "slug": p.slug }))
}

pub(super) struct NewBooking<'a> {
    pub id: Uuid,
    pub reference: &'a str,
    pub customer_id: Uuid,
    pub status: BookingStatus,
    pub meta: &'a BookingRequestMeta,
    pub currency: &'a CurrencyCode,
    pub arrival: Option<NaiveDate>,
    pub departure: Option<NaiveDate>,
    pub guests: (i16, i16, i16),
    pub quote: &'a Quote,
    pub deposit_due_minor: i64,
    pub cancellation_rules: Option<&'a Value>,
    pub hold_expires_at: Option<DateTime<Utc>>,
}

pub(super) async fn insert_booking(
    conn: &mut PgConnection,
    b: NewBooking<'_>,
) -> Result<(), BookingError> {
    sqlx::query!(
        "INSERT INTO bookings (id, reference, customer_id, status, source, locale, currency, arrival_date,
                               departure_date, adults, children, infants, subtotal_minor, discount_minor,
                               tax_minor, total_minor, deposit_due_minor, rate_plan_id,
                               cancellation_policy_snapshot, hold_expires_at, special_requests, created_by,
                               ip_address, user_agent)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                 $21, $22, $23, $24)",
        b.id,
        b.reference,
        b.customer_id,
        b.status.as_str(),
        b.meta.source.as_str(),
        b.meta.locale.as_str(),
        b.currency.as_str(),
        b.arrival,
        b.departure,
        b.guests.0,
        b.guests.1,
        b.guests.2,
        b.quote.subtotal_minor,
        b.quote.discount_minor,
        b.quote.tax_minor,
        b.quote.total_minor,
        b.deposit_due_minor,
        b.meta.rate_plan_id,
        b.cancellation_rules,
        b.hold_expires_at,
        b.meta.special_requests.as_deref(),
        b.meta.created_by,
        b.meta.ip_address,
        b.meta.user_agent.as_deref(),
    )
    .execute(conn)
    .await?;
    Ok(())
}

pub(super) struct NewItem<'a> {
    pub id: Uuid,
    pub booking_id: Uuid,
    pub product_id: Uuid,
    pub product_kind: &'static str,
    pub rate_plan_id: Option<Uuid>,
    pub activity_slot_id: Option<Uuid>,
    pub service_period: Option<DateRange>,
    pub starts_at: Option<DateTime<Utc>>,
    pub quantity: i32,
    pub quote: &'a Quote,
    pub snapshot: Value,
}

pub(super) async fn insert_item(
    conn: &mut PgConnection,
    item: NewItem<'_>,
    guests: &[GuestCount],
) -> Result<(), BookingError> {
    let q = item.quote;
    sqlx::query!(
        "INSERT INTO booking_items (id, booking_id, product_id, product_kind, rate_plan_id, activity_slot_id,
                                    service_period, starts_at, quantity, unit_price_minor, subtotal_minor,
                                    discount_minor, tax_minor, total_minor, price_breakdown, product_snapshot)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        item.id,
        item.booking_id,
        item.product_id,
        item.product_kind,
        item.rate_plan_id,
        item.activity_slot_id,
        item.service_period as Option<DateRange>,
        item.starts_at,
        item.quantity,
        q.subtotal_minor / i64::from(item.quantity.max(1)),
        q.subtotal_minor,
        q.discount_minor,
        q.tax_minor,
        q.total_minor,
        json!(q.lines),
        item.snapshot,
    )
    .execute(&mut *conn)
    .await?;
    for guest in guests.iter().filter(|g| g.count > 0) {
        sqlx::query!(
            "INSERT INTO booking_item_guests (booking_item_id, guest_category_id, guest_count) VALUES ($1, $2, $3)",
            item.id,
            guest.guest_category_id,
            guest.count
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(super) fn initial_status(terms: Option<&RatePlanTerms>) -> BookingStatus {
    match terms.map(|t| t.payment_mode) {
        Some(PaymentMode::OnlineRequired) => BookingStatus::AwaitingPayment,
        _ => BookingStatus::Pending,
    }
}
