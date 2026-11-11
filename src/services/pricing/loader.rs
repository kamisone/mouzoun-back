//! Loads the pricing rules of one product from the database into a `PricingContext`.

use std::collections::HashMap;

use chrono::NaiveDate;
use sqlx::PgConnection;
use uuid::Uuid;

use super::engine::{
    GuestCategoryInfo, PriceRule, PricingContext, PromotionRule, SeasonRule, TaxRule,
};
use crate::db::{
    pricing::{Discount, DiscountType, PricingUnit, TaxCalcType},
    types::{CurrencyCode, DateRange, DaysOfWeek, Money, TsRange},
};

/// Rules applicable to `product_id` for service dates in `[from, to)`, in `currency`.
/// An unknown, inactive or exhausted-by-code promotion is returned as `None`: the caller decides
/// whether that is an error.
pub async fn load_context(
    conn: &mut PgConnection,
    product_id: Uuid,
    currency: &CurrencyCode,
    from: NaiveDate,
    to: NaiveDate,
    promo_code: Option<&str>,
) -> sqlx::Result<PricingContext> {
    let prices = sqlx::query!(
        r#"SELECT id, rate_plan_id, season_id, guest_category_id,
                  pricing_unit AS "pricing_unit: PricingUnit", amount_minor, extra_guest_amount_minor,
                  min_quantity, max_quantity, min_nights, days_of_week,
                  valid_period AS "valid_period: DateRange", priority
           FROM prices
           WHERE product_id = $1 AND is_active AND currency = $2"#,
        product_id,
        currency.as_str()
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|p| PriceRule {
        id: p.id,
        rate_plan_id: p.rate_plan_id,
        season_id: p.season_id,
        guest_category_id: p.guest_category_id,
        unit: p.pricing_unit,
        amount_minor: p.amount_minor,
        extra_guest_amount_minor: p.extra_guest_amount_minor,
        min_quantity: p.min_quantity,
        max_quantity: p.max_quantity,
        min_nights: p.min_nights,
        days_of_week: DaysOfWeek(p.days_of_week),
        valid_period: p.valid_period,
        priority: p.priority,
    })
    .collect();

    let mut seasons: HashMap<Uuid, SeasonRule> = HashMap::new();
    for row in sqlx::query!(
        r#"SELECT s.id, s.priority, sp.period AS "period: DateRange"
           FROM seasons s
           JOIN season_periods sp ON sp.season_id = s.id
           WHERE sp.period && daterange($1, $2)"#,
        from,
        to
    )
    .fetch_all(&mut *conn)
    .await?
    {
        seasons
            .entry(row.id)
            .or_insert_with(|| SeasonRule {
                id: row.id,
                priority: row.priority,
                periods: vec![],
            })
            .periods
            .push(row.period);
    }

    let taxes = sqlx::query!(
        r#"SELECT t.id, t.code::text AS "code!", t.calc_type AS "calc_type: TaxCalcType", t.rate_percent,
                  t.amount_minor, t.currency AS "currency: CurrencyCode", t.is_inclusive,
                  t.exempt_guest_category_ids, t.valid_period AS "valid_period: DateRange"
           FROM taxes t
           JOIN product_taxes pt ON pt.tax_id = t.id
           WHERE pt.product_id = $1 AND t.is_active
           ORDER BY t.code"#,
        product_id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|t| TaxRule {
        id: t.id,
        code: t.code,
        calc_type: t.calc_type,
        rate_percent: t.rate_percent,
        amount_minor: t.amount_minor,
        currency: t.currency,
        is_inclusive: t.is_inclusive,
        exempt_guest_category_ids: t.exempt_guest_category_ids,
        valid_period: t.valid_period,
    })
    .collect();

    let guest_categories =
        sqlx::query!("SELECT id, counts_toward_capacity FROM guest_categories WHERE is_active")
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|g| GuestCategoryInfo {
                id: g.id,
                counts_toward_capacity: g.counts_toward_capacity,
            })
            .collect();

    let promotion = match promo_code {
        Some(code) => load_promotion(conn, code).await?,
        None => None,
    };

    Ok(PricingContext {
        currency: currency.clone(),
        prices,
        seasons: seasons.into_values().collect(),
        taxes,
        guest_categories,
        promotion,
    })
}

async fn load_promotion(
    conn: &mut PgConnection,
    code: &str,
) -> sqlx::Result<Option<PromotionRule>> {
    let Some(p) = sqlx::query!(
        r#"SELECT id, code::text AS "code", discount_type AS "discount_type: DiscountType",
                  discount_percent, discount_amount_minor, discount_free_nights,
                  currency AS "currency: CurrencyCode", min_subtotal_minor, min_nights,
                  min_days_before_arrival, booking_window AS "booking_window: TsRange",
                  stay_window AS "stay_window: DateRange", applies_to_all
           FROM promotions
           WHERE code = $1::text::citext AND is_active"#,
        code
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let product_ids = sqlx::query_scalar!(
        "SELECT product_id FROM promotion_products WHERE promotion_id = $1",
        p.id
    )
    .fetch_all(&mut *conn)
    .await?;
    let discount = Discount::from_columns(
        p.discount_type,
        p.discount_percent,
        p.discount_amount_minor,
        p.currency.clone(),
        p.discount_free_nights,
    )
    .ok_or_else(|| {
        sqlx::Error::Decode(format!("promotion {}: inconsistent discount", p.id).into())
    })?;
    let min_subtotal = match (p.min_subtotal_minor, p.currency) {
        (Some(amount), Some(currency)) => Some(Money::new(amount, currency)),
        (None, _) => None,
        (Some(_), None) => {
            return Err(sqlx::Error::Decode(
                format!("promotion {}: min_subtotal_minor without currency", p.id).into(),
            ));
        }
    };
    Ok(Some(PromotionRule {
        id: p.id,
        code: p.code,
        discount,
        min_subtotal,
        min_nights: p.min_nights,
        min_days_before_arrival: p.min_days_before_arrival,
        booking_window: p.booking_window,
        stay_window: p.stay_window,
        applies_to_all: p.applies_to_all,
        product_ids,
    }))
}
