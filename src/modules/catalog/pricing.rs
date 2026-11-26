//! Pricing tab: the price rules of a product, its taxes, the simulator and the "from" price.
//!
//! Nothing is priced here: the simulator calls `trip::quote` (the booking funnel's function,
//! back-office mode), the "from" price and the package items' sample prices call
//! `trip::pricing::base_lines` / the engine, and the ambiguity warning uses the engine's own
//! matching (`engine::ambiguous_pairs`).

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::{Duration, NaiveDate, NaiveTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Fields, Kind, changed, field_error, product_of_kind};
use crate::{
    auth::Staff,
    db::{
        catalog::ProductKind,
        foundation,
        pricing::PricingUnit,
        types::{DateRange, DaysOfWeek, Locale},
    },
    error::{AppError, AppResult},
    services::{
        booking::{
            BookingError,
            trip::{
                self, TripContext, TripItem, TripRequest,
                pricing::{PricingInput, base_lines},
            },
        },
        pricing::{self as engine_pricing, GuestCount, ServiceTime, engine},
    },
    state::AppState,
};

pub async fn staff_context(state: &AppState) -> AppResult<TripContext> {
    Ok(TripContext {
        locale: Locale::new("fr"),
        public: false,
        now: Utc::now(),
        settings: crate::modules::booking::booking_settings(state).await?,
        online_payments: state.payments.is_enabled(),
    })
}

// ---- Price rules ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PriceBody {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub season_id: Option<Uuid>,
    #[serde(default)]
    pub guest_category_id: Option<Uuid>,
    pub pricing_unit: String,
    /// Cents.
    pub amount_minor: i64,
    #[serde(default)]
    pub extra_guest_amount_minor: Option<i64>,
    #[serde(default = "one")]
    pub min_quantity: i32,
    #[serde(default)]
    pub max_quantity: Option<i32>,
    #[serde(default)]
    pub min_nights: Option<i32>,
    /// Mon=1 … Sun=64.
    #[serde(default = "every_day")]
    pub days_of_week: i16,
    #[serde(default)]
    pub valid_from: Option<NaiveDate>,
    /// Exclusive.
    #[serde(default)]
    pub valid_to: Option<NaiveDate>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "yes")]
    pub is_active: bool,
}

fn one() -> i32 {
    1
}

fn every_day() -> i16 {
    127
}

fn yes() -> bool {
    true
}

