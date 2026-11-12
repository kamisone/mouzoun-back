//! Prices a resolved trip with the existing engine, then applies promotions at trip level.
//!
//! Rounding rule (documented in docs/booking/README.md and tested):
//! - every engine line is `unit amount × quantity` (exact integers, cents);
//! - percentages (promotion, percent tax, deposit) are rounded once, half away from zero, per
//!   item (deposit: once on the booking total);
//! - a fixed-amount promotion is split across the eligible items in proportion to their
//!   subtotals, with the largest-remainder method, so the parts add up exactly;
//! - item total = subtotal − discount + exclusive taxes; trip totals are sums of item totals.

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    catalog::RatePlan,
    model::{AppliedDiscount, CancellationStep, PaymentChoice, PromoStatus},
};
use crate::{
    camp_time,
    db::{
        bookings::PaymentOption,
        catalog::{PackagePricingMode, ProductKind},
        pricing::{Deposit, Discount, PaymentMode},
        types::{CurrencyCode, percent_of},
    },
    services::{
        booking::BookingError,
        pricing::{
            self, GuestCount, PricingRequest, ServiceTime,
            engine::{PriceLine, PromotionRule, Quote},
        },
    },
};

/// Check-in time used when an accommodation has none (cancellation deadlines).
pub const DEFAULT_CHECK_IN: NaiveTime = match NaiveTime::from_hms_opt(14, 0, 0) {
    Some(t) => t,
    None => unreachable!(),
};

/// What the pricing step needs to know about one resolved item.
#[derive(Debug, Clone)]
pub struct PricingInput {
    pub product_id: Uuid,
    pub kind: ProductKind,
    pub service: ServiceTime,
    pub guests: Vec<GuestCount>,
    pub quantity: i32,
    pub standard_occupancy: Option<i32>,
    /// Per-person extras bought several times (breakfast × 3 mornings): lines are repeated.
    pub repeat: i32,
    pub in_package: bool,
    pub package: Option<(PackagePricingMode, Decimal)>,
    pub parent: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Priced {
    pub lines: Vec<PriceLine>,
    pub subtotal_minor: i64,
    pub discounts: Vec<AppliedDiscount>,
    pub quote: Quote,
}

/// Engine lines of one item (no promotion, no tax).
pub async fn base_lines(
    conn: &mut PgConnection,
    input: &PricingInput,
    rate_plan_id: Option<Uuid>,
    currency: &CurrencyCode,
    now: DateTime<Utc>,
) -> Result<(Vec<PriceLine>, i64), BookingError> {
    if input.in_package {
        return Ok((vec![], 0));
    }
    let (from, to) = match input.service {
        ServiceTime::Stay { arrival, departure } => (arrival, departure),
        ServiceTime::Date { date, .. } => (date, date.succ_opt().unwrap_or(date)),
    };
    let ctx = pricing::load_context(conn, input.product_id, currency, from, to, None).await?;
    let req = request(input, rate_plan_id, now);
    let (mut lines, mut subtotal) = pricing::engine::price_lines(&req, &ctx)?;
    if input.repeat > 1 {
        for line in &mut lines {
            line.quantity *= i64::from(input.repeat);
            line.amount_minor *= i64::from(input.repeat);
        }
        subtotal *= i64::from(input.repeat);
    }
    Ok((lines, subtotal))
}

pub fn request<'a>(
    input: &'a PricingInput,
    rate_plan_id: Option<Uuid>,
    now: DateTime<Utc>,
) -> PricingRequest<'a> {
    PricingRequest {
        product_id: input.product_id,
        rate_plan_id,
        service: input.service,
        guests: &input.guests,
        quantity: input.quantity,
        standard_occupancy: input.standard_occupancy,
        booked_at: now,
        booked_on: camp_time::today(now),
    }
}

