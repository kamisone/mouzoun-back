//! Checkout: creates the booking of a trip in ONE transaction that locks the inventory,
//! re-checks availability, re-prices, compares with what the guest accepted, then writes the
//! booking, its items, allocations, redemptions, status history, audit entry and outbox events.

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::{Built, Resolved, Service, TripContext, TripQuote, TripRequest, build, item_start};
use crate::{
    db::{
        bookings::{BalanceCollection, BookingSource, BookingStatus, PaymentOption},
        catalog::ProductKind,
        types::DateRange,
    },
    services::{
        audit::{self, Actor},
        booking::{
            BookingError,
            common::{
                CustomerRef, guest_summary, product_snapshot, record_status, redeem_promotion,
                resolve_customer,
            },
        },
        numbering,
        outbox::{self, Event},
    },
};

#[derive(Debug, Clone)]
pub struct CheckoutInput {
    pub request: TripRequest,
    pub customer: CustomerRef,
    pub payment_option: PaymentOption,
    /// The total the guest accepted (from the signed quote). `None` for staff bookings.
    pub expected_total_minor: Option<i64>,
    pub special_requests: Option<String>,
    pub estimated_arrival_time: Option<NaiveTime>,
    pub terms_version: Option<String>,
    pub marketing_consent: bool,
    pub whatsapp_preferred: bool,
    /// Consent to keep the card for the balance (deposit option, saved-card collection).
    pub save_card_for_balance: bool,
    pub source: BookingSource,
    pub actor: Actor,
    pub skip_guest_emails: bool,
    pub request_id: Option<String>,
    pub ip_address: Option<IpNetwork>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedTrip {
    pub booking_id: Uuid,
    pub reference: String,
    pub status: BookingStatus,
    pub total_minor: i64,
    /// Amount the guest pays now (0 for pay-at-camp and requests).
    pub due_now_minor: i64,
    pub payment_option: PaymentOption,
    pub hold_expires_at: Option<DateTime<Utc>>,
    pub customer_id: Uuid,
    pub quote: TripQuote,
}

pub async fn create(
    pool: &PgPool,
    input: &CheckoutInput,
    ctx: &TripContext,
) -> Result<CreatedTrip, BookingError> {
    let reference = numbering::next_booking_reference(pool, ctx.now).await?;
    let mut tx = pool.begin().await?;
    let built = build(&mut tx, &input.request, ctx, true).await?;
    let quote = &built.quote;

    if let Some((index, issue)) = quote
        .items
        .iter()
        .find_map(|i| i.issue.clone().map(|issue| (i.index, issue)))
    {
        return Err(BookingError::ItemUnavailable { index, issue });
    }
    if let Some(promo) = &quote.promo
        && promo.status != "applied"
    {
        return Err(BookingError::PromoInvalid(promo.clone()));
    }
    if let Some(expected) = input.expected_total_minor
        && expected != quote.total_minor
    {
        return Err(BookingError::PriceChanged {
            expected,
            actual: quote.total_minor,
        });
    }
    let choice = quote
        .choice(input.payment_option)
        .cloned()
        .ok_or(BookingError::PaymentOptionNotAllowed)?;

    // Status and hold: a request waits for the camp; online payment holds for a few minutes;
    // paying at the camp (or nothing to pay) confirms at once.
    let (status, hold_expires_at) = if quote.is_request {
        (
            BookingStatus::Pending,
            Some(ctx.now + Duration::hours(ctx.settings.request_answer_hours)),
        )
    } else if choice.due_now_minor > 0 {
        (
            BookingStatus::AwaitingPayment,
            Some(ctx.now + Duration::minutes(ctx.settings.hold_minutes)),
        )
    } else {
        (BookingStatus::Confirmed, None)
    };

    let customer_id = resolve_customer(&mut tx, &input.customer).await?;
    update_customer(&mut tx, customer_id, input, ctx.now).await?;

    let main_guests = built
        .resolved
        .iter()
        .filter(|r| r.issue.is_none() && !r.guests.is_empty())
        .max_by_key(|r| r.guests.iter().map(|g| g.count).sum::<i32>())
        .map(|r| r.guests.clone())
        .unwrap_or_default();
    let party = guest_summary(&mut tx, &main_guests).await?;
    let (arrival, departure) = dates(&built);
    let balance_due_on = (input.payment_option == PaymentOption::Deposit)
        .then_some(choice.due_later_on)
        .flatten();
    let balance_collection = match input.payment_option {
        PaymentOption::Deposit if input.save_card_for_balance => Some(BalanceCollection::SavedCard),
        PaymentOption::Deposit => Some(BalanceCollection::PaymentLink),
        PaymentOption::OnSite => Some(BalanceCollection::OnSite),
        PaymentOption::Full => None,
    };
    let deposit_due_minor = match input.payment_option {
        PaymentOption::Deposit => choice.due_now_minor,
        PaymentOption::Full => quote.total_minor,
        PaymentOption::OnSite => 0,
    };
    let booking_id = Uuid::now_v7();
    let policy = built
        .rate_plan
        .as_ref()
        .and_then(|p| p.cancellation_rules.clone());

    sqlx::query!(
        "INSERT INTO bookings (id, reference, customer_id, status, source, locale, currency, arrival_date,
                               departure_date, adults, children, infants, subtotal_minor, discount_minor,
                               tax_minor, total_minor, deposit_due_minor, balance_due_on, rate_plan_id,
                               cancellation_policy_snapshot, hold_expires_at, estimated_arrival_time,
                               special_requests, created_by, ip_address, user_agent, confirmed_at,
                               payment_option, balance_collection, terms_version, terms_accepted_at,
                               skip_guest_emails, checkout_request_id, balance_consent_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20,
                 $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33, $34)",
        booking_id,
        reference,
        customer_id,
        status.as_str(),
        input.source.as_str(),
        ctx.locale.as_str(),
        quote.currency.as_str(),
        arrival,
        departure,
        party.0,
        party.1,
        party.2,
        quote.subtotal_minor,
        quote.discount_minor,
        quote.tax_minor,
        quote.total_minor,
        deposit_due_minor,
        balance_due_on,
        built.rate_plan.as_ref().map(|p| p.id),
        policy,
        hold_expires_at,
        input.estimated_arrival_time,
        input
            .special_requests
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        input.actor.user_id,
        input.ip_address,
        input.user_agent.as_deref(),
        (status == BookingStatus::Confirmed).then_some(ctx.now),
        input.payment_option.as_str(),
        balance_collection.map(BalanceCollection::as_str),
        input.terms_version.as_deref(),
        input.terms_version.as_ref().map(|_| ctx.now),
        input.skip_guest_emails,
        input.request_id.as_deref(),
        (balance_collection == Some(BalanceCollection::SavedCard)).then_some(ctx.now),
    )
    .execute(&mut *tx)
    .await?;

