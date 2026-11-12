//! Availability for the date picker, the search results page and activity slot chips. Cached
//! briefly in Redis under `availability:` (invalidated by every booking change); checkout always
//! re-checks under locks, so a stale cache can never oversell.

use std::{collections::HashMap, time::Duration as StdDuration};

use axum::{
    Json,
    extract::{Query, State},
};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{booking_settings, catalog};
use crate::{
    camp_time,
    db::{catalog::ProductKind, foundation, inventory::SlotStatus},
    error::{AppError, AppResult},
    services::{
        booking::trip::{self, TripContext, TripItem, TripRequest},
        pricing::{self, GuestCount, PricingRequest, ServiceTime},
    },
    state::AppState,
    web::{
        client::{ClientInfo, RequestLocale},
        limits::rate_limit,
    },
};

const TTL: StdDuration = StdDuration::from_secs(60);

#[derive(Debug, Deserialize)]
pub struct CalendarQuery {
    /// One accommodation, or every published one ("any accommodation").
    pub product_id: Option<Uuid>,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarDay {
    pub date: NaiveDate,
    /// At least one unit free for this night.
    pub available: bool,
    pub remaining: i32,
    pub closed_to_arrival: bool,
    pub closed_to_departure: bool,
    /// Minimum nights for a stay arriving this day.
    pub min_nights: i32,
    /// "From" price per night (standard occupancy), when prices are shown in the calendar.
    pub price_minor: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calendar {
    pub product_id: Option<Uuid>,
    pub currency: String,
    pub min_nights: i32,
    pub max_nights: Option<i32>,
    pub check_in_from: Option<chrono::NaiveTime>,
    pub check_out_until: Option<chrono::NaiveTime>,
    pub days: Vec<CalendarDay>,
}

pub async fn calendar(
    State(state): State<AppState>,
    Query(q): Query<CalendarQuery>,
) -> AppResult<Json<Calendar>> {
    if q.to <= q.from || (q.to - q.from).num_days() > 124 {
        return Err(AppError::BadRequest("from < to, at most 4 months".into()));
    }
    let key = format!(
        "availability:calendar:{}:{}:{}",
        q.product_id.map_or_else(|| "any".into(), |p| p.to_string()),
        q.from,
        q.to
    );
    if let Some(cached) = state.cache.get_json::<Calendar>(&key).await {
        return Ok(Json(cached));
    }
    let settings = booking_settings(&state).await?;
    let products: Vec<Uuid> = match q.product_id {
        Some(id) => vec![id],
        None => sqlx::query_scalar!(
            "SELECT a.product_id FROM accommodation_types a JOIN products p ON p.id = a.product_id
             WHERE p.status = 'published' AND p.deleted_at IS NULL AND p.booking_mode <> 'not_bookable'"
        )
        .fetch_all(&state.db)
        .await?,
    };
    let rows = sqlx::query!(
        "SELECT ic.product_id, ic.day, ic.capacity, ic.reserved, ic.stop_sell, ic.min_nights_override,
                ic.closed_to_arrival, ic.closed_to_departure, a.min_nights,
                EXISTS (SELECT 1 FROM closures c WHERE c.period @> ic.day
                        AND (c.scope = 'site' OR (c.scope = 'product' AND c.product_id = ic.product_id))) AS closed
         FROM inventory_calendar ic JOIN accommodation_types a ON a.product_id = ic.product_id
         WHERE ic.product_id = ANY($1) AND ic.day >= $2 AND ic.day < $3
         ORDER BY ic.day",
        &products,
        q.from,
        q.to
    )
    .fetch_all(&state.db)
    .await?;
    let terms = sqlx::query!(
        "SELECT min(min_nights) AS min_nights, max(max_nights) AS max_nights,
                min(check_in_from) AS check_in_from, max(check_out_until) AS check_out_until
         FROM accommodation_types WHERE product_id = ANY($1)",
        &products
    )
    .fetch_one(&state.db)
    .await?;

    // Nightly "from" prices: the engine on one-night stays at standard occupancy.
    let currency = foundation::default_currency(&state.db).await?.code;
    let mut prices: HashMap<(Uuid, NaiveDate), i64> = HashMap::new();
    if settings.calendar_show_prices {
        let adult = sqlx::query_scalar!(
            "SELECT id FROM guest_categories WHERE code = 'adult' AND is_active"
        )
        .fetch_optional(&state.db)
        .await?;
        let mut conn = state.db.acquire().await?;
        for product_id in &products {
            let occupancy = sqlx::query_scalar!(
                "SELECT standard_occupancy FROM accommodation_types WHERE product_id = $1",
                product_id
            )
            .fetch_optional(&mut *conn)
            .await?
            .unwrap_or(2);
            let Some(adult) = adult else { break };
            let guests = [GuestCount {
                guest_category_id: adult,
                count: occupancy,
            }];
            let ctx = pricing::load_context(&mut conn, *product_id, &currency, q.from, q.to, None)
                .await?;
            let mut day = q.from;
            while day < q.to {
                let req = PricingRequest {
                    product_id: *product_id,
                    rate_plan_id: None,
                    service: ServiceTime::Stay {
                        arrival: day,
                        departure: day + Duration::days(1),
                    },
                    guests: &guests,
                    quantity: 1,
                    standard_occupancy: Some(occupancy),
                    booked_at: Utc::now(),
                    booked_on: camp_time::today(Utc::now()),
                };
                if let Ok((_, subtotal)) = pricing::engine::price_lines(&req, &ctx) {
                    prices.insert((*product_id, day), subtotal);
                }
                day += Duration::days(1);
            }
        }
    }

    let mut by_day: std::collections::BTreeMap<NaiveDate, CalendarDay> =
        std::collections::BTreeMap::new();
    for r in rows {
        let open = !r.stop_sell && !r.closed.unwrap_or(false);
        let remaining = if open {
            (r.capacity - r.reserved).max(0)
        } else {
            0
        };
        let price = prices
            .get(&(r.product_id, r.day))
            .copied()
            .filter(|_| remaining > 0);
        let min_nights = i32::from(r.min_nights_override.unwrap_or(0)).max(r.min_nights);
        let entry = by_day.entry(r.day).or_insert(CalendarDay {
            date: r.day,
            available: false,
            remaining: 0,
            // Aggregate: a restriction applies only when every product has it.
            closed_to_arrival: true,
            closed_to_departure: true,
            min_nights: i32::MAX,
            price_minor: None,
        });
        entry.available |= remaining > 0;
        entry.remaining += remaining;
        entry.closed_to_arrival &= r.closed_to_arrival || remaining == 0;
        entry.closed_to_departure &= r.closed_to_departure;
        entry.min_nights = entry.min_nights.min(min_nights);
        entry.price_minor = match (entry.price_minor, price) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    let calendar = Calendar {
        product_id: q.product_id,
        currency: currency.as_str().to_string(),
        min_nights: terms.min_nights.unwrap_or(1),
        max_nights: terms.max_nights,
        check_in_from: terms.check_in_from,
        check_out_until: terms.check_out_until,
        days: by_day
            .into_values()
            .map(|mut d| {
                if d.min_nights == i32::MAX {
                    d.min_nights = 1;
                }
                d
            })
            .collect(),
    };
    state.cache.set_json(&key, &calendar, TTL).await;
    Ok(Json(calendar))
}

#[derive(Debug, Deserialize)]
pub struct SearchBody {
    pub arrival: NaiveDate,
    pub departure: NaiveDate,
    pub guests: Vec<GuestCount>,
    #[serde(default)]
    pub child_ages: Vec<u8>,
    #[serde(default)]
    pub promo_code: Option<String>,
}

/// Accommodation types for the dates and party: each with its rate plans side by side (total
/// for the stay, payment and cancellation terms), unavailable ones with their nearest dates.
pub async fn search(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    client: ClientInfo,
    Json(body): Json<SearchBody>,
) -> AppResult<Json<Value>> {
    rate_limit(
        &state.cache,
        "search",
        &client.key(),
        60,
        StdDuration::from_secs(60),
    )
    .await?;
    if body.departure <= body.arrival || (body.departure - body.arrival).num_days() > 60 {
        return Err(AppError::BadRequest("invalid dates".into()));
    }
    let settings = booking_settings(&state).await?;
    let ctx = TripContext {
        locale: locale.clone(),
        public: true,
        now: Utc::now(),
        settings: settings.clone(),
        online_payments: state.payments.is_enabled(),
    };
    let cards = catalog::cards(
        &state,
        locale.as_str(),
        Some(ProductKind::Accommodation),
        false,
        None,
    )
    .await?;
    let plans = sqlx::query_scalar!(
        "SELECT id FROM rate_plans WHERE is_active AND is_public ORDER BY sort_order, code"
    )
    .fetch_all(&state.db)
    .await?;
    let max_guests: HashMap<Uuid, i32> =
        sqlx::query!("SELECT product_id, max_guests FROM accommodation_types")
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| (r.product_id, r.max_guests))
            .collect();
    let capacity_party: i32 = {
        let counted: Vec<Uuid> =
            sqlx::query_scalar!("SELECT id FROM guest_categories WHERE counts_toward_capacity")
                .fetch_all(&state.db)
                .await?;
        body.guests
            .iter()
            .filter(|g| counted.contains(&g.guest_category_id))
            .map(|g| g.count)
            .sum()
    };
    let nights = (body.departure - body.arrival).num_days();
    let mut conn = state.db.acquire().await?;
    let mut results = Vec::new();
    for card in cards {
        let per_unit = max_guests.get(&card.id).copied().unwrap_or(1).max(1);
        let quantity = ((capacity_party + per_unit - 1) / per_unit).max(1);
        let mut options = Vec::new();
        let mut issue = None;
        for plan in plans
            .iter()
            .map(Some)
            .chain(plans.is_empty().then_some(None))
        {
            let request = TripRequest {
                items: vec![TripItem::Stay {
                    product_id: card.id,
                    arrival: body.arrival,
                    departure: body.departure,
                    quantity,
                    guests: body.guests.clone(),
                    unit_ids: vec![],
                }],
                rate_plan_id: plan.copied(),
                promo_code: body.promo_code.clone(),
                child_ages: body.child_ages.clone(),
            };
            let quote = trip::quote(&mut conn, &request, &ctx).await?;
            if let Some(i) = quote.items.iter().find_map(|i| i.issue.clone()) {
                issue = Some(i);
                break;
            }
            options.push(json!({
                "rate_plan": quote.rate_plan,
                "total_minor": quote.total_minor,
                "subtotal_minor": quote.subtotal_minor,
                "discount_minor": quote.discount_minor,
                "discounts": quote.discounts,
                "tax_minor": quote.tax_minor,
                "included_tax_minor": quote.included_tax_minor,
                "per_night_minor": quote.total_minor / nights.max(1),
                "payment_options": quote.payment_options,
                "cancellation": quote.cancellation,
                "is_request": quote.is_request,
                "items": quote.items,
                "remaining": quote.items.first().and_then(|i| i.remaining),
            }));
        }
        let nearest = if issue.is_some() {
            nearest_dates(&state, card.id, body.arrival, nights, quantity).await?
        } else {
            None
        };
        results.push(json!({
            "product": card,
            "available": issue.is_none() && !options.is_empty(),
            "quantity": quantity,
            "nights": nights,
            "issue": issue,
            "options": options,
            "nearest": nearest,
        }));
    }
    let urgency = settings.urgency_threshold;
    Ok(Json(json!({
        "arrival": body.arrival,
        "departure": body.departure,
        "nights": nights,
        "urgency_threshold": urgency,
        "results": results,
    })))
}

/// The closest arrival (within 45 days around the wanted one) where the same stay fits.
async fn nearest_dates(
    state: &AppState,
    product_id: Uuid,
    wanted: NaiveDate,
    nights: i64,
    quantity: i32,
) -> AppResult<Option<Value>> {
    let today = camp_time::today(Utc::now());
    let from = (wanted - Duration::days(21)).max(today);
    let to = wanted + Duration::days(45 + nights);
    let rows = sqlx::query!(
        "SELECT ic.day, (ic.capacity - ic.reserved >= $4 AND NOT ic.stop_sell
                         AND NOT EXISTS (SELECT 1 FROM closures c WHERE c.period @> ic.day
                             AND (c.scope = 'site' OR (c.scope = 'product' AND c.product_id = ic.product_id)))) AS free,
                ic.closed_to_arrival, ic.closed_to_departure, COALESCE(ic.min_nights_override, 0) AS min_override
         FROM inventory_calendar ic WHERE ic.product_id = $1 AND ic.day >= $2 AND ic.day < $3 ORDER BY ic.day",
        product_id,
        from,
        to,
        quantity
    )
    .fetch_all(&state.db)
    .await?;
    let by_day: HashMap<NaiveDate, (bool, bool, bool, i64)> = rows
        .into_iter()
        .map(|r| {
            (
                r.day,
                (
                    r.free.unwrap_or(false),
                    r.closed_to_arrival,
                    r.closed_to_departure,
                    i64::from(r.min_override.unwrap_or(0)),
                ),
            )
        })
        .collect();
    let mut candidates: Vec<NaiveDate> = from
        .iter_days()
        .take_while(|d| *d < to - Duration::days(nights))
        .collect();
    candidates.sort_by_key(|d| ((*d - wanted).num_days().abs(), *d));
    for arrival in candidates.into_iter().filter(|d| *d != wanted) {
        let departure = arrival + Duration::days(nights);
        let Some((_, cta, _, min)) = by_day.get(&arrival) else {
            continue;
        };
        if *cta || nights < *min {
            continue;
        }
        if by_day.get(&departure).is_some_and(|d| d.2) {
            continue;
        }
        if arrival
            .iter_days()
            .take_while(|d| *d < departure)
            .all(|d| by_day.get(&d).is_some_and(|x| x.0))
        {
            return Ok(Some(json!({ "arrival": arrival, "departure": departure })));
        }
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
pub struct SlotsQuery {
    pub product_id: Uuid,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotView {
    pub id: Uuid,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub date: NaiveDate,
    pub remaining: i32,
    pub capacity: i32,
    /// `open`, `full`, `too_late`, `closed`.
    pub state: String,
    pub language: Option<String>,
}

pub async fn slots(
    State(state): State<AppState>,
    Query(q): Query<SlotsQuery>,
) -> AppResult<Json<Value>> {
    if q.to <= q.from || (q.to - q.from).num_days() > 93 {
        return Err(AppError::BadRequest("from < to, at most 3 months".into()));
    }
    let key = format!("availability:slots:{}:{}:{}", q.product_id, q.from, q.to);
    if let Some(cached) = state.cache.get_json::<Value>(&key).await {
        return Ok(Json(cached));
    }
    let activity = sqlx::query!(
        r#"SELECT a.scheduling_mode, a.booking_cutoff_hours, a.min_participants, a.max_participants, a.min_age,
                  a.guide_languages, a.duration_minutes
           FROM activities a JOIN products p ON p.id = a.product_id
           WHERE a.product_id = $1 AND p.status = 'published' AND p.deleted_at IS NULL"#,
        q.product_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let from = camp_time::to_utc(q.from.and_hms_opt(0, 0, 0).unwrap_or_default());
    let to = camp_time::to_utc(q.to.and_hms_opt(0, 0, 0).unwrap_or_default());
    let now = Utc::now();
    let cutoff = Duration::hours(i64::from(activity.booking_cutoff_hours));
    // Closure periods are compared with each slot's camp-local date in Rust (never SQL time zones).
    let closures: Vec<crate::db::types::DateRange> = sqlx::query_scalar!(
        r#"SELECT period AS "period: crate::db::types::DateRange" FROM closures
           WHERE period && daterange($2, $3) AND (scope = 'site' OR (scope = 'product' AND product_id = $1))"#,
        q.product_id,
        q.from,
        q.to
    )
    .fetch_all(&state.db)
    .await?;
    let slots: Vec<SlotView> = sqlx::query!(
        r#"SELECT s.id, s.starts_at, s.ends_at, s.capacity, s.reserved, s.status AS "status: SlotStatus", s.guide_language
           FROM activity_slots s
           WHERE s.product_id = $1 AND s.starts_at >= $2 AND s.starts_at < $3 AND s.status <> 'cancelled'
           ORDER BY s.starts_at"#,
        q.product_id,
        from,
        to
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|s| {
        let remaining = (s.capacity - s.reserved).max(0);
        let date = camp_time::today(s.starts_at);
        let closed = closures.iter().any(|c| c.contains(date));
        let state = if closed || s.status == SlotStatus::Closed {
            "closed"
        } else if s.starts_at - cutoff < now {
            "too_late"
        } else if remaining == 0 || s.status == SlotStatus::Full {
            "full"
        } else {
            "open"
        };
        SlotView {
            id: s.id,
            starts_at: s.starts_at,
            ends_at: s.ends_at,
            date,
            remaining,
            capacity: s.capacity,
            state: state.into(),
            language: s.guide_language,
        }
    })
    .collect();
    let body = json!({
        "product_id": q.product_id,
        "scheduling_mode": activity.scheduling_mode,
        "booking_cutoff_hours": activity.booking_cutoff_hours,
        "min_participants": activity.min_participants,
        "max_participants": activity.max_participants,
        "min_age": activity.min_age,
        "guide_languages": activity.guide_languages,
        "duration_minutes": activity.duration_minutes,
        "slots": slots,
    });
    state.cache.set_json(&key, &body, TTL).await;
    Ok(Json(body))
}
