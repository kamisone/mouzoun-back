//! Shop settings (`shop.settings`) and the Stripe status panel. Stripe keys live only in the
//! environment: never read, stored or returned here (the status says configured / test / live).

use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::{
    auth::Staff,
    config::StripeMode,
    error::{AppError, AppResult},
    services::{
        audit,
        outbox::{self, Event},
    },
    state::AppState,
};

/// Keys editable here, with their expected JSON type.
const EDITABLE: [(&str, &str); 42] = [
    ("shop.status", "string"),
    ("shop.prices_include_tax", "boolean"),
    ("shop.guest_checkout", "boolean"),
    ("shop.checkout_hold_minutes", "number"),
    ("shop.cart_ttl_days", "number"),
    ("shop.low_stock_threshold", "number"),
    ("shop.min_order_minor", "number"),
    ("shop.max_quantity_per_line", "number"),
    ("shop.ships_to_countries", "array"),
    ("shop.carriers", "array"),
    ("shop.withdrawal_days", "number"),
    ("shop.stay_delivery_enabled", "boolean"),
    ("shop.stay_delivery_fee_minor", "number"),
    ("shop.abandoned_cart_hours", "number"),
    ("shop.abandoned_cart_requires_consent", "boolean"),
    ("shop.notify_emails", "array"),
    ("shop.route_prefix", "object"),
    ("shop.refund_deadline_days", "number"),
    ("shop.legal_guarantee_years", "number"),
    ("shop.consumer_mediator", "object"),
    ("shop.checkout_button_label", "object"),
    ("payments.allow_cash_fr", "boolean"),
    ("tax.oss_enabled", "boolean"),
    ("tax.keep_gross_price_constant", "boolean"),
    ("customs.ioss_enabled", "boolean"),
    ("customs.ioss_number", "string"),
    ("legal.cgv_page_id", "nullable"),
    ("legal.notice_page_id", "nullable"),
    ("legal.privacy_page_id", "nullable"),
    ("legal.cookies_page_id", "nullable"),
    ("legal.withdrawal_page_id", "nullable"),
    ("payments.stripe.enabled", "boolean"),
    ("payments.stripe.checkout_mode", "string"),
    ("payments.stripe.statement_descriptor", "string"),
    ("payments.cod.enabled", "boolean"),
    ("payments.cod.max_order_minor", "nullable"),
    ("payments.pay_at_pickup.enabled", "boolean"),
    ("payments.bank_transfer.enabled", "boolean"),
    ("payments.bank_transfer.details", "object"),
    ("payments.bank_transfer.hold_days", "number"),
    ("legal.shipping_page_id", "nullable"),
    ("privacy.data_retention", "object"),
];

async fn stripe_status(state: &AppState) -> AppResult<Value> {
    let last = sqlx::query_scalar!(
        "SELECT max(received_at) FROM payment_webhook_events WHERE provider = 'stripe'"
    )
    .fetch_one(&state.db)
    .await?;
    let configured = state.shop_payments.stripe_configured();
    Ok(json!({
        "configured": configured,
        "mode": state.shop_payments.stripe_mode().map(|m| match m { StripeMode::Test => "test", StripeMode::Live => "live" }),
        "webhook_configured": configured,
        "last_webhook_at": last,
        "webhook_recent": last.is_some_and(|l| l > Utc::now() - Duration::hours(24)),
        "webhook_path": "/api/v1/webhooks/stripe",
        "events": ["checkout.session.completed", "checkout.session.async_payment_succeeded", "checkout.session.async_payment_failed",
                   "checkout.session.expired", "charge.refunded", "refund.updated", "charge.dispute.created"],
    }))
}

pub async fn get(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        "SELECT key, value FROM settings WHERE key LIKE 'shop.%' OR key LIKE 'payments.stripe.%' OR key LIKE 'payments.cod.%'
             OR key LIKE 'payments.pay_at_pickup.%' OR key LIKE 'payments.bank_transfer.%' OR key = 'payments.allow_cash_fr'
             OR key LIKE 'tax.%' OR key LIKE 'customs.%' OR key LIKE 'legal.%' OR key = 'privacy.data_retention'"
    )
    .fetch_all(&state.db)
    .await?;
    let values: HashMap<String, Value> = rows.into_iter().map(|r| (r.key, r.value)).collect();
    Ok(Json(
        json!({ "values": values, "stripe": stripe_status(&state).await? }),
    ))
}

pub async fn stripe(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    Ok(Json(stripe_status(&state).await?))
}

fn type_ok(kind: &str, v: &Value) -> bool {
    match kind {
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "number" => v.as_i64().is_some_and(|n| n >= 0),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "nullable" => v.is_null() || v.is_string() || v.as_i64().is_some(),
        _ => false,
    }
}