    let item_ids = insert_items(&mut tx, booking_id, &built, ctx).await?;
    reserve(&mut tx, &built, &item_ids).await?;

    for (promotion, amounts) in &built.promotions {
        let amount: i64 = amounts.iter().map(|a| a.1).sum();
        redeem_promotion(&mut tx, promotion, booking_id, customer_id, amount).await?;
    }
    record_status(&mut tx, booking_id, None, status, input.actor.user_id, None).await?;
    audit::record(
        &mut tx,
        &input.actor,
        "booking.created",
        "booking",
        &booking_id.to_string(),
        json!({ "after": {
            "reference": reference, "status": status, "total_minor": quote.total_minor,
            "payment_option": input.payment_option, "source": input.source,
        }}),
    )
    .await?;
    outbox::enqueue(&mut tx, &Event::BookingCreated { booking_id }).await?;
    if status == BookingStatus::Confirmed {
        outbox::enqueue(&mut tx, &Event::BookingConfirmed { booking_id }).await?;
    }
    tx.commit().await?;

    tracing::info!(booking = %reference, status = %status, total_minor = quote.total_minor, "booking created");
    Ok(CreatedTrip {
        booking_id,
        reference,
        status,
        total_minor: quote.total_minor,
        due_now_minor: if status == BookingStatus::AwaitingPayment {
            choice.due_now_minor
        } else {
            0
        },
        payment_option: input.payment_option,
        hold_expires_at,
        customer_id,
        quote: built.quote,
    })
}