/// Tax and totals of one item once its discount is known.
#[allow(clippy::too_many_arguments)]
pub async fn finish_item(
    conn: &mut PgConnection,
    input: &PricingInput,
    rate_plan_id: Option<Uuid>,
    currency: &CurrencyCode,
    lines: Vec<PriceLine>,
    subtotal_minor: i64,
    discount_minor: i64,
    now: DateTime<Utc>,
) -> Result<Quote, BookingError> {
    let (from, to) = match input.service {
        ServiceTime::Stay { arrival, departure } => (arrival, departure),
        ServiceTime::Date { date, .. } => (date, date.succ_opt().unwrap_or(date)),
    };
    let ctx = pricing::load_context(conn, input.product_id, currency, from, to, None).await?;
    let req = request(input, rate_plan_id, now);
    Ok(pricing::engine::finish(
        &req,
        &ctx,
        lines,
        subtotal_minor,
        discount_minor,
    )?)
}

// ---- Promotions -------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PromotionMeta {
    pub name: Option<String>,
    pub is_stackable: bool,
    pub exhausted: bool,
}

pub async fn promotion_meta(
    conn: &mut PgConnection,
    ids: &[Uuid],
    locale: &str,
) -> Result<HashMap<Uuid, PromotionMeta>, BookingError> {
    Ok(sqlx::query!(
        r#"SELECT p.id, p.is_stackable, COALESCE(t.name, d.name) AS "name?",
                  (p.max_redemptions IS NOT NULL AND p.max_redemptions <= (
                      SELECT count(*) FROM promotion_redemptions r JOIN bookings b ON b.id = r.booking_id
                      WHERE r.promotion_id = p.id AND b.status NOT IN ('cancelled', 'expired'))) AS "exhausted!"
           FROM promotions p
           LEFT JOIN promotion_translations t ON t.promotion_id = p.id AND t.locale = $2
           LEFT JOIN promotion_translations d ON d.promotion_id = p.id
                 AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE p.id = ANY($1)"#,
        ids,
        locale
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.id,
            PromotionMeta {
                name: r.name,
                is_stackable: r.is_stackable,
                exhausted: r.exhausted,
            },
        )
    })
    .collect())
}

/// The trip as the promotion rules see it.
pub struct TripFacts<'a> {
    pub currency: &'a CurrencyCode,
    pub starts_on: Option<NaiveDate>,
    pub stay_nights: i64,
    pub booked_at: DateTime<Utc>,
    pub booked_on: NaiveDate,
}

/// Per-item discount amounts of one promotion (index → cents), or why it does not apply.
pub fn evaluate(
    promo: &PromotionRule,
    inputs: &[PricingInput],
    base: &[(Vec<PriceLine>, i64)],
    trip: &TripFacts<'_>,
) -> Result<Vec<(usize, i64)>, &'static str> {
    if promo
        .booking_window
        .is_some_and(|w| !w.contains(trip.booked_at))
    {
        return Err("outside_booking_window");
    }
    let Some(start) = trip.starts_on else {
        return Err("not_applicable");
    };
    if promo.stay_window.is_some_and(|w| !w.contains(start)) {
        return Err("outside_stay_window");
    }
    if promo
        .min_nights
        .is_some_and(|m| trip.stay_nights < i64::from(m))
    {
        return Err("stay_too_short");
    }
    if promo
        .min_days_before_arrival
        .is_some_and(|m| (start - trip.booked_on).num_days() < i64::from(m))
    {
        return Err("booked_too_late");
    }
    let eligible: Vec<usize> = inputs
        .iter()
        .enumerate()
        .filter(|(i, input)| {
            base[*i].1 > 0
                && (promo.applies_to_all || promo.product_ids.contains(&input.product_id))
        })
        .map(|(i, _)| i)
        .collect();
    if eligible.is_empty() {
        return Err("not_valid_for_items");
    }
    let eligible_subtotal: i64 = eligible.iter().map(|i| base[*i].1).sum();
    if let Some(min) = &promo.min_subtotal {
        if &min.currency != trip.currency {
            return Err("not_applicable");
        }
        if eligible_subtotal < min.amount_minor {
            return Err("subtotal_too_low");
        }
    }
    let amounts: Vec<(usize, i64)> = match &promo.discount {
        Discount::Percent { percent } => eligible
            .iter()
            .map(|i| {
                Ok((
                    *i,
                    percent_of(base[*i].1, *percent).map_err(|_| "not_applicable")?,
                ))
            })
            .collect::<Result<_, &'static str>>()?,
        Discount::FixedAmount { amount } => {
            if &amount.currency != trip.currency {
                return Err("not_applicable");
            }
            let total = amount.amount_minor.min(eligible_subtotal);
            let weights: Vec<i64> = eligible.iter().map(|i| base[*i].1).collect();
            eligible
                .iter()
                .copied()
                .zip(allocate(total, &weights))
                .collect()
        }
        Discount::FreeNights { nights } => {
            let free = usize::try_from(*nights).unwrap_or(usize::MAX);
            let stays: Vec<(usize, i64)> = eligible
                .iter()
                .filter(|i| matches!(inputs[**i].service, ServiceTime::Stay { .. }))
                .map(|i| {
                    let mut per_night: Vec<i64> = inputs[*i]
                        .service
                        .dates()
                        .iter()
                        .map(|d| {
                            base[*i]
                                .0
                                .iter()
                                .filter(|l| l.date == *d)
                                .map(|l| l.amount_minor)
                                .sum()
                        })
                        .collect();
                    per_night.sort_unstable();
                    (*i, per_night.iter().take(free.min(per_night.len())).sum())
                })
                .collect();
            if stays.is_empty() {
                return Err("stays_only");
            }
            stays
        }
    };
    Ok(amounts.into_iter().filter(|(_, a)| *a > 0).collect())
}

