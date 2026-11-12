//! Pricing engine: a pure function from (request, rules) to a priced quote, in minor units.
//!
//! Rules:
//! - a `prices` row applies to a night (or service date) when its rate plan is NULL or the
//!   requested one, its season is NULL or the night's season, the weekday is in `days_of_week`
//!   (Mon=1 … Sun=64), `valid_period` contains the date, `min_nights` is met and the quantity is
//!   within `[min_quantity, max_quantity]`;
//! - the most specific applicable row wins (each of rate plan, season, guest category, validity
//!   period, weekday restriction and minimum nights adds one level), then the highest
//!   `priority`, then the lowest id (deterministic);
//! - a night's season is the highest-priority season whose period contains it;
//! - stays use nightly units (`per_unit_per_night`, `per_person_per_night`), dated services use
//!   the others (`per_person`, `per_unit`, `per_booking`, `per_hour`, `per_day`);
//! - the winning row's unit decides the mode: per person (one line per guest category, each
//!   category taking its own most specific row) or per unit (with the extra-guest surcharge
//!   above standard occupancy);
//! - discount first, then taxes on the discounted subtotal; inclusive taxes are reported but
//!   not added.

use std::cmp::Reverse;

use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{
    pricing::{Discount, PricingUnit, TaxCalcType},
    types::{
        CurrencyCode, DateRange, DaysOfWeek, Money, MoneyError, TsRange, inclusive_tax_of,
        percent_of,
    },
};

// ---- Inputs -----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PriceRule {
    pub id: Uuid,
    pub rate_plan_id: Option<Uuid>,
    pub season_id: Option<Uuid>,
    pub guest_category_id: Option<Uuid>,
    pub unit: PricingUnit,
    pub amount_minor: i64,
    pub extra_guest_amount_minor: Option<i64>,
    pub min_quantity: i32,
    pub max_quantity: Option<i32>,
    pub min_nights: Option<i32>,
    pub days_of_week: DaysOfWeek,
    pub valid_period: Option<DateRange>,
    pub priority: i32,
}

#[derive(Debug, Clone)]
pub struct SeasonRule {
    pub id: Uuid,
    pub priority: i32,
    pub periods: Vec<DateRange>,
}

#[derive(Debug, Clone)]
pub struct TaxRule {
    pub id: Uuid,
    pub code: String,
    pub calc_type: TaxCalcType,
    pub rate_percent: Option<Decimal>,
    pub amount_minor: Option<i64>,
    pub currency: Option<CurrencyCode>,
    pub is_inclusive: bool,
    pub exempt_guest_category_ids: Vec<Uuid>,
    pub valid_period: Option<DateRange>,
}

#[derive(Debug, Clone)]
pub struct PromotionRule {
    pub id: Uuid,
    pub code: Option<String>,
    pub discount: Discount,
    /// Minimum subtotal, in its own currency (never compared across currencies).
    pub min_subtotal: Option<Money>,
    pub min_nights: Option<i32>,
    pub min_days_before_arrival: Option<i32>,
    pub booking_window: Option<TsRange>,
    pub stay_window: Option<DateRange>,
    pub applies_to_all: bool,
    pub product_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Copy)]
pub struct GuestCategoryInfo {
    pub id: Uuid,
    pub counts_toward_capacity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestCount {
    pub guest_category_id: Uuid,
    pub count: i32,
}

/// When the product is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceTime {
    /// Nights `[arrival, departure)`.
    Stay {
        arrival: NaiveDate,
        departure: NaiveDate,
    },
    /// A dated service (activity, extra). `hours` / `days` feed `per_hour` / `per_day` prices.
    Date {
        date: NaiveDate,
        hours: Option<i32>,
        days: Option<i32>,
    },
}

impl ServiceTime {
    pub fn first_date(&self) -> NaiveDate {
        match *self {
            Self::Stay { arrival, .. } => arrival,
            Self::Date { date, .. } => date,
        }
    }

    pub fn nights(&self) -> i64 {
        match *self {
            Self::Stay { arrival, departure } => (departure - arrival).num_days(),
            Self::Date { .. } => 0,
        }
    }

