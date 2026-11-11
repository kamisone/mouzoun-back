//! Booking an accommodation (invariant 1): one transaction that locks the night rows of
//! `inventory_calendar`, checks every restriction, increments `reserved`, allocates units in
//! unit mode and writes the booking, its item, the status history and an outbox event.

use chrono::{DateTime, NaiveDate, Utc};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::{
    BookingError,
    common::{
        BookingCreated, BookingRequestMeta, NewBooking, NewItem, deposit_due, guest_summary,
        initial_status, insert_booking, insert_item, product_snapshot, rate_plan_terms,
        record_status, redeem_promotion, resolve_customer,
    },
};
use crate::{
    db::{
        bookings::BookingSource,
        catalog::{BookingMode, InventoryMode, ProductStatus},
        foundation,
        types::DateRange,
    },
    services::{
        numbering, outbox,
        pricing::{self, GuestCount, PricingError, PricingRequest, ServiceTime},
    },
};

#[derive(Debug, Clone)]
pub struct AccommodationRequest {
    pub product_id: Uuid,
    pub arrival: NaiveDate,
    pub departure: NaiveDate,
    /// Units (pitches, tents) booked.
    pub quantity: i32,
    pub guests: Vec<GuestCount>,
    pub meta: BookingRequestMeta,
}