/// Splits `total` across `weights` proportionally (largest remainder): parts sum to `total`.
pub fn allocate(total: i64, weights: &[i64]) -> Vec<i64> {
    let sum: i64 = weights.iter().sum();
    if sum <= 0 || weights.is_empty() {
        return vec![0; weights.len()];
    }
    let (total, sum) = (i128::from(total), i128::from(sum));
    let mut parts: Vec<(usize, i128, i128)> = weights
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let exact = total * i128::from(*w);
            (i, exact / sum, exact % sum)
        })
        .collect();
    let mut left = total - parts.iter().map(|p| p.1).sum::<i128>();
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by(|a, b| parts[*b].2.cmp(&parts[*a].2).then(a.cmp(b)));
    for i in order {
        if left <= 0 {
            break;
        }
        parts[i].1 += 1;
        left -= 1;
    }
    parts
        .into_iter()
        .map(|p| i64::try_from(p.1).unwrap_or(i64::MAX))
        .collect()
}

/// A promotion with its discount per item index.
pub type Chosen = (PromotionRule, Vec<(usize, i64)>);

/// Chooses the promotions to apply: every applicable stackable offer together, or the best
/// single non-stackable one, whichever saves more. Returns the chosen ones with their per-item
/// amounts, and the status of the code the guest typed.
pub fn choose(
    automatic: &[PromotionRule],
    code: Option<(&str, Option<&PromotionRule>)>,
    meta: &HashMap<Uuid, PromotionMeta>,
    inputs: &[PricingInput],
    base: &[(Vec<PriceLine>, i64)],
    trip: &TripFacts<'_>,
) -> (Vec<Chosen>, Option<PromoStatus>) {
    let mut applicable: Vec<(Chosen, i64)> = Vec::new();
    let mut code_status = None;
    for promo in automatic {
        if meta.get(&promo.id).is_some_and(|m| m.exhausted) {
            continue;
        }
        if let Ok(amounts) = evaluate(promo, inputs, base, trip) {
            let total = amounts.iter().map(|a| a.1).sum();
            if total > 0 {
                applicable.push(((promo.clone(), amounts), total));
            }
        }
    }
    let mut code_id = None;
    if let Some((typed, rule)) = code {
        match rule {
            None => {
                code_status = Some(PromoStatus {
                    code: typed.to_string(),
                    status: "invalid".into(),
                    reason: Some("unknown_code".into()),
                });
            }
            Some(rule) if meta.get(&rule.id).is_some_and(|m| m.exhausted) => {
                code_status = Some(PromoStatus {
                    code: typed.to_string(),
                    status: "invalid".into(),
                    reason: Some("fully_redeemed".into()),
                });
            }
            Some(rule) => match evaluate(rule, inputs, base, trip) {
                Ok(amounts) if amounts.iter().any(|a| a.1 > 0) => {
                    let total = amounts.iter().map(|a| a.1).sum();
                    code_id = Some(rule.id);
                    applicable.push(((rule.clone(), amounts), total));
                }
                Ok(_) => {
                    code_status = Some(PromoStatus {
                        code: typed.to_string(),
                        status: "invalid".into(),
                        reason: Some("not_valid_for_items".into()),
                    });
                }
                Err(reason) => {
                    code_status = Some(PromoStatus {
                        code: typed.to_string(),
                        status: "invalid".into(),
                        reason: Some(reason.into()),
                    });
                }
            },
        }
    }
    let stackable = |p: &PromotionRule| meta.get(&p.id).is_some_and(|m| m.is_stackable);
    let stack_total: i64 = applicable
        .iter()
        .filter(|a| stackable(&a.0.0))
        .map(|a| a.1)
        .sum();
    let best_single = applicable
        .iter()
        .filter(|a| !stackable(&a.0.0))
        .max_by_key(|a| (a.1, Some(a.0.0.id) == code_id));
    let chosen: Vec<Chosen> = match best_single {
        Some(single) if single.1 > stack_total => vec![single.0.clone()],
        _ => applicable
            .iter()
            .filter(|a| stackable(&a.0.0))
            .map(|a| a.0.clone())
            .collect(),
    };
    if let (Some(id), Some((typed, _))) = (code_id, code) {
        code_status = Some(PromoStatus {
            code: typed.to_string(),
            status: if chosen.iter().any(|c| c.0.id == id) {
                "applied".into()
            } else {
                "not_combinable".into()
            },
            reason: None,
        });
    }
    (chosen, code_status)
}