async fn requires_date(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<bool> {
    Ok(
        sqlx::query_scalar!("SELECT requires_date FROM extras WHERE product_id = $1", id)
            .fetch_optional(conn)
            .await?
            .unwrap_or(false),
    )
}

pub async fn write_prices(
    conn: &mut PgConnection,
    kind: Kind,
    id: Uuid,
    rows: &[PriceBody],
) -> AppResult<()> {
    let allowed = kind.allowed_units(requires_date(conn, id).await?);
    let mut f = Fields::default();
    for (i, p) in rows.iter().enumerate() {
        let path = |field: &str| format!("prices[{i}].{field}");
        if !allowed.contains(&p.pricing_unit.as_str()) {
            f.add(path("pricing_unit"), "not_allowed_for_kind");
        }
        if p.amount_minor < 0 {
            f.add(path("amount_minor"), "min");
        }
        if p.extra_guest_amount_minor.is_some_and(|x| x < 0) {
            f.add(path("extra_guest_amount_minor"), "min");
        }
        if p.extra_guest_amount_minor.is_some() && kind != Kind::Stays {
            f.add(path("extra_guest_amount_minor"), "stays_only");
        }
        if p.min_quantity < 1 {
            f.add(path("min_quantity"), "min");
        }
        if p.max_quantity.is_some_and(|m| m < p.min_quantity) {
            f.add(path("max_quantity"), "below_min");
        }
        if p.min_nights.is_some_and(|m| m < 1) {
            f.add(path("min_nights"), "min");
        }
        if !(1..=127).contains(&p.days_of_week) {
            f.add(path("days_of_week"), "required");
        }
        if let (Some(a), Some(b)) = (p.valid_from, p.valid_to)
            && b <= a
        {
            f.add(path("valid_to"), "before_from");
        }
    }
    f.check()?;
    let currency = foundation::default_currency(&mut *conn).await?.code;
    let keep: Vec<Uuid> = rows.iter().filter_map(|p| p.id).collect();
    sqlx::query!(
        "DELETE FROM prices WHERE product_id = $1 AND NOT (id = ANY($2))",
        id,
        &keep
    )
    .execute(&mut *conn)
    .await?;
    for p in rows {
        let period = match (p.valid_from, p.valid_to) {
            (None, None) => None,
            (a, b) => Some(DateRange { start: a, end: b }),
        };
        sqlx::query!(
            "INSERT INTO prices (id, product_id, rate_plan_id, season_id, guest_category_id, pricing_unit, amount_minor, currency,
                                 extra_guest_amount_minor, min_quantity, max_quantity, min_nights, days_of_week, valid_period,
                                 priority, is_active)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (id) DO UPDATE SET rate_plan_id = $3, season_id = $4, guest_category_id = $5, pricing_unit = $6,
                 amount_minor = $7, extra_guest_amount_minor = $9, min_quantity = $10, max_quantity = $11, min_nights = $12,
                 days_of_week = $13, valid_period = $14, priority = $15, is_active = $16
             WHERE prices.product_id = $2",
            p.id.unwrap_or_else(Uuid::now_v7),
            id,
            p.rate_plan_id,
            p.season_id,
            p.guest_category_id,
            p.pricing_unit,
            p.amount_minor,
            currency.as_str(),
            p.extra_guest_amount_minor,
            p.min_quantity,
            p.max_quantity,
            p.min_nights,
            p.days_of_week,
            period as Option<DateRange>,
            p.priority,
            p.is_active
        )
        .execute(&mut *conn)
        .await
        .map_err(|e| {
            if e.as_database_error().is_some_and(|d| d.is_foreign_key_violation()) {
                field_error("prices", "unknown_reference")
            } else {
                e.into()
            }
        })?;
    }
    Ok(())
}