pub async fn book_accommodation(
    pool: &PgPool,
    req: &AccommodationRequest,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> Result<BookingCreated, BookingError> {
    if req.departure <= req.arrival {
        return Err(BookingError::Invalid(
            "departure must be after arrival".into(),
        ));
    }
    if req.quantity < 1 {
        return Err(BookingError::Invalid("quantity must be at least 1".into()));
    }
    if req.arrival < today {
        return Err(BookingError::Invalid("arrival is in the past".into()));
    }
    let nights = (req.departure - req.arrival).num_days();
    // Numbered before the transaction, in its own atomic statement: the year's counter row is
    // locked for microseconds, not for the whole booking (a failed booking leaves a gap).
    let reference = numbering::next_booking_reference(pool, now).await?;
    let mut tx = pool.begin().await?;

    let product = sqlx::query!(
        r#"SELECT p.status AS "status: ProductStatus", p.booking_mode AS "booking_mode: BookingMode",
                  a.inventory_mode AS "inventory_mode: InventoryMode", a.max_guests, a.min_nights, a.max_nights,
                  a.standard_occupancy
           FROM products p
           JOIN accommodation_types a ON a.product_id = p.id
           WHERE p.id = $1 AND p.deleted_at IS NULL"#,
        req.product_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BookingError::NotFound)?;
    let public_channel = req.meta.source == BookingSource::Website;
    if product.booking_mode == BookingMode::NotBookable
        || (public_channel && product.status != ProductStatus::Published)
    {
        return Err(BookingError::NotAvailable(
            "this accommodation cannot be booked".into(),
        ));
    }
    if nights < i64::from(product.min_nights)
        || product.max_nights.is_some_and(|m| nights > i64::from(m))
    {
        return Err(BookingError::NotAvailable(format!(
            "stay must be {} night(s) minimum",
            product.min_nights
        )));
    }
    let guests = guest_summary(&mut tx, &req.guests).await?;

    // Lock the nights (and the departure day, for closed-to-departure) in date order: every
    // writer locks in the same order, so concurrent bookings serialise instead of deadlocking.
    let days = sqlx::query!(
        "SELECT day, capacity, reserved, stop_sell, min_nights_override, closed_to_arrival, closed_to_departure
         FROM inventory_calendar
         WHERE product_id = $1 AND day >= $2 AND day <= $3
         ORDER BY day
         FOR UPDATE",
        req.product_id,
        req.arrival,
        req.departure
    )
    .fetch_all(&mut *tx)
    .await?;
    let stay_nights: Vec<_> = days.iter().filter(|d| d.day < req.departure).collect();
    if i64::try_from(stay_nights.len()).unwrap_or_default() != nights {
        return Err(BookingError::NotAvailable(
            "these dates are not open for booking yet".into(),
        ));
    }
    if stay_nights.iter().any(|d| d.stop_sell) {
        return Err(BookingError::NotAvailable(
            "closed for sale on some of these nights".into(),
        ));
    }
    if stay_nights[0].closed_to_arrival {
        return Err(BookingError::NotAvailable(
            "arrivals are not possible on this date".into(),
        ));
    }
    if days
        .iter()
        .any(|d| d.day == req.departure && d.closed_to_departure)
    {
        return Err(BookingError::NotAvailable(
            "departures are not possible on this date".into(),
        ));
    }
    if let Some(min) = stay_nights[0].min_nights_override
        && nights < i64::from(min)
    {
        return Err(BookingError::NotAvailable(format!(
            "stay must be {min} night(s) minimum for this arrival date"
        )));
    }
    if stay_nights
        .iter()
        .any(|d| d.reserved + req.quantity > d.capacity)
    {
        return Err(BookingError::NotAvailable(
            "fully booked on some of these nights".into(),
        ));
    }

    let closed = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM closures
               WHERE period && daterange($2, $3)
                 AND (scope = 'site' OR (scope = 'product' AND product_id = $1))
           ) AS "closed!""#,
        req.product_id,
        req.arrival,
        req.departure
    )
    .fetch_one(&mut *tx)
    .await?;
    if closed {
        return Err(BookingError::NotAvailable(
            "the camp is closed during these dates".into(),
        ));
    }

    let unit_ids = if product.inventory_mode == InventoryMode::Unit {
        let free = sqlx::query_scalar!(
            "SELECT u.id
             FROM accommodation_units u
             WHERE u.product_id = $1 AND u.status = 'active'
               AND NOT EXISTS (SELECT 1 FROM unit_allocations a
                               WHERE a.unit_id = u.id AND a.released_at IS NULL AND a.stay && daterange($2, $3))
               AND NOT EXISTS (SELECT 1 FROM closures c
                               WHERE c.scope = 'unit' AND c.unit_id = u.id AND c.period && daterange($2, $3))
             ORDER BY u.sort_order, u.code
             LIMIT $4",
            req.product_id,
            req.arrival,
            req.departure,
            i64::from(req.quantity)
        )
        .fetch_all(&mut *tx)
        .await?;
        if free.len() < usize::try_from(req.quantity).unwrap_or_default() {
            return Err(BookingError::NotAvailable(
                "no unit free for the whole stay".into(),
            ));
        }
        free
    } else {
        vec![]
    };

    sqlx::query!(
        "UPDATE inventory_calendar SET reserved = reserved + $4
         WHERE product_id = $1 AND day >= $2 AND day < $3",
        req.product_id,
        req.arrival,
        req.departure,
        req.quantity
    )
    .execute(&mut *tx)
    .await?;

    // Pricing.
    let currency = foundation::default_currency(&mut *tx).await?;
    let ctx = pricing::load_context(
        &mut tx,
        req.product_id,
        &currency.code,
        req.arrival,
        req.departure,
        req.meta.promo_code.as_deref(),
    )
    .await?;
    if req.meta.promo_code.is_some() && ctx.promotion.is_none() {
        return Err(PricingError::PromotionNotApplicable("unknown or inactive code").into());
    }
    let pricing_request = PricingRequest {
        product_id: req.product_id,
        rate_plan_id: req.meta.rate_plan_id,
        service: ServiceTime::Stay {
            arrival: req.arrival,
            departure: req.departure,
        },
        guests: &req.guests,
        quantity: req.quantity,
        standard_occupancy: Some(product.standard_occupancy),
        booked_at: now,
        booked_on: today,
    };
    if pricing::engine::capacity_guests(&pricing_request, &ctx)
        > i64::from(product.max_guests) * i64::from(req.quantity)
    {
        return Err(BookingError::Invalid(format!(
            "at most {} guests per unit",
            product.max_guests
        )));
    }
    let quote = pricing::quote(&pricing_request, &ctx)?;

    // Booking rows.
    let terms = rate_plan_terms(&mut tx, req.meta.rate_plan_id).await?;
    let customer_id = resolve_customer(&mut tx, &req.meta.customer).await?;
    let booking_id = Uuid::now_v7();
    let item_id = Uuid::now_v7();
    let status = initial_status(terms.as_ref());
    let hold_expires_at = Some(now + req.meta.hold);
    let deposit_due_minor = deposit_due(terms.as_ref(), quote.total_minor, &ctx.currency)?;
    insert_booking(
        &mut tx,
        NewBooking {
            id: booking_id,
            reference: &reference,
            customer_id,
            status,
            meta: &req.meta,
            currency: &currency.code,
            arrival: Some(req.arrival),
            departure: Some(req.departure),
            guests,
            quote: &quote,
            deposit_due_minor,
            cancellation_rules: terms.as_ref().and_then(|t| t.cancellation_rules.as_ref()),
            hold_expires_at,
        },
    )
    .await?;
    let snapshot = product_snapshot(&mut tx, req.product_id).await?;
    insert_item(
        &mut tx,
        NewItem {
            id: item_id,
            booking_id,
            product_id: req.product_id,
            product_kind: "accommodation",
            rate_plan_id: req.meta.rate_plan_id,
            activity_slot_id: None,
            service_period: Some(DateRange::new(req.arrival, req.departure)),
            starts_at: None,
            quantity: req.quantity,
            quote: &quote,
            snapshot,
        },
        &req.guests,
    )
    .await?;
    for unit_id in &unit_ids {
        sqlx::query!(
            "INSERT INTO unit_allocations (id, booking_item_id, unit_id, stay) VALUES ($1, $2, $3, daterange($4, $5))",
            Uuid::now_v7(),
            item_id,
            unit_id,
            req.arrival,
            req.departure
        )
        .execute(&mut *tx)
        .await?;
    }
    if let (Some(promotion), Some(discount)) = (&ctx.promotion, &quote.discount) {
        redeem_promotion(
            &mut tx,
            promotion,
            booking_id,
            customer_id,
            discount.amount_minor,
        )
        .await?;
    }
    record_status(&mut tx, booking_id, None, status, req.meta.created_by, None).await?;
    outbox::enqueue(
        &mut tx,
        "booking",
        &booking_id.to_string(),
        "booking.created",
        json!({
            "booking_id": booking_id, "reference": reference, "status": status,
            "total_minor": quote.total_minor, "currency": currency.code,
            "arrival_date": req.arrival, "departure_date": req.departure,
        }),
    )
    .await?;
    tx.commit().await?;

    Ok(BookingCreated {
        booking_id,
        reference,
        status,
        currency: currency.code,
        total_minor: quote.total_minor,
        deposit_due_minor,
        hold_expires_at,
        item_id,
        allocated_ids: unit_ids,
    })
}