// ---- Payment options and cancellation -----------------------------------------------------------

/// Deposit of a total under a rate plan (cents, never above the total).
pub fn deposit_of(deposit: &Deposit, total_minor: i64, currency: &CurrencyCode) -> Option<i64> {
    match deposit {
        Deposit::None => None,
        Deposit::Full => Some(total_minor),
        Deposit::Percent { percent } => percent_of(total_minor, *percent).ok(),
        Deposit::Fixed { amount } => {
            (&amount.currency == currency).then_some(amount.amount_minor.min(total_minor))
        }
    }
}

pub fn payment_options(
    plan: Option<&RatePlan>,
    total_minor: i64,
    currency: &CurrencyCode,
    starts_on: Option<NaiveDate>,
    today: NaiveDate,
    balance_due_days: i64,
) -> Vec<PaymentChoice> {
    let on_site = PaymentChoice {
        option: PaymentOption::OnSite,
        due_now_minor: 0,
        due_later_minor: total_minor,
        due_later_on: None,
    };
    let Some(plan) = plan else {
        return vec![on_site];
    };
    if total_minor == 0 {
        return vec![on_site];
    }
    let full = PaymentChoice {
        option: PaymentOption::Full,
        due_now_minor: total_minor,
        due_later_minor: 0,
        due_later_on: None,
    };
    let balance_due_on = starts_on.map(|s| s - Duration::days(balance_due_days));
    let deposit = deposit_of(&plan.deposit, total_minor, currency)
        .filter(|d| *d > 0 && *d < total_minor)
        .filter(|_| balance_due_on.is_some_and(|due| due > today))
        .map(|d| PaymentChoice {
            option: PaymentOption::Deposit,
            due_now_minor: d,
            due_later_minor: total_minor - d,
            due_later_on: balance_due_on,
        });
    match plan.payment_mode {
        PaymentMode::PayOnSite => vec![on_site],
        PaymentMode::OptionalOnline => [Some(full), deposit, Some(on_site)]
            .into_iter()
            .flatten()
            .collect(),
        PaymentMode::OnlineRequired => [Some(full), deposit].into_iter().flatten().collect(),
    }
}