    pub fn dates(&self) -> Vec<NaiveDate> {
        match *self {
            Self::Stay { arrival, departure } => {
                arrival.iter_days().take_while(|d| *d < departure).collect()
            }
            Self::Date { date, .. } => vec![date],
        }
    }
}

#[derive(Debug, Clone)]
pub struct PricingRequest<'a> {
    pub product_id: Uuid,
    pub rate_plan_id: Option<Uuid>,
    pub service: ServiceTime,
    pub guests: &'a [GuestCount],
    /// Units booked (pitches, tents, quads...).
    pub quantity: i32,
    /// Guests included in a unit price; above it, `extra_guest_amount_minor` applies.
    pub standard_occupancy: Option<i32>,
    pub booked_at: DateTime<Utc>,
    /// Booking date in the camp's timezone (for `min_days_before_arrival`).
    pub booked_on: NaiveDate,
}

/// Everything the engine needs, loaded beforehand (see `pricing::load_context`).
#[derive(Debug, Clone)]
pub struct PricingContext {
    pub currency: CurrencyCode,
    /// Active price rows of the product, in `currency`.
    pub prices: Vec<PriceRule>,
    pub seasons: Vec<SeasonRule>,
    /// Active taxes linked to the product.
    pub taxes: Vec<TaxRule>,
    pub guest_categories: Vec<GuestCategoryInfo>,
    pub promotion: Option<PromotionRule>,
}

