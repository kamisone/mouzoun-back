//! Booking an activity slot (invariant 2): same pattern as accommodations on
//! `activity_slots.reserved`, plus resource assignments (quads, guides...) derived from
//! `product_resource_requirements`. The exclusion constraint on `resource_assignments` is the
//! final guarantee that a resource is never in two places at once.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Africa::Casablanca;
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
        catalog::{BookingMode, ProductStatus},
        foundation,
        inventory::{ResourceKind, ResourceRequirement, SlotStatus},
    },
    services::{
        numbering, outbox,
        pricing::{self, GuestCount, PricingError, PricingRequest, ServiceTime},
    },
};

#[derive(Debug, Clone)]
pub struct ActivityRequest {
    pub slot_id: Uuid,
    pub guests: Vec<GuestCount>,
    /// Units for unit-based prices (e.g. quads for a per-hour quad price). Defaults to 1.
    pub quantity: i32,
    pub meta: BookingRequestMeta,
}

pub async fn book_activity(
    pool: &PgPool,
    req: &ActivityRequest,
    now: DateTime<Utc>,
    today: NaiveDate,
) -> Result<BookingCreated, BookingError> {
    if req.quantity < 1 {
        return Err(BookingError::Invalid("quantity must be at least 1".into()));
    }
    // Numbered before the transaction, in its own atomic statement: the year's counter row is
    // locked for microseconds, not for the whole booking (a failed booking leaves a gap).
    let reference = numbering::next_booking_reference(pool, now).await?;
    let mut tx = pool.begin().await?;

    let slot = sqlx::query!(
        r#"SELECT product_id, starts_at, ends_at, capacity, reserved, status AS "status: SlotStatus"
           FROM activity_slots WHERE id = $1 FOR UPDATE"#,
        req.slot_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BookingError::NotFound)?;
    let activity = sqlx::query!(
        r#"SELECT p.status AS "status: ProductStatus", p.booking_mode AS "booking_mode: BookingMode",
                  a.max_participants, a.booking_cutoff_hours, a.duration_minutes
           FROM products p JOIN activities a ON a.product_id = p.id
           WHERE p.id = $1 AND p.deleted_at IS NULL"#,
        slot.product_id
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(BookingError::NotFound)?;

    let public_channel = req.meta.source == BookingSource::Website;
    if activity.booking_mode == BookingMode::NotBookable
        || (public_channel && activity.status != ProductStatus::Published)
    {
        return Err(BookingError::NotAvailable(
            "this activity cannot be booked".into(),
        ));
    }
    if slot.status != SlotStatus::Open {
        return Err(BookingError::NotAvailable(format!(
            "this departure is {}",
            slot.status
        )));
    }
    if slot.starts_at - chrono::Duration::hours(i64::from(activity.booking_cutoff_hours)) < now {
        return Err(BookingError::NotAvailable(
            "too late to book this departure".into(),
        ));
    }
    let guests = guest_summary(&mut tx, &req.guests).await?;

    let local_date = slot.starts_at.with_timezone(&Casablanca).date_naive();
    let currency = foundation::default_currency(&mut *tx).await?;
    let ctx = pricing::load_context(
        &mut tx,
        slot.product_id,
        &currency.code,
        local_date,
        local_date.succ_opt().unwrap_or(local_date),
        req.meta.promo_code.as_deref(),
    )
    .await?;
    if req.meta.promo_code.is_some() && ctx.promotion.is_none() {
        return Err(PricingError::PromotionNotApplicable("unknown or inactive code").into());
    }
    let duration_minutes = activity.duration_minutes.unwrap_or(60);
    let pricing_request = PricingRequest {
        product_id: slot.product_id,
        rate_plan_id: req.meta.rate_plan_id,
        service: ServiceTime::Date {
            date: local_date,
            hours: (duration_minutes % 60 == 0).then_some(duration_minutes / 60),
            days: None,
        },
        guests: &req.guests,
        quantity: req.quantity,
        standard_occupancy: None,
        booked_at: now,
        booked_on: today,
    };
    let participants = i32::try_from(pricing::engine::capacity_guests(&pricing_request, &ctx))
        .map_err(|_| BookingError::Invalid("too many participants".into()))?;
    if participants < 1 {
        return Err(BookingError::Invalid(
            "at least one participant is required".into(),
        ));
    }
    if activity.max_participants.is_some_and(|m| participants > m) {
        return Err(BookingError::Invalid(format!(
            "at most {} participants per booking",
            activity.max_participants.unwrap_or_default()
        )));
    }
    if slot.reserved + participants > slot.capacity {
        return Err(BookingError::NotAvailable(format!(
            "only {} place(s) left",
            slot.capacity - slot.reserved
        )));
    }

    let closed = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM closures
               WHERE period @> $2::date AND (scope = 'site' OR (scope = 'product' AND product_id = $1))
           ) AS "closed!""#,
        slot.product_id,
        local_date
    )
    .fetch_one(&mut *tx)
    .await?;
    if closed {
        return Err(BookingError::NotAvailable("closed on this date".into()));
    }

    // Resources (quads, guides, camels...) for the slot's time window.
    let requirements = sqlx::query_as!(
        ResourceRequirement,
        r#"SELECT product_id, resource_kind AS "resource_kind: ResourceKind", units_per_group,
                  participants_per_group, created_at, updated_at
           FROM product_resource_requirements WHERE product_id = $1 ORDER BY resource_kind"#,
        slot.product_id
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut resource_ids = Vec::new();
    for requirement in &requirements {
        let needed = requirement.units_needed(participants);
        let free = sqlx::query_scalar!(
            "SELECT r.id
             FROM resources r
             WHERE r.kind = $1 AND r.status = 'active'
               AND NOT EXISTS (SELECT 1 FROM resource_assignments a
                               WHERE a.resource_id = r.id AND a.released_at IS NULL
                                 AND a.period && tstzrange($2, $3))
               AND NOT EXISTS (SELECT 1 FROM closures c
                               WHERE c.scope = 'resource' AND c.resource_id = r.id AND c.period @> $4::date)
             ORDER BY r.code
             LIMIT $5
             FOR UPDATE OF r SKIP LOCKED",
            requirement.resource_kind.as_str(),
            slot.starts_at,
            slot.ends_at,
            local_date,
            i64::from(needed)
        )
        .fetch_all(&mut *tx)
        .await?;
        if free.len() < usize::try_from(needed).unwrap_or_default() {
            return Err(BookingError::NotAvailable(format!(
                "not enough {} available",
                requirement.resource_kind
            )));
        }
        resource_ids.extend(free);
    }

    sqlx::query!(
        "UPDATE activity_slots
         SET reserved = reserved + $2,
             status = CASE WHEN reserved + $2 >= capacity THEN 'full' ELSE status END
         WHERE id = $1",
        req.slot_id,
        participants
    )
    .execute(&mut *tx)
    .await?;

    let quote = pricing::quote(&pricing_request, &ctx)?;
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
            arrival: Some(local_date),
            departure: None,
            guests,
            quote: &quote,
            deposit_due_minor,
            cancellation_rules: terms.as_ref().and_then(|t| t.cancellation_rules.as_ref()),
            hold_expires_at,
        },
    )
    .await?;
    let snapshot = product_snapshot(&mut tx, slot.product_id).await?;
    insert_item(
        &mut tx,
        NewItem {
            id: item_id,
            booking_id,
            product_id: slot.product_id,
            product_kind: "activity",
            rate_plan_id: req.meta.rate_plan_id,
            activity_slot_id: Some(req.slot_id),
            service_period: None,
            starts_at: Some(slot.starts_at),
            quantity: req.quantity,
            quote: &quote,
            snapshot,
        },
        &req.guests,
    )
    .await?;
    for resource_id in &resource_ids {
        sqlx::query!(
            "INSERT INTO resource_assignments (id, resource_id, booking_item_id, activity_slot_id, period)
             VALUES ($1, $2, $3, $4, tstzrange($5, $6))",
            Uuid::now_v7(),
            resource_id,
            item_id,
            req.slot_id,
            slot.starts_at,
            slot.ends_at
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
            "total_minor": quote.total_minor, "currency": currency.code, "starts_at": slot.starts_at,
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
        allocated_ids: resource_ids,
    })
}