pub async fn write_taxes(conn: &mut PgConnection, id: Uuid, tax_ids: &[Uuid]) -> AppResult<()> {
    let known = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM taxes WHERE id = ANY($1)"#,
        tax_ids
    )
    .fetch_one(&mut *conn)
    .await?;
    if usize::try_from(known).unwrap_or_default() != tax_ids.len() {
        return Err(field_error("tax_ids", "unknown"));
    }
    sqlx::query!("DELETE FROM product_taxes WHERE product_id = $1", id)
        .execute(&mut *conn)
        .await?;
    for t in tax_ids {
        sqlx::query!(
            "INSERT INTO product_taxes (product_id, tax_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            id,
            t
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The rules of a product with the ambiguous ones flagged.
pub async fn prices(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    product_of_kind(&mut conn, kind, id).await?;
    let rows = sqlx::query!(
        r#"SELECT id, rate_plan_id, season_id, guest_category_id, pricing_unit AS "pricing_unit: PricingUnit",
                  amount_minor, extra_guest_amount_minor, min_quantity, max_quantity, min_nights, days_of_week,
                  valid_period AS "valid_period: DateRange", priority, is_active
           FROM prices WHERE product_id = $1 ORDER BY priority DESC, created_at"#,
        id
    )
    .fetch_all(&mut *conn)
    .await?;
    let rules: Vec<engine::PriceRule> = rows
        .iter()
        .filter(|r| r.is_active)
        .map(|r| engine::PriceRule {
            id: r.id,
            rate_plan_id: r.rate_plan_id,
            season_id: r.season_id,
            guest_category_id: r.guest_category_id,
            unit: r.pricing_unit,
            amount_minor: r.amount_minor,
            extra_guest_amount_minor: r.extra_guest_amount_minor,
            min_quantity: r.min_quantity,
            max_quantity: r.max_quantity,
            min_nights: r.min_nights,
            days_of_week: DaysOfWeek(r.days_of_week),
            valid_period: r.valid_period,
            priority: r.priority,
        })
        .collect();
    let pairs = engine::ambiguous_pairs(&rules);
    let mut with: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (a, b) in &pairs {
        with.entry(*a).or_default().push(*b);
        with.entry(*b).or_default().push(*a);
    }
    let taxes = sqlx::query_scalar!("SELECT tax_id FROM product_taxes WHERE product_id = $1", id)
        .fetch_all(&mut *conn)
        .await?;
    Ok(Json(json!({
        "allowed_units": kind.allowed_units(requires_date(&mut conn, id).await?),
        "tax_ids": taxes,
        "data": rows.iter().map(|r| json!({
            "id": r.id, "rate_plan_id": r.rate_plan_id, "season_id": r.season_id, "guest_category_id": r.guest_category_id,
            "pricing_unit": r.pricing_unit, "amount_minor": r.amount_minor, "extra_guest_amount_minor": r.extra_guest_amount_minor,
            "min_quantity": r.min_quantity, "max_quantity": r.max_quantity, "min_nights": r.min_nights,
            "days_of_week": r.days_of_week, "valid_from": r.valid_period.and_then(|p| p.start),
            "valid_to": r.valid_period.and_then(|p| p.end), "priority": r.priority, "is_active": r.is_active,
            "ambiguous_with": with.get(&r.id).cloned().unwrap_or_default(),
        })).collect::<Vec<_>>(),
    })))
}

pub async fn put_prices(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind_s, id)): Path<(String, Uuid)>,
    Json(rows): Json<Vec<PriceBody>>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind_s)?;
    staff.require("pricing.write")?;
    let mut tx = state.db.begin().await?;
    product_of_kind(&mut tx, kind, id).await?;
    write_prices(&mut tx, kind, id, &rows).await?;
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "prices.updated",
        json!({ "rules": rows.len() }),
    )
    .await?;
    tx.commit().await?;
    prices(State(state), staff, Path((kind_s, id))).await
}

#[derive(Debug, Deserialize)]
pub struct TaxesBody {
    pub tax_ids: Vec<Uuid>,
}