async fn update_customer(
    conn: &mut PgConnection,
    customer_id: Uuid,
    input: &CheckoutInput,
    now: DateTime<Utc>,
) -> Result<(), BookingError> {
    let (phone, country) = match &input.customer {
        CustomerRef::New(c) => (c.phone.clone(), c.country_code.clone()),
        CustomerRef::Existing(_) => (None, None),
    };
    sqlx::query!(
        "UPDATE customers
         SET phone = COALESCE(NULLIF(phone, ''), $2),
             country_code = COALESCE(country_code, $3),
             whatsapp_preferred = whatsapp_preferred OR $4,
             marketing_consent = marketing_consent OR $5,
             marketing_consent_at = CASE WHEN $5 AND NOT marketing_consent THEN $6 ELSE marketing_consent_at END
         WHERE id = $1",
        customer_id,
        phone,
        country,
        input.whatsapp_preferred,
        input.marketing_consent,
        now
    )
    .execute(conn)
    .await?;
    Ok(())
}

fn dates(built: &Built) -> (Option<NaiveDate>, Option<NaiveDate>) {
    let stays: Vec<(NaiveDate, NaiveDate)> = built
        .resolved
        .iter()
        .filter_map(|r| match r.service {
            Service::Stay {
                arrival, departure, ..
            } => Some((arrival, departure)),
            _ => None,
        })
        .collect();
    if stays.is_empty() {
        let first = built.quote.starts_on;
        let last = built
            .quote
            .ends_on
            .filter(|l| first.is_some_and(|f| l > &f));
        return (first, last);
    }
    (
        stays.iter().map(|s| s.0).min(),
        stays.iter().map(|s| s.1).max(),
    )
}