/// `{min_hours_before, refund_percent}` rules, most generous first.
pub fn rules(policy: Option<&serde_json::Value>) -> Vec<(i64, i64)> {
    let mut rules: Vec<(i64, i64)> = policy
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some((
                        r.get("min_hours_before")?.as_i64()?,
                        r.get("refund_percent")?.as_i64()?.clamp(0, 100),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    rules.sort_by_key(|r| std::cmp::Reverse(r.0));
    rules
}

/// Deadlines in absolute time (the camp's local check-in or activity start).
pub fn cancellation_steps(
    policy: Option<&serde_json::Value>,
    start: Option<DateTime<Utc>>,
) -> Vec<CancellationStep> {
    let Some(start) = start else { return vec![] };
    let rules = rules(policy);
    let mut steps: Vec<CancellationStep> = rules
        .iter()
        .map(|(hours, percent)| CancellationStep {
            refund_percent: *percent,
            until: Some(start - Duration::hours(*hours)),
        })
        .collect();
    if steps.last().is_none_or(|s| s.refund_percent > 0) && !steps.is_empty() {
        steps.push(CancellationStep {
            refund_percent: 0,
            until: None,
        });
    }
    steps
}

/// Refund percentage that applies when cancelling at `now` for a service starting at `start`.
pub fn refund_percent(
    policy: Option<&serde_json::Value>,
    start: DateTime<Utc>,
    now: DateTime<Utc>,
) -> i64 {
    let rules = rules(policy);
    if rules.is_empty() {
        // No policy: free cancellation until the service starts.
        return if now < start { 100 } else { 0 };
    }
    let hours_before = (start - now).num_minutes().div_euclid(60);
    if now >= start {
        return 0;
    }
    rules
        .iter()
        .find(|(min, _)| hours_before >= *min)
        .map_or(0, |(_, p)| *p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    #[test]
    fn allocation_is_proportional_and_exact() {
        assert_eq!(allocate(1000, &[3000, 1000]), vec![750, 250]);
        assert_eq!(allocate(100, &[1, 1, 1]), vec![34, 33, 33]);
        assert_eq!(allocate(1, &[5, 5]), vec![1, 0]);
        assert_eq!(allocate(0, &[5, 5]), vec![0, 0]);
        let parts = allocate(999, &[123, 456, 789, 1]);
        assert_eq!(parts.iter().sum::<i64>(), 999);
    }

    #[test]
    fn refunds_follow_the_policy_boundaries_exactly() {
        let policy = json!([
            {"min_hours_before": 168, "refund_percent": 100},
            {"min_hours_before": 48, "refund_percent": 50},
            {"min_hours_before": 0, "refund_percent": 0}
        ]);
        let start = Utc.with_ymd_and_hms(2027, 1, 20, 14, 0, 0).unwrap();
        let at = |h: i64, m: i64| start - Duration::hours(h) - Duration::minutes(m);
        assert_eq!(
            refund_percent(Some(&policy), start, at(168, 0)),
            100,
            "exactly 7 days"
        );
        assert_eq!(
            refund_percent(Some(&policy), start, at(167, 59)),
            50,
            "one minute late"
        );
        assert_eq!(refund_percent(Some(&policy), start, at(48, 0)), 50);
        assert_eq!(refund_percent(Some(&policy), start, at(47, 59)), 0);
        assert_eq!(refund_percent(Some(&policy), start, start), 0);
        assert_eq!(refund_percent(None, start, at(1, 0)), 100);

        let steps = cancellation_steps(Some(&policy), Some(start));
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].until, Some(at(168, 0)));
        assert_eq!(steps[1].refund_percent, 50);
        assert_eq!(steps[2].until, Some(start));
    }
}