// ---- Output -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Base,
    ExtraGuest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceLine {
    pub date: NaiveDate,
    pub season_id: Option<Uuid>,
    pub price_id: Uuid,
    pub unit: PricingUnit,
    pub kind: LineKind,
    pub guest_category_id: Option<Uuid>,
    pub quantity: i64,
    pub unit_amount_minor: i64,
    pub amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscountLine {
    pub promotion_id: Uuid,
    pub code: Option<String>,
    pub amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxLine {
    pub tax_id: Uuid,
    pub code: String,
    pub amount_minor: i64,
    /// Already contained in the prices (reported, not added to the total).
    pub inclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub currency: CurrencyCode,
    pub lines: Vec<PriceLine>,
    pub subtotal_minor: i64,
    pub discount: Option<DiscountLine>,
    pub discount_minor: i64,
    pub taxes: Vec<TaxLine>,
    /// Taxes added on top of the prices.
    pub tax_minor: i64,
    pub total_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("no applicable price on {date}{}", category.map(|c| format!(" for guest category {c}")).unwrap_or_default())]
    NoPrice {
        date: NaiveDate,
        category: Option<Uuid>,
    },
    #[error("promotion not applicable: {0}")]
    PromotionNotApplicable(&'static str),
    #[error("tax {0} is not in the quote currency")]
    TaxCurrency(String),
    #[error(transparent)]
    Money(#[from] MoneyError),
}

// ---- Engine -----------------------------------------------------------------------------------

pub fn quote(req: &PricingRequest<'_>, ctx: &PricingContext) -> Result<Quote, PricingError> {
    let (lines, subtotal_minor) = price_lines(req, ctx)?;
    let discount = match &ctx.promotion {
        Some(promotion) => Some(apply_promotion(
            req,
            &ctx.currency,
            promotion,
            &lines,
            subtotal_minor,
        )?),
        None => None,
    };
    let discount_minor = discount.as_ref().map_or(0, |d| d.amount_minor);
    let mut quote = finish(req, ctx, lines, subtotal_minor, discount_minor)?;
    quote.discount = discount;
    Ok(quote)
}

/// The priced lines of a request (no promotion, no tax) and their sum.
pub fn price_lines(
    req: &PricingRequest<'_>,
    ctx: &PricingContext,
) -> Result<(Vec<PriceLine>, i64), PricingError> {
    validate(req)?;
    let nights = req.service.nights();
    let mut lines = Vec::new();

    for date in req.service.dates() {
        let season_id = season_of(date, &ctx.seasons);
        let applicable: Vec<&PriceRule> = ctx
            .prices
            .iter()
            .filter(|rule| applies(rule, req, date, season_id, nights))
            .collect();
        let best = best_of(
            applicable
                .iter()
                .copied()
                .filter(|r| category_requested(r, req)),
        )
        .ok_or(PricingError::NoPrice {
            date,
            category: None,
        })?;

        if best.unit.is_per_person() {
            for guest in req.guests.iter().filter(|g| g.count > 0) {
                let rule = best_of(applicable.iter().copied().filter(|r| {
                    r.unit.is_per_person()
                        && r.guest_category_id
                            .is_none_or(|c| c == guest.guest_category_id)
                }))
                .ok_or(PricingError::NoPrice {
                    date,
                    category: Some(guest.guest_category_id),
                })?;
                lines.push(line(
                    date,
                    season_id,
                    rule,
                    LineKind::Base,
                    Some(guest.guest_category_id),
                    guest.count.into(),
                    rule.amount_minor,
                )?);
            }
        } else {
            lines.extend(unit_lines(req, ctx, date, season_id, best)?);
        }
    }
    let subtotal_minor = sum(lines.iter().map(|l| l.amount_minor))?;
    Ok((lines, subtotal_minor))
}

/// Applies an already computed discount, then the taxes on the discounted subtotal. The
/// returned quote has `discount: None` (the caller knows which promotions made the amount).
pub fn finish(
    req: &PricingRequest<'_>,
    ctx: &PricingContext,
    lines: Vec<PriceLine>,
    subtotal_minor: i64,
    discount_minor: i64,
) -> Result<Quote, PricingError> {
    let discount_minor = discount_minor.clamp(0, subtotal_minor);
    let taxable_minor = subtotal_minor - discount_minor;
    let taxes = compute_taxes(req, ctx, taxable_minor)?;
    let tax_minor = sum(taxes
        .iter()
        .filter(|t| !t.inclusive)
        .map(|t| t.amount_minor))?;

    Ok(Quote {
        currency: ctx.currency.clone(),
        lines,
        subtotal_minor,
        discount: None,
        discount_minor,
        taxes,
        tax_minor,
        total_minor: taxable_minor
            .checked_add(tax_minor)
            .ok_or(MoneyError::Overflow)?,
    })
}

fn validate(req: &PricingRequest<'_>) -> Result<(), PricingError> {
    if req.quantity < 1 {
        return Err(PricingError::InvalidRequest("quantity must be at least 1"));
    }
    if req.guests.iter().any(|g| g.count < 0) {
        return Err(PricingError::InvalidRequest(
            "guest counts cannot be negative",
        ));
    }
    if let ServiceTime::Stay { arrival, departure } = req.service
        && departure <= arrival
    {
        return Err(PricingError::InvalidRequest(
            "departure must be after arrival",
        ));
    }
    Ok(())
}

/// Highest-priority season whose period contains `date` (ties broken by id).
pub fn season_of(date: NaiveDate, seasons: &[SeasonRule]) -> Option<Uuid> {
    seasons
        .iter()
        .filter(|s| s.periods.iter().any(|p| p.contains(date)))
        .max_by_key(|s| (s.priority, Reverse(s.id)))
        .map(|s| s.id)
}

fn applies(
    rule: &PriceRule,
    req: &PricingRequest<'_>,
    date: NaiveDate,
    season: Option<Uuid>,
    nights: i64,
) -> bool {
    let nightly = matches!(
        rule.unit,
        PricingUnit::PerUnitPerNight | PricingUnit::PerPersonPerNight
    );
    nightly == matches!(req.service, ServiceTime::Stay { .. })
        && rule
            .rate_plan_id
            .is_none_or(|p| Some(p) == req.rate_plan_id)
        && rule.season_id.is_none_or(|s| Some(s) == season)
        && rule.days_of_week.contains(date)
        && rule.valid_period.is_none_or(|p| p.contains(date))
        && rule.min_nights.is_none_or(|m| nights >= i64::from(m))
        && req.quantity >= rule.min_quantity
        && rule.max_quantity.is_none_or(|m| req.quantity <= m)
}

/// A category-specific row only competes when that category is requested.
fn category_requested(rule: &PriceRule, req: &PricingRequest<'_>) -> bool {
    rule.guest_category_id.is_none_or(|c| {
        req.guests
            .iter()
            .any(|g| g.guest_category_id == c && g.count > 0)
    })
}

fn specificity(rule: &PriceRule) -> u8 {
    u8::from(rule.rate_plan_id.is_some())
        + u8::from(rule.season_id.is_some())
        + u8::from(rule.guest_category_id.is_some())
        + u8::from(rule.valid_period.is_some())
        + u8::from(!rule.days_of_week.is_every_day())
        + u8::from(rule.min_nights.is_some())
}

/// Most specific, then highest priority, then lowest id.
fn best_of<'r>(rules: impl Iterator<Item = &'r PriceRule>) -> Option<&'r PriceRule> {
    rules.max_by_key(|r| (specificity(r), r.priority, Reverse(r.id)))
}

fn unit_lines(
    req: &PricingRequest<'_>,
    ctx: &PricingContext,
    date: NaiveDate,
    season_id: Option<Uuid>,
    rule: &PriceRule,
) -> Result<Vec<PriceLine>, PricingError> {
    let qty = i64::from(req.quantity);
    let base_quantity = match (rule.unit, req.service) {
        (PricingUnit::PerUnitPerNight | PricingUnit::PerUnit, _) => qty,
        (PricingUnit::PerBooking, _) => 1,
        (PricingUnit::PerHour, ServiceTime::Date { hours: Some(h), .. }) if h > 0 => {
            qty * i64::from(h)
        }
        (PricingUnit::PerDay, ServiceTime::Date { days: Some(d), .. }) if d > 0 => {
            qty * i64::from(d)
        }
        (PricingUnit::PerHour, _) => {
            return Err(PricingError::InvalidRequest(
                "per-hour price needs a number of hours",
            ));
        }
        (PricingUnit::PerDay, _) => {
            return Err(PricingError::InvalidRequest(
                "per-day price needs a number of days",
            ));
        }
        _ => unreachable!("per-person units are priced per guest category"),
    };
    let mut lines = vec![line(
        date,
        season_id,
        rule,
        LineKind::Base,
        None,
        base_quantity,
        rule.amount_minor,
    )?];

    if let (PricingUnit::PerUnitPerNight, Some(extra), Some(standard)) = (
        rule.unit,
        rule.extra_guest_amount_minor,
        req.standard_occupancy,
    ) {
        let extra_guests = capacity_guests(req, ctx) - i64::from(standard) * qty;
        if extra_guests > 0 {
            lines.push(line(
                date,
                season_id,
                rule,
                LineKind::ExtraGuest,
                None,
                extra_guests,
                extra,
            )?);
        }
    }
    Ok(lines)
}

fn line(
    date: NaiveDate,
    season_id: Option<Uuid>,
    rule: &PriceRule,
    kind: LineKind,
    guest_category_id: Option<Uuid>,
    quantity: i64,
    unit_amount_minor: i64,
) -> Result<PriceLine, PricingError> {
    Ok(PriceLine {
        date,
        season_id,
        price_id: rule.id,
        unit: rule.unit,
        kind,
        guest_category_id,
        quantity,
        unit_amount_minor,
        amount_minor: unit_amount_minor
            .checked_mul(quantity)
            .ok_or(MoneyError::Overflow)?,
    })
}

/// Guests counting toward capacity (infants usually do not).
pub fn capacity_guests(req: &PricingRequest<'_>, ctx: &PricingContext) -> i64 {
    req.guests
        .iter()
        .filter(|g| {
            ctx.guest_categories
                .iter()
                .find(|c| c.id == g.guest_category_id)
                .is_none_or(|c| c.counts_toward_capacity)
        })
        .map(|g| i64::from(g.count))
        .sum()
}

/// Discount of one promotion on one priced request, after checking its conditions.
pub fn apply_promotion(
    req: &PricingRequest<'_>,
    currency: &CurrencyCode,
    promo: &PromotionRule,
    lines: &[PriceLine],
    subtotal_minor: i64,
) -> Result<DiscountLine, PricingError> {
    let start = req.service.first_date();
    let nights = req.service.nights();
    let not = PricingError::PromotionNotApplicable;
    if promo
        .booking_window
        .is_some_and(|w| !w.contains(req.booked_at))
    {
        return Err(not("outside the booking window"));
    }
    if promo.stay_window.is_some_and(|w| !w.contains(start)) {
        return Err(not("outside the stay window"));
    }
    if promo.min_nights.is_some_and(|m| nights < i64::from(m)) {
        return Err(not("stay too short"));
    }
    if promo
        .min_days_before_arrival
        .is_some_and(|m| (start - req.booked_on).num_days() < i64::from(m))
    {
        return Err(not("booked too late"));
    }
    if let Some(min) = &promo.min_subtotal {
        if &min.currency != currency {
            return Err(not("currency differs from the booking currency"));
        }
        if subtotal_minor < min.amount_minor {
            return Err(not("subtotal too low"));
        }
    }
    if !promo.applies_to_all && !promo.product_ids.contains(&req.product_id) {
        return Err(not("not valid for this product"));
    }

    let amount = match &promo.discount {
        Discount::Percent { percent } => percent_of(subtotal_minor, *percent)?,
        Discount::FixedAmount { amount } => {
            if &amount.currency != currency {
                return Err(not("currency differs from the booking currency"));
            }
            amount.amount_minor
        }
        Discount::FreeNights {
            nights: free_nights,
        } => {
            if nights == 0 {
                return Err(not("free nights only apply to stays"));
            }
            let free = usize::try_from(*free_nights).unwrap_or(usize::MAX);
            let mut per_night: Vec<i64> = req
                .service
                .dates()
                .iter()
                .map(|d| {
                    lines
                        .iter()
                        .filter(|l| l.date == *d)
                        .map(|l| l.amount_minor)
                        .sum()
                })
                .collect();
            per_night.sort_unstable();
            per_night.iter().take(free.min(per_night.len())).sum()
        }
    };
    Ok(DiscountLine {
        promotion_id: promo.id,
        code: promo.code.clone(),
        amount_minor: amount.min(subtotal_minor),
    })
}

fn compute_taxes(
    req: &PricingRequest<'_>,
    ctx: &PricingContext,
    taxable_minor: i64,
) -> Result<Vec<TaxLine>, PricingError> {
    let date = req.service.first_date();
    let mut lines = Vec::new();
    for tax in ctx
        .taxes
        .iter()
        .filter(|t| t.valid_period.is_none_or(|p| p.contains(date)))
    {
        let amount = match tax.calc_type {
            TaxCalcType::Percent => {
                let rate = tax.rate_percent.unwrap_or_default();
                if tax.is_inclusive {
                    inclusive_tax_of(taxable_minor, rate)?
                } else {
                    percent_of(taxable_minor, rate)?
                }
            }
            fixed => {
                if tax.currency.as_ref() != Some(&ctx.currency) {
                    return Err(PricingError::TaxCurrency(tax.code.clone()));
                }
                let unit = tax.amount_minor.unwrap_or_default();
                let count = match fixed {
                    TaxCalcType::FixedPerPersonPerNight => {
                        let taxable_guests: i64 = req
                            .guests
                            .iter()
                            .filter(|g| {
                                !tax.exempt_guest_category_ids.contains(&g.guest_category_id)
                            })
                            .map(|g| i64::from(g.count))
                            .sum();
                        taxable_guests * req.service.nights()
                    }
                    TaxCalcType::FixedPerBooking => 1,
                    TaxCalcType::FixedPerUnit => i64::from(req.quantity),
                    TaxCalcType::Percent => unreachable!(),
                };
                unit.checked_mul(count).ok_or(MoneyError::Overflow)?
            }
        };
        if amount > 0 {
            lines.push(TaxLine {
                tax_id: tax.id,
                code: tax.code.clone(),
                amount_minor: amount,
                inclusive: tax.is_inclusive,
            });
        }
    }
    Ok(lines)
}

fn sum(mut amounts: impl Iterator<Item = i64>) -> Result<i64, MoneyError> {
    amounts.try_fold(0_i64, |acc, a| {
        acc.checked_add(a).ok_or(MoneyError::Overflow)
    })
}