async fn insert_items(
    conn: &mut PgConnection,
    booking_id: Uuid,
    built: &Built,
    ctx: &TripContext,
) -> Result<Vec<Uuid>, BookingError> {
    let ids: Vec<Uuid> = built.resolved.iter().map(|_| Uuid::now_v7()).collect();
    let policy = built
        .rate_plan
        .as_ref()
        .and_then(|p| p.cancellation_rules.clone());
    for (i, (r, q)) in built.resolved.iter().zip(&built.quote.items).enumerate() {
        let mut snapshot = product_snapshot(conn, r.product.id).await?;
        snapshot["localized_name"] = json!(r.product.name);
        snapshot["locale"] = json!(ctx.locale);
        snapshot["cancellation_rules"] = json!(policy);
        snapshot["mandatory"] = json!(r.mandatory);
        snapshot["in_package"] = json!(r.in_package);
        snapshot["starts_at"] = json!(item_start(r));
        snapshot["discounts"] = json!(q.discounts);
        snapshot["taxes"] = json!(q.taxes);
        if let Some(acc) = &r.product.accommodation {
            snapshot["check_in_from"] = json!(acc.check_in_from);
            snapshot["check_out_until"] = json!(acc.check_out_until);
        }
        let service_period = match &r.service {
            Service::Stay {
                arrival, departure, ..
            } => Some(DateRange::new(*arrival, *departure)),
            Service::OnDate { date, .. } if r.product.kind == ProductKind::Extra => {
                Some(DateRange::new(*date, date.succ_opt().unwrap_or(*date)))
            }
            Service::Package { arrival } => {
                let nights = r
                    .product
                    .package
                    .as_ref()
                    .and_then(|p| p.duration_nights)
                    .unwrap_or(0)
                    .max(1);
                Some(DateRange::new(
                    *arrival,
                    *arrival + Duration::days(i64::from(nights)),
                ))
            }
            _ => None,
        };
        let slot_id = match r.service {
            Service::Slot { slot_id } => Some(slot_id),
            _ => None,
        };
        let requested_time = match r.service {
            Service::OnDate { time, .. } => time,
            _ => None,
        };
        let quantity = r.quantity.max(1);
        sqlx::query!(
            "INSERT INTO booking_items (id, booking_id, parent_item_id, product_id, product_kind, rate_plan_id,
                                        activity_slot_id, service_period, starts_at, quantity, unit_price_minor,
                                        subtotal_minor, discount_minor, tax_minor, total_minor, price_breakdown,
                                        product_snapshot, guide_language, requested_time, requested_unit_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
            ids[i],
            booking_id,
            r.parent.map(|p| ids[p]),
            r.product.id,
            r.product.kind.as_str(),
            built.rate_plan.as_ref().map(|p| p.id),
            slot_id,
            service_period as Option<DateRange>,
            r.starts_at,
            quantity,
            q.subtotal_minor / i64::from(quantity),
            q.subtotal_minor,
            q.discount_minor,
            q.tax_minor,
            q.total_minor,
            json!(q.lines),
            snapshot,
            r.language.as_deref(),
            requested_time,
            match &r.service {
                Service::Stay { requested_units, .. } => requested_units.first().copied(),
                _ => None,
            },
        )
        .execute(&mut *conn)
        .await?;
        for guest in r.guests.iter().filter(|g| g.count > 0) {
            sqlx::query!(
                "INSERT INTO booking_item_guests (booking_item_id, guest_category_id, guest_count) VALUES ($1, $2, $3)",
                ids[i],
                guest.guest_category_id,
                guest.count
            )
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(ids)
}

/// Inventory counters, unit allocations and resource assignments (rows are already locked).
async fn reserve(
    conn: &mut PgConnection,
    built: &Built,
    item_ids: &[Uuid],
) -> Result<(), BookingError> {
    let participants = |r: &Resolved| -> i32 {
        r.guests
            .iter()
            .filter(|g| {
                built
                    .categories
                    .iter()
                    .find(|c| c.id == g.guest_category_id)
                    .is_none_or(|c| c.counts_toward_capacity)
            })
            .map(|g| g.count)
            .sum()
    };
    let mut slot_seats: HashMap<Uuid, i32> = HashMap::new();
    for (i, r) in built.resolved.iter().enumerate() {
        match &r.service {
            Service::Stay {
                arrival, departure, ..
            } => {
                sqlx::query!(
                    "UPDATE inventory_calendar SET reserved = reserved + $4
                     WHERE product_id = $1 AND day >= $2 AND day < $3",
                    r.product.id,
                    arrival,
                    departure,
                    r.quantity
                )
                .execute(&mut *conn)
                .await?;
                for unit_id in &r.unit_ids {
                    sqlx::query!(
                        "INSERT INTO unit_allocations (id, booking_item_id, unit_id, stay) VALUES ($1, $2, $3, daterange($4, $5))",
                        Uuid::now_v7(),
                        item_ids[i],
                        unit_id,
                        arrival,
                        departure
                    )
                    .execute(&mut *conn)
                    .await?;
                }
            }
            Service::Slot { slot_id } => {
                *slot_seats.entry(*slot_id).or_default() += participants(r);
            }
            _ => {}
        }
        if let (Some(starts_at), Some(ends_at)) = (r.starts_at, r.ends_at) {
            for resource_id in &r.resource_ids {
                sqlx::query!(
                    "INSERT INTO resource_assignments (id, resource_id, booking_item_id, activity_slot_id, period)
                     VALUES ($1, $2, $3, $4, tstzrange($5, $6))",
                    Uuid::now_v7(),
                    resource_id,
                    item_ids[i],
                    match r.service {
                        Service::Slot { slot_id } => Some(slot_id),
                        _ => None,
                    },
                    starts_at,
                    ends_at
                )
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    let mut slots: Vec<(Uuid, i32)> = slot_seats.into_iter().collect();
    slots.sort();
    for (slot_id, seats) in slots {
        sqlx::query!(
            "UPDATE activity_slots
             SET reserved = reserved + $2,
                 status = CASE WHEN reserved + $2 >= capacity THEN 'full' ELSE status END
             WHERE id = $1",
            slot_id,
            seats
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
