//! Shop dashboard and reports (`shop.read`): days, products and categories in
//! Africa/Casablanca time, CSV export.

use axum::{
    Json,
    extract::{Query, State},
    http::header,
    response::{IntoResponse, Response},
};
use chrono::{Duration, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{auth::Staff, error::AppResult, state::AppState};

pub async fn dashboard(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let today = crate::camp_time::today(Utc::now());
    let soon = today + Duration::days(30);
    let threshold =
        crate::services::settings::number(&state.db, "shop.low_stock_threshold", 5).await?;
    let c = sqlx::query!(
        r#"SELECT
              (SELECT count(*) FROM orders WHERE requires_confirmation AND confirmed_at IS NULL AND status = 'pending') AS "to_confirm!",
              (SELECT count(*) FROM orders WHERE status = 'open' AND fulfillment_status IN ('unfulfilled', 'partially_fulfilled')) AS "to_fulfil!",
              (SELECT count(*) FROM fulfillments WHERE status = 'ready_for_pickup') AS "ready_for_pickup!",
              (SELECT count(*) FROM orders WHERE risk_level <> 'normal' AND status IN ('pending', 'open')) AS "at_risk!",
              (SELECT count(*) FROM order_returns WHERE status IN ('requested', 'approved')) AS "returns_open!",
              (SELECT count(*) FROM product_variants v WHERE v.track_inventory AND v.deleted_at IS NULL AND v.status = 'active'
                 AND COALESCE((SELECT sum(on_hand - reserved) FROM inventory_levels l WHERE l.variant_id = v.id), 0)
                     <= COALESCE(v.low_stock_threshold, $3::int)) AS "low_stock!",
              (SELECT count(*) FROM inventory_lots WHERE quantity > 0 AND best_before <= $2) AS "lots_expiring!",
              (SELECT COALESCE(sum(total_minor), 0) FROM orders
                 WHERE status IN ('open', 'completed') AND (placed_at AT TIME ZONE 'Africa/Casablanca')::date = $1)::bigint AS "sales_today!",
              (SELECT count(*) FROM orders
                 WHERE status IN ('open', 'completed') AND (placed_at AT TIME ZONE 'Africa/Casablanca')::date = $1) AS "orders_today!""#,
        today,
        soon,
        i32::try_from(threshold).unwrap_or(5)
    )
    .fetch_one(&state.db)
    .await?;
    let s = super::super::public::shop_settings(&state).await?;
    let stripe_warning = s.payments.stripe_enabled && !state.shop_payments.stripe_configured();
    Ok(Json(json!({
        "to_confirm": c.to_confirm, "to_fulfil": c.to_fulfil, "ready_for_pickup": c.ready_for_pickup, "at_risk": c.at_risk,
        "returns_open": c.returns_open, "low_stock": c.low_stock, "lots_expiring": c.lots_expiring,
        "sales_today_minor": c.sales_today, "orders_today": c.orders_today, "currency": s.currency,
        "shop_status": s.status, "stripe": { "enabled": s.payments.stripe_enabled, "configured": state.shop_payments.stripe_configured(),
                                              "warning": stripe_warning },
    })))
}

#[derive(Debug, Deserialize)]
pub struct Range {
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    #[serde(default)]
    pub format: Option<String>,
}

pub async fn report(
    State(state): State<AppState>,
    staff: Staff,
    Query(r): Query<Range>,
) -> AppResult<Response> {
    staff.require("shop.read")?;
    let to = r.to.unwrap_or_else(|| crate::camp_time::today(Utc::now()));
    let from = r.from.unwrap_or(to - Duration::days(29));
    let days = sqlx::query!(
        r#"SELECT d::date AS "day!",
                  COALESCE(sum(o.total_minor), 0)::bigint AS "sales!", count(o.id) AS "orders!",
                  COALESCE(sum(o.amount_refunded_minor), 0)::bigint AS "refunded!", COALESCE(sum(o.tax_minor), 0)::bigint AS "tax!",
                  COALESCE(sum(o.shipping_minor - o.shipping_discount_minor), 0)::bigint AS "shipping!"
           FROM generate_series($1::date, $2::date, interval '1 day') d
           LEFT JOIN orders o ON (o.placed_at AT TIME ZONE 'Africa/Casablanca')::date = d::date AND o.status IN ('open', 'completed')
           GROUP BY d ORDER BY d"#,
        from,
        to
    )
    .fetch_all(&state.db)
    .await?;
    let products = sqlx::query!(
        r#"SELECT oi.product_id, max(oi.product_snapshot->>'name') AS "name!", sum(oi.quantity)::bigint AS "units!",
                  sum(oi.total_minor)::bigint AS "sales!",
                  sum(COALESCE((oi.product_snapshot->>'cost_minor')::bigint, 0) * oi.quantity)::bigint AS "cost!"
           FROM order_items oi JOIN orders o ON o.id = oi.order_id
           WHERE o.status IN ('open', 'completed') AND (o.placed_at AT TIME ZONE 'Africa/Casablanca')::date BETWEEN $1 AND $2
           GROUP BY oi.product_id ORDER BY 4 DESC LIMIT 50"#,
        from,
        to
    )
    .fetch_all(&state.db)
    .await?;
    let categories = sqlx::query!(
        r#"SELECT COALESCE(ct.name, 'Sans catégorie') AS "name!", sum(oi.total_minor)::bigint AS "sales!", sum(oi.quantity)::bigint AS "units!"
           FROM order_items oi JOIN orders o ON o.id = oi.order_id
           LEFT JOIN LATERAL (SELECT c.id FROM product_categories pc JOIN categories c ON c.id = pc.category_id AND c.scope = 'shop'
                              WHERE pc.product_id = oi.product_id ORDER BY c.sort_order LIMIT 1) cat ON true
           LEFT JOIN category_translations ct ON ct.category_id = cat.id AND ct.locale = (SELECT code FROM locales WHERE is_default)
           WHERE o.status IN ('open', 'completed') AND (o.placed_at AT TIME ZONE 'Africa/Casablanca')::date BETWEEN $1 AND $2
           GROUP BY 1 ORDER BY 2 DESC"#,
        from,
        to
    )
    .fetch_all(&state.db)
    .await?;
    let stock_value = sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(GREATEST(l.on_hand, 0) * COALESCE(v.cost_minor, 0)), 0)::bigint AS "x!"
           FROM inventory_levels l JOIN product_variants v ON v.id = l.variant_id AND v.deleted_at IS NULL"#
    )
    .fetch_one(&state.db)
    .await?;
    let total_sales: i64 = days.iter().map(|d| d.sales).sum();
    let total_orders: i64 = days.iter().map(|d| d.orders).sum();
    let can_cost = staff.can("shop.pricing");
    if r.format.as_deref() == Some("csv") {
        let mut out =
            String::from("day,orders,sales_minor,refunded_minor,tax_minor,shipping_minor\n");
        for d in &days {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                d.day, d.orders, d.sales, d.refunded, d.tax, d.shipping
            ));
        }
        return Ok((
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"shop-sales.csv\"",
                ),
            ],
            out,
        )
            .into_response());
    }
    Ok(Json(json!({
        "from": from, "to": to,
        "totals": { "sales_minor": total_sales, "orders": total_orders,
                    "average_order_minor": if total_orders > 0 { total_sales / total_orders } else { 0 },
                    "refunded_minor": days.iter().map(|d| d.refunded).sum::<i64>(),
                    "stock_value_minor": if can_cost { Some(stock_value) } else { None } },
        "days": days.iter().map(|d| json!({ "day": d.day, "orders": d.orders, "sales_minor": d.sales, "refunded_minor": d.refunded,
                                              "tax_minor": d.tax, "shipping_minor": d.shipping })).collect::<Vec<_>>(),
        "products": products.iter().map(|p| json!({ "product_id": p.product_id, "name": p.name, "units": p.units, "sales_minor": p.sales,
                                                      "margin_minor": if can_cost { Some(p.sales - p.cost) } else { None } })).collect::<Vec<_>>(),
        "categories": categories.iter().map(|c| json!({ "name": c.name, "sales_minor": c.sales, "units": c.units })).collect::<Vec<_>>(),
    }))
    .into_response())
}