/// Partial update `{key: value}`. Turning Stripe on without its environment keys is refused.
pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<HashMap<String, Value>>,
) -> AppResult<Json<Value>> {
    staff.require("shop.settings")?;
    for (key, value) in &body {
        let Some((_, kind)) = EDITABLE.iter().find(|(k, _)| k == key) else {
            return Err(AppError::BadRequest(format!("{key} is not a shop setting")));
        };
        if !type_ok(kind, value) {
            return Err(AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid",
                format!("{key}: expected {kind}"),
            )
            .with_details(json!({ "key": key })));
        }
    }
    // The simulated provider (PAYMENTS_FAKE, development) counts as configured.
    if body.get("payments.stripe.enabled") == Some(&Value::Bool(true))
        && !state.shop_payments.card_available()
    {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "stripe_not_configured",
            "Stripe keys are missing: set STRIPE_SECRET_KEY, STRIPE_PUBLISHABLE_KEY and STRIPE_WEBHOOK_SECRET on the server first",
        ));
    }
    if let Some(s) = body.get("shop.status").and_then(Value::as_str)
        && !["closed", "catalog_only", "open"].contains(&s)
    {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "status: closed, catalog_only or open",
        ));
    }
    if let Some(m) = body
        .get("payments.stripe.checkout_mode")
        .and_then(Value::as_str)
        && !["hosted", "embedded"].contains(&m)
    {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "checkout mode: hosted or embedded",
        ));
    }
    if body
        .get("shop.checkout_hold_minutes")
        .and_then(Value::as_i64)
        .is_some_and(|m| m < 30)
    {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "Stripe needs at least 30 minutes",
        ));
    }
    // The French checkout text must keep stating the payment obligation (Consumer Code L221-14).
    if let Some(label) = body.get("shop.checkout_button_label")
        && !label["fr"]
            .as_str()
            .is_some_and(|l| l.to_lowercase().contains("obligation de paiement"))
    {
        return Err(AppError::domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            "the French button label must state the payment obligation (« Commander avec obligation de paiement »)",
        ));
    }
    let mut tx = state.db.begin().await?;
    // Cash for a French seller (NF525 certified cash register): super admin only.
    if body.contains_key("payments.allow_cash_fr")
        && !super::france::is_super_admin(&mut tx, staff.id).await?
    {
        return Err(AppError::Forbidden);
    }
    let entity_fr = sqlx::query_scalar!(
        r#"SELECT country_code = 'FR' AS "x!" FROM legal_entities WHERE id = shop_entity_id()"#
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(true);
    let allow_cash = match body.get("payments.allow_cash_fr") {
        Some(v) => v.as_bool().unwrap_or(false),
        None => {
            sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'payments.allow_cash_fr'")
                .fetch_optional(&mut *tx)
                .await?
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        }
    };
    let cash_on = ["payments.cod.enabled", "payments.pay_at_pickup.enabled"]
        .iter()
        .any(|k| body.get(*k) == Some(&Value::Bool(true)));
    if entity_fr && cash_on && !allow_cash {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "cash_locked_fr",
            "cash payments are locked for a French company (certified cash register required): a super admin must allow them first",
        ));
    }
    if body.get("shop.stay_delivery_enabled") == Some(&Value::Bool(true)) && entity_fr {
        return Err(AppError::domain(
            StatusCode::CONFLICT,
            "stay_delivery_unavailable",
            "delivery at the camp is not offered by the French company",
        ));
    }
    if body.get("shop.status").and_then(Value::as_str) == Some("open") {
        let items = super::france::checklist(&mut tx, &state).await?;
        let failing: Vec<&Value> = items.iter().filter(|i| i["ok"] != json!(true)).collect();
        if !failing.is_empty() {
            return Err(AppError::domain(
                StatusCode::CONFLICT,
                "compliance_checklist",
                "the shop cannot open before the compliance checklist passes",
            )
            .with_details(json!({ "failing": failing })));
        }
    }
    for (key, value) in &body {
        let before =
            sqlx::query_scalar!("SELECT value FROM settings WHERE key = $1 FOR UPDATE", key)
                .fetch_optional(&mut *tx)
                .await?;
        sqlx::query!(
            "UPDATE settings SET value = $2, updated_by = $3, updated_at = now() WHERE key = $1",
            key,
            value,
            staff.id
        )
        .execute(&mut *tx)
        .await?;
        audit::record(
            &mut tx,
            &staff.actor(),
            "setting.updated",
            "setting",
            key,
            json!({ "before": before, "after": value }),
        )
        .await?;
        outbox::enqueue(&mut tx, &Event::SettingsUpdated { key: key.clone() }).await?;
    }
    tx.commit().await?;
    state.cache.delete(["settings:shop".to_string()]).await;
    get(State(state), staff).await
}
