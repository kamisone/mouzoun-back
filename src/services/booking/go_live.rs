//! "Go live" checklist: what must be true before `booking.enabled` can be turned on. Each check
//! returns the offending items (product names, missing languages...) as data; the back office
//! words them. `required` checks block the switch, the others are advice.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    camp_time,
    config::{AppEnv, Config, EmailProvider},
    services::payments::status::{PaymentMode, PaymentStatus},
};

/// How far ahead bookable availability must exist.
pub const AVAILABILITY_DAYS: i64 = 60;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Stable key, translated by the back office (`prices`, `availability`, ...).
    pub key: &'static str,
    pub ok: bool,
    pub required: bool,
    /// What is missing, as data.
    pub details: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct Checklist {
    /// Every required check passes.
    pub ready: bool,
    pub enabled: bool,
    pub checks: Vec<Check>,
    pub payments: PaymentStatus,
    pub checked_at: DateTime<Utc>,
}

fn check(key: &'static str, required: bool, missing: Vec<Value>) -> Check {
    Check {
        key,
        ok: missing.is_empty(),
        required,
        details: Value::Array(missing),
    }
}

pub async fn run(pool: &PgPool, config: &Config, now: DateTime<Utc>) -> sqlx::Result<Checklist> {
    let today = camp_time::today(now);
    let horizon = today + Duration::days(AVAILABILITY_DAYS);

    // Published, bookable products without an active price in the default currency.
    let unpriced = sqlx::query!(
        r#"SELECT p.id, p.kind, COALESCE(t.name, p.id::text) AS "name!"
           FROM products p
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
           WHERE p.status = 'published' AND p.deleted_at IS NULL AND p.booking_mode <> 'not_bookable'
             AND NOT EXISTS (SELECT 1 FROM prices pr WHERE pr.product_id = p.id AND pr.is_active
                             AND pr.currency = (SELECT code FROM currencies WHERE is_default))
           ORDER BY p.sort_order"#
    )
    .fetch_all(pool)
    .await?;

    // Public rate plans: at least one, each with a cancellation policy guests can read.
    let plans = sqlx::query!(
        r#"SELECT rp.code::text AS "code!", rp.cancellation_policy_id IS NOT NULL AS "has_policy!"
           FROM rate_plans rp WHERE rp.is_public AND rp.is_active ORDER BY rp.sort_order"#
    )
    .fetch_all(pool)
    .await?;
    let mut plan_problems: Vec<Value> = plans
        .iter()
        .filter(|p| !p.has_policy)
        .map(|p| json!({ "rate_plan": p.code, "missing": "cancellation_policy" }))
        .collect();
    if plans.is_empty() {
        plan_problems.push(json!({ "missing": "public_rate_plan" }));
    }

    // Stays with nothing to sell and scheduled activities without open departures, ahead.
    let unavailable = sqlx::query!(
        r#"SELECT p.kind, COALESCE(t.name, p.id::text) AS "name!"
           FROM products p
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN activities a ON a.product_id = p.id
           WHERE p.status = 'published' AND p.deleted_at IS NULL AND p.booking_mode <> 'not_bookable'
             AND (
               (p.kind = 'accommodation' AND NOT EXISTS (
                  SELECT 1 FROM inventory_calendar ic
                  WHERE ic.product_id = p.id AND ic.day >= $1 AND ic.day < $2
                    AND NOT ic.stop_sell AND ic.capacity > ic.reserved))
               OR (p.kind = 'activity' AND a.scheduling_mode = 'fixed_slots' AND NOT EXISTS (
                  SELECT 1 FROM activity_slots s
                  WHERE s.product_id = p.id AND s.status = 'open' AND s.capacity > s.reserved
                    AND s.starts_at >= $3 AND s.starts_at < $3 + make_interval(days => $4::int)))
             )
           ORDER BY p.sort_order"#,
        today,
        horizon,
        now,
        AVAILABILITY_DAYS as i32
    )
    .fetch_all(pool)
    .await?;

    // Every active language: product texts, rate plans and cancellation policies.
    let untranslated = sqlx::query!(
        r#"SELECT 'product' AS "what!", COALESCE(d.name, p.id::text) AS "name!", l.code AS "locale!"
           FROM products p CROSS JOIN locales l
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = l.code
           WHERE l.is_active AND p.status = 'published' AND p.deleted_at IS NULL
             AND (t.product_id IS NULL OR btrim(t.name) = '' OR btrim(COALESCE(t.short_description, '')) = '')
           UNION ALL
           SELECT 'rate_plan', rp.code::text, l.code
           FROM rate_plans rp CROSS JOIN locales l
           WHERE l.is_active AND rp.is_public AND rp.is_active
             AND NOT EXISTS (SELECT 1 FROM rate_plan_translations t WHERE t.rate_plan_id = rp.id AND t.locale = l.code
                             AND btrim(t.name) <> '')
           UNION ALL
           SELECT 'cancellation_policy', cp.code::text, l.code
           FROM cancellation_policies cp CROSS JOIN locales l
           WHERE l.is_active AND EXISTS (SELECT 1 FROM rate_plans rp WHERE rp.cancellation_policy_id = cp.id
                                          AND rp.is_public AND rp.is_active)
             AND NOT EXISTS (SELECT 1 FROM cancellation_policy_translations t WHERE t.cancellation_policy_id = cp.id
                             AND t.locale = l.code AND btrim(t.name) <> '')
           ORDER BY 1, 2, 3"#
    )
    .fetch_all(pool)
    .await?;

    let payments = PaymentStatus::load(pool, &config.booking).await?;
    let production = config.app_env == AppEnv::Production;
    let mut payment_problems = Vec::new();
    if !payments.online {
        payment_problems.push(json!({ "missing": "provider" }));
    } else if payments.mode == Some(PaymentMode::Simulated) && production {
        payment_problems.push(json!({ "missing": "real_provider" }));
    }
    let mut payment_advice = Vec::new();
    if payments.online && payments.mode != Some(PaymentMode::Simulated) {
        if payments.mode == Some(PaymentMode::Test) {
            payment_advice.push(json!({ "advice": "test_mode" }));
        }
        if payments.last_webhook_at.is_none() {
            payment_advice.push(json!({ "advice": "no_webhook_received" }));
        }
        if payments.webhook_errors > 0 {
            payment_advice
                .push(json!({ "advice": "webhook_errors", "count": payments.webhook_errors }));
        }
    }

    // Guests must receive their confirmation: real email delivery outside development.
    let emails = if config.email.provider == EmailProvider::Log {
        vec![json!({ "missing": "smtp" })]
    } else {
        vec![]
    };

    let enabled = sqlx::query_scalar!("SELECT value FROM settings WHERE key = 'booking.enabled'")
        .fetch_optional(pool)
        .await?
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let checks = vec![
        check(
            "prices",
            true,
            unpriced
                .into_iter()
                .map(|p| json!({ "product": p.name, "kind": p.kind }))
                .collect(),
        ),
        check("rate_plans", true, plan_problems),
        check(
            "availability",
            true,
            unavailable
                .into_iter()
                .map(|p| json!({ "product": p.name, "kind": p.kind }))
                .collect(),
        ),
        check(
            "content",
            true,
            untranslated
                .into_iter()
                .map(|t| json!({ "what": t.what, "name": t.name, "locale": t.locale }))
                .collect(),
        ),
        check("payments", true, payment_problems),
        check("payments_verified", false, payment_advice),
        check("emails", production, emails),
    ];
    Ok(Checklist {
        ready: checks.iter().all(|c| c.ok || !c.required),
        enabled,
        checks,
        payments,
        checked_at: now,
    })
}