pub async fn put_taxes(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<TaxesBody>,
) -> AppResult<StatusCode> {
    let kind = Kind::parse(&kind)?;
    staff.require("pricing.write")?;
    let mut tx = state.db.begin().await?;
    product_of_kind(&mut tx, kind, id).await?;
    write_taxes(&mut tx, id, &b.tax_ids).await?;
    changed(
        &mut tx,
        &staff.actor(),
        kind,
        id,
        "taxes.updated",
        json!({ "tax_ids": b.tax_ids }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Simulator --------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SimulatorBody {
    /// Stays and packages: first night / start; activities `anytime` and extras: the date.
    #[serde(default)]
    pub date: Option<NaiveDate>,
    /// Stays: the departure.
    #[serde(default)]
    pub departure: Option<NaiveDate>,
    /// Fixed-departure activities.
    #[serde(default)]
    pub slot_id: Option<Uuid>,
    #[serde(default)]
    pub time: Option<NaiveTime>,
    /// `{guest_category_id: count}`.
    #[serde(default)]
    pub guests: HashMap<Uuid, i32>,
    #[serde(default = "one")]
    pub quantity: i32,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub promo_code: Option<String>,
}

/// The request the simulator sends to the funnel (also used by the test that compares both).
pub fn simulator_request(kind: Kind, id: Uuid, b: &SimulatorBody) -> AppResult<TripRequest> {
    let guests: Vec<GuestCount> = b
        .guests
        .iter()
        .filter(|(_, n)| **n > 0)
        .map(|(g, n)| GuestCount {
            guest_category_id: *g,
            count: *n,
        })
        .collect();
    let date = b.date.ok_or_else(|| field_error("date", "required"));
    let item = match kind {
        Kind::Stays => {
            let arrival = date?;
            TripItem::Stay {
                product_id: id,
                arrival,
                departure: b.departure.unwrap_or(arrival + Duration::days(1)),
                quantity: b.quantity,
                guests,
                unit_ids: vec![],
            }
        }
        Kind::Activities => TripItem::Activity {
            product_id: id,
            slot_id: b.slot_id,
            date: if b.slot_id.is_some() {
                None
            } else {
                Some(date?)
            },
            time: b.time,
            guests,
            quantity: b.quantity,
            language: None,
        },
        Kind::Packages => TripItem::Package {
            product_id: id,
            arrival: date?,
            guests,
        },
        Kind::Extras => TripItem::Extra {
            product_id: id,
            parent: None,
            date: b.date,
            quantity: b.quantity,
        },
    };
    Ok(TripRequest {
        items: vec![item],
        rate_plan_id: b.rate_plan_id,
        promo_code: b.promo_code.clone().filter(|c| !c.trim().is_empty()),
        child_ages: vec![],
    })
}

/// "Simulateur de prix": exactly what the booking funnel charges for this input, with the rule
/// of each line, taxes, deposit and cancellation steps.
pub async fn quote(
    State(state): State<AppState>,
    staff: Staff,
    Path((kind, id)): Path<(String, Uuid)>,
    Json(b): Json<SimulatorBody>,
) -> AppResult<Json<Value>> {
    let kind = Kind::parse(&kind)?;
    staff.require("products.read")?;
    let mut conn = state.db.acquire().await?;
    product_of_kind(&mut conn, kind, id).await?;
    let request = simulator_request(kind, id, &b)?;
    let ctx = staff_context(&state).await?;
    let quote = match trip::quote(&mut conn, &request, &ctx).await {
        Ok(q) => q,
        Err(BookingError::Invalid(m)) => {
            return Err(AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                m,
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let rule_ids: Vec<Uuid> = quote
        .items
        .iter()
        .flat_map(|i| i.lines.iter().map(|l| l.price_id))
        .collect();
    let rules = sqlx::query!(
        r#"SELECT id, product_id, pricing_unit, amount_minor, priority, season_id, rate_plan_id, guest_category_id, days_of_week
           FROM prices WHERE id = ANY($1)"#,
        &rule_ids
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(json!({
        "quote": quote,
        "rules": rules.iter().map(|r| (r.id.to_string(), json!({
            "product_id": r.product_id, "pricing_unit": r.pricing_unit, "amount_minor": r.amount_minor, "priority": r.priority,
            "season_id": r.season_id, "rate_plan_id": r.rate_plan_id, "guest_category_id": r.guest_category_id,
            "days_of_week": r.days_of_week,
        }))).collect::<serde_json::Map<String, Value>>(),
    })))
}

// ---- "From" and sample prices -------------------------------------------------------------------

struct Basis {
    adult: Option<Uuid>,
    standard_occupancy: Option<i32>,
    duration_hours: Option<i32>,
    package_mode: Option<String>,
}

async fn basis(conn: &mut PgConnection, id: Uuid) -> sqlx::Result<Basis> {
    let r = sqlx::query!(
        r#"SELECT (SELECT id FROM guest_categories WHERE code = 'adult') AS "adult?",
                  (SELECT standard_occupancy FROM accommodation_types WHERE product_id = $1) AS "occ?",
                  (SELECT duration_minutes FROM activities WHERE product_id = $1) AS "minutes?",
                  (SELECT pricing_mode FROM packages WHERE product_id = $1) AS "mode?""#,
        id
    )
    .fetch_one(conn)
    .await?;
    Ok(Basis {
        adult: r.adult,
        standard_occupancy: r.occ,
        duration_hours: r.minutes.map(|m| ((m + 59) / 60).max(1)),
        package_mode: r.mode,
    })
}

fn input(kind: Kind, id: Uuid, b: &Basis, day: NaiveDate, quantity: i32) -> Option<PricingInput> {
    let adult = b.adult?;
    let (service, guests, standard) = match kind {
        Kind::Stays => (
            ServiceTime::Stay {
                arrival: day,
                departure: day + Duration::days(i64::from(quantity.max(1))),
            },
            b.standard_occupancy.unwrap_or(1),
            b.standard_occupancy,
        ),
        _ => (
            ServiceTime::Date {
                date: day,
                hours: b.duration_hours.or(Some(1)),
                days: Some(1),
            },
            1,
            None,
        ),
    };
    Some(PricingInput {
        product_id: id,
        kind: match kind {
            Kind::Stays => ProductKind::Accommodation,
            Kind::Activities => ProductKind::Activity,
            Kind::Packages => ProductKind::Package,
            Kind::Extras => ProductKind::Extra,
        },
        service,
        guests: vec![GuestCount {
            guest_category_id: adult,
            count: guests,
        }],
        quantity: if kind == Kind::Stays {
            1
        } else {
            quantity.max(1)
        },
        standard_occupancy: standard,
        repeat: 1,
        in_package: false,
        package: None,
        parent: None,
    })
}

/// Lowest subtotal over the next `days` days (stays: one night at standard occupancy; others:
/// one adult), as the funnel prices one item. Packages priced from their items ask the funnel.
pub async fn lowest_price(
    conn: &mut PgConnection,
    state: &AppState,
    kind: Kind,
    id: Uuid,
    days: i64,
) -> AppResult<Option<i64>> {
    let b = basis(conn, id).await?;
    let today = crate::camp_time::today(Utc::now());
    let dates: Vec<NaiveDate> = (1..=days).map(|n| today + Duration::days(n)).collect();
    if kind == Kind::Packages && b.package_mode.as_deref() != Some("fixed") {
        let Some(adult) = b.adult else {
            return Ok(None);
        };
        let ctx = staff_context(state).await?;
        let mut best: Option<i64> = None;
        // Seven start dates are enough for an indication (each one prices every item).
        for day in dates.iter().step_by(4).take(7) {
            let request = TripRequest {
                items: vec![TripItem::Package {
                    product_id: id,
                    arrival: *day,
                    guests: vec![GuestCount {
                        guest_category_id: adult,
                        count: 1,
                    }],
                }],
                ..TripRequest::default()
            };
            if let Ok(q) = trip::quote(conn, &request, &ctx).await
                && q.subtotal_minor > 0
            {
                let v = q.subtotal_minor - q.discount_minor;
                best = Some(best.map_or(v, |x| x.min(v)));
            }
        }
        return Ok(best);
    }
    let currency = foundation::default_currency(&mut *conn).await?.code;
    let (Some(first), Some(last)) = (dates.first(), dates.last()) else {
        return Ok(None);
    };
    // One load for the range, then the engine per date (what `base_lines` does per item).
    let ctx =
        engine_pricing::load_context(conn, id, &currency, *first, *last + Duration::days(1), None)
            .await?;
    let now = Utc::now();
    let mut best: Option<i64> = None;
    for day in &dates {
        let Some(inp) = input(kind, id, &b, *day, 1) else {
            return Ok(None);
        };
        let req = trip::pricing::request(&inp, None, now);
        if let Ok((_, subtotal)) = engine::price_lines(&req, &ctx)
            && subtotal > 0
        {
            best = Some(best.map_or(subtotal, |x| x.min(subtotal)));
        }
    }
    Ok(best)
}

/// One item's price on `day` (package content preview): `quantity` nights for a stay.
pub async fn sample_price(
    conn: &mut PgConnection,
    _state: &AppState,
    kind: Kind,
    id: Uuid,
    day: NaiveDate,
    quantity: i32,
) -> AppResult<Option<i64>> {
    let b = basis(conn, id).await?;
    let Some(inp) = input(kind, id, &b, day, quantity) else {
        return Ok(None);
    };
    let currency = foundation::default_currency(&mut *conn).await?.code;
    match base_lines(conn, &inp, None, &currency, Utc::now()).await {
        Ok((_, subtotal)) => Ok(Some(subtotal)),
        Err(BookingError::Pricing(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
