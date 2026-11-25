//! French operation of the shop: legal entities (and an invoice preview), VAT classes and
//! rates, the compliance checklist that must pass before the shop opens, the OSS threshold
//! warning, invoice PDFs and the accounting exports (sales journal, credit notes, Stripe fees
//! and payouts). See docs/shop/france.md.

use std::time::Duration as StdDuration;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use chrono::{Datelike, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    auth::Staff,
    error::{AppError, AppResult},
    services::{
        audit,
        pdf::{Document, Style},
        shop::{invoices, legal, legal_entity, settings as shop_settings},
    },
    state::AppState,
};

fn invalid(message: impl Into<String>) -> AppError {
    AppError::domain(StatusCode::UNPROCESSABLE_ENTITY, "invalid", message)
}

pub async fn is_super_admin(conn: &mut PgConnection, user_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM user_roles ur JOIN roles r ON r.id = ur.role_id
                          WHERE ur.user_id = $1 AND r.code = 'super_admin') AS "x!""#,
        user_id
    )
    .fetch_one(conn)
    .await
}

// ---- Legal entities -------------------------------------------------------------------------------

pub async fn entities(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT e.id, e.code::text AS "code!", e.legal_name, e.legal_form, e.share_capital_minor, e.currency::text AS "currency!",
                  e.address, e.country_code::text AS "country_code!", e.registration, e.email::text AS email, e.phone, e.invoice_prefix,
                  e.publication_director, e.host, e.is_active,
                  COALESCE((SELECT jsonb_object_agg(locale, invoice_footer) FROM legal_entity_translations WHERE legal_entity_id = e.id), '{}') AS "invoice_footer!"
           FROM legal_entities e ORDER BY e.code"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "data": rows.into_iter().map(|e| json!({
        "id": e.id, "code": e.code, "legal_name": e.legal_name, "legal_form": e.legal_form,
        "share_capital_minor": e.share_capital_minor, "currency": e.currency, "address": e.address,
        "country_code": e.country_code, "registration": e.registration, "email": e.email, "phone": e.phone,
        "invoice_prefix": e.invoice_prefix, "publication_director": e.publication_director, "host": e.host,
        "is_active": e.is_active, "invoice_footer": e.invoice_footer,
    })).collect::<Vec<_>>() })))
}

#[derive(Debug, Deserialize)]
pub struct EntityInput {
    pub legal_name: String,
    pub legal_form: String,
    pub share_capital_minor: Option<i64>,
    pub address: Value,
    #[serde(default)]
    pub registration: Value,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub publication_director: Option<String>,
    #[serde(default)]
    pub host: Value,
    #[serde(default)]
    pub invoice_footer: std::collections::HashMap<String, String>,
}

/// Names of the fields (one level into `address`, `registration`, `host`) that differ: the audit
/// keeps no values.
fn changed_fields(before: &Value, after: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in after.as_object().into_iter().flatten() {
        if k == "updated_at" {
            continue;
        }
        match (v, &before[k]) {
            (Value::Object(a), Value::Object(b)) => {
                let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
                out.extend(
                    keys.into_iter()
                        .filter(|x| a.get(*x) != b.get(*x))
                        .map(|x| format!("{k}.{x}")),
                );
            }
            (a, b) if a != b => out.push(k.clone()),
            _ => {}
        }
    }
    out
}

pub async fn update_entity(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(b): Json<EntityInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.settings")?;
    if !b.address.is_object() || !(b.registration.is_object() || b.registration.is_null()) {
        return Err(invalid("address and registration are objects"));
    }
    if b.legal_name.trim().is_empty() || b.legal_form.trim().is_empty() {
        return Err(invalid("legal name and legal form are required"));
    }
    let mut tx = state.db.begin().await?;
    let before = sqlx::query_scalar!(
        "SELECT to_jsonb(e) FROM legal_entities e WHERE id = $1 FOR UPDATE",
        id
    )
    .fetch_optional(&mut *tx)
    .await?
    .flatten()
    .ok_or(AppError::NotFound)?;
    // A French entity follows the same rules as Admin › Settings › Legal entity.
    if before["country_code"] == "FR" {
        let text = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
        let form = legal_entity::LegalEntityForm {
            legal_name: b.legal_name.clone(),
            legal_form: b.legal_form.clone(),
            share_capital_minor: b.share_capital_minor,
            country_code: "FR".into(),
            email: b.email.clone().unwrap_or_default(),
            address: legal_entity::Address {
                postal_code: text(&b.address, "postal_code"),
                ..Default::default()
            },
            registration: legal_entity::Registration {
                siren: text(&b.registration, "siren"),
                siret: text(&b.registration, "siret"),
                vat_number: text(&b.registration, "vat_number"),
                ape_code: text(&b.registration, "ape_code"),
                ..Default::default()
            },
            ..Default::default()
        }
        .normalized();
        let errors = form.errors();
        if !errors.is_empty() {
            return Err(AppError::domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_fields",
                "some fields are not valid",
            )
            .with_details(json!({ "fields": errors })));
        }
    }
    sqlx::query!(
        "UPDATE legal_entities SET legal_name = $2, legal_form = $3, share_capital_minor = $4, address = $5,
                registration = $6, email = NULLIF(btrim($7), '')::citext, phone = NULLIF(btrim($8), ''),
                publication_director = NULLIF(btrim($9), ''), host = $10
         WHERE id = $1",
        id,
        b.legal_name.trim(),
        b.legal_form.trim(),
        b.share_capital_minor,
        b.address,
        if b.registration.is_null() { json!({}) } else { b.registration.clone() },
        b.email.as_deref().unwrap_or(""),
        b.phone.as_deref().unwrap_or(""),
        b.publication_director.as_deref().unwrap_or(""),
        if b.host.is_object() { b.host.clone() } else { json!({}) }
    )
    .execute(&mut *tx)
    .await?;
    let after = sqlx::query_scalar!("SELECT to_jsonb(e) FROM legal_entities e WHERE id = $1", id)
        .fetch_one(&mut *tx)
        .await?
        .unwrap_or(Value::Null);
    for (locale, footer) in &b.invoice_footer {
        sqlx::query!(
            "INSERT INTO legal_entity_translations (legal_entity_id, locale, invoice_footer) VALUES ($1, $2, $3)
             ON CONFLICT (legal_entity_id, locale) DO UPDATE SET invoice_footer = EXCLUDED.invoice_footer",
            id,
            locale,
            footer
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "legal_entity.updated",
        "legal_entity",
        &id.to_string(),
        json!({ "fields": changed_fields(&before, &after) }),
    )
    .await?;
    tx.commit().await?;
    entities(State(state), staff).await
}

/// A sample invoice of the entity (not numbered, not stored), to check the printed identity.
pub async fn invoice_preview(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    let seller = invoices::seller_snapshot(&mut conn, id)
        .await
        .map_err(|e| match e {
            sqlx::Error::RowNotFound => AppError::NotFound,
            e => e.into(),
        })?;
    let mut d = Document::new();
    d.text(Style::Title, "Facture n° APERÇU (spécimen)");
    d.text(
        Style::Body,
        format!(
            "{} — {}",
            seller["legal_name"].as_str().unwrap_or(""),
            seller["legal_form"].as_str().unwrap_or("")
        ),
    );
    d.text(Style::Body, invoices::address_line(&seller["address"]));
    let reg = &seller["registration"];
    d.text(
        Style::Body,
        format!(
            "SIREN {} · SIRET {} · RCS {} · TVA {}",
            reg["siren"].as_str().unwrap_or("—"),
            reg["siret"].as_str().unwrap_or("—"),
            reg["rcs_city"].as_str().unwrap_or("—"),
            reg["vat_number"].as_str().unwrap_or("—")
        ),
    );
    d.space(8.0).rule();
    d.row(
        &[
            (0.5, "Dattes Medjool 500 g"),
            (0.1, "1"),
            (0.2, "TVA 5,5 %"),
            (0.2, &invoices::eur(1290, "EUR")),
        ],
        false,
    );
    d.row(
        &[
            (0.5, "Panier en osier"),
            (0.1, "1"),
            (0.2, "TVA 20 %"),
            (0.2, &invoices::eur(2400, "EUR")),
        ],
        false,
    );
    d.rule();
    if let Some(f) = seller["invoice_footer"]["fr"].as_str() {
        d.text(Style::Small, f.to_string());
    }
    Ok(([(header::CONTENT_TYPE, "application/pdf")], d.render()).into_response())
}

/// Signed link to the sealed PDF of an invoice or credit note.
pub async fn invoice_pdf(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Response> {
    staff.require("shop.read")?;
    if !staff.can("shop.orders") && !staff.can("shop.refunds") {
        return Err(AppError::Forbidden);
    }
    let r = sqlx::query!(
        "SELECT number, pdf_object_key FROM invoices WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let Some(key) = r.pdf_object_key else {
        // Not sealed yet (the worker stores it right after issuing): render it on the fly.
        let mut conn = state.db.acquire().await?;
        let (bytes, _) = invoices::render_pdf(&mut conn, id).await?;
        return Ok(([(header::CONTENT_TYPE, "application/pdf")], bytes).into_response());
    };
    let url = state
        .media
        .storage()
        .signed_download_url(
            &key,
            StdDuration::from_secs(300),
            Some(&format!("{}.pdf", r.number)),
        )
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(Redirect::temporary(&url).into_response())
}

// ---- VAT -------------------------------------------------------------------------------------------

pub async fn tax_rates(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let classes = sqlx::query!(
        r#"SELECT c.code::text AS "code!", COALESCE((SELECT jsonb_object_agg(locale, name) FROM tax_class_translations WHERE tax_class_code = c.code), '{}') AS "names!"
           FROM tax_classes c ORDER BY c.sort_order"#
    )
    .fetch_all(&state.db)
    .await?;
    let rates = sqlx::query!(
        r#"SELECT id, tax_class_code::text AS "class!", country_code::text AS "country!", rate_percent,
                  lower(valid_period) AS valid_from, upper(valid_period) AS valid_until, is_active, is_confirmed, note
           FROM tax_rates ORDER BY country_code = 'FR' DESC, country_code, tax_class_code, lower(valid_period)"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({
        "classes": classes.into_iter().map(|c| json!({ "code": c.code, "names": c.names })).collect::<Vec<_>>(),
        "rates": rates.into_iter().map(|r| json!({ "id": r.id, "tax_class_code": r.class, "country_code": r.country,
            "rate_percent": r.rate_percent, "valid_from": r.valid_from, "valid_until": r.valid_until,
            "is_active": r.is_active, "is_confirmed": r.is_confirmed, "note": r.note })).collect::<Vec<_>>(),
        "oss": oss_status(&state).await?,
    })))
}

#[derive(Debug, Deserialize)]
pub struct RateInput {
    pub tax_class_code: String,
    pub country_code: String,
    pub rate_percent: Decimal,
    pub valid_from: NaiveDate,
    pub valid_until: Option<NaiveDate>,
    #[serde(default = "yes")]
    pub is_active: bool,
    #[serde(default)]
    pub is_confirmed: bool,
    pub note: Option<String>,
}

fn yes() -> bool {
    true
}

/// Creates (`id` None) or replaces a rate. Overlapping active periods for the same class and
/// country are refused by the database (exclusion constraint) → 409.
pub async fn save_rate(
    State(state): State<AppState>,
    staff: Staff,
    id: Option<Path<Uuid>>,
    Json(b): Json<RateInput>,
) -> AppResult<Json<Value>> {
    staff.require("shop.pricing")?;
    if b.rate_percent < Decimal::ZERO
        || b.rate_percent > Decimal::from(100)
        || b.valid_until.is_some_and(|u| u <= b.valid_from)
    {
        return Err(invalid(
            "rate between 0 and 100, period end after its start",
        ));
    }
    let country = b.country_code.trim().to_ascii_uppercase();
    let period = sqlx::postgres::types::PgRange {
        start: std::ops::Bound::Included(b.valid_from),
        end: b
            .valid_until
            .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
    };
    let mut tx = state.db.begin().await?;
    let rate_id = id.map_or_else(Uuid::now_v7, |Path(x)| x);
    let res = sqlx::query!(
        "INSERT INTO tax_rates (id, tax_class_code, country_code, rate_percent, valid_period, is_active, is_confirmed, note)
         VALUES ($1, $2::text::citext, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (id) DO UPDATE SET tax_class_code = EXCLUDED.tax_class_code, country_code = EXCLUDED.country_code,
             rate_percent = EXCLUDED.rate_percent, valid_period = EXCLUDED.valid_period, is_active = EXCLUDED.is_active,
             is_confirmed = EXCLUDED.is_confirmed, note = EXCLUDED.note",
        rate_id,
        b.tax_class_code,
        country,
        b.rate_percent,
        period,
        b.is_active,
        b.is_confirmed,
        b.note
    )
    .execute(&mut *tx)
    .await;
    if let Err(e) = res {
        let code = e
            .as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.to_string());
        return Err(match code.as_deref() {
            Some("23P01") => AppError::domain(
                StatusCode::CONFLICT,
                "rate_overlap",
                "another active rate covers this period",
            ),
            Some("23503" | "23514") => invalid("unknown tax class or invalid country"),
            _ => e.into(),
        });
    }
    audit::record(
        &mut tx,
        &staff.actor(),
        "tax_rate.saved",
        "tax_rate",
        &rate_id.to_string(),
        json!({ "rate_percent": b.rate_percent, "country": country }),
    )
    .await?;
    tx.commit().await?;
    tax_rates(State(state), staff).await
}

/// EU distance sales of the calendar year (goods shipped to another member state), against
/// the €10,000 threshold above which VAT is due in the customer's country (OSS).
pub async fn oss_status(state: &AppState) -> AppResult<Value> {
    let s = shop_settings::load(&state.db).await?;
    let year = Utc::now().year();
    let total = sqlx::query_scalar!(
        r#"SELECT COALESCE(sum(o.total_minor - o.tax_minor), 0)::bigint AS "n!" FROM orders o
           WHERE o.payment_status IN ('paid', 'partially_refunded') AND extract(year FROM o.paid_at)::int = $1
             AND COALESCE(o.relay_point->>'country_code', o.shipping_address->>'country_code') <> 'FR'
             AND COALESCE(o.relay_point->>'country_code', o.shipping_address->>'country_code') = ANY($2)"#,
        year,
        &s.eu_countries
    )
    .fetch_one(&state.db)
    .await?;
    let warn_at = s.tax.oss_threshold_minor * 8 / 10;
    Ok(json!({
        "year": year, "eu_distance_sales_minor": total, "threshold_minor": s.tax.oss_threshold_minor,
        "oss_enabled": s.tax.oss_enabled,
        "level": if s.tax.oss_enabled { "ok" } else if total >= s.tax.oss_threshold_minor { "exceeded" } else if total >= warn_at { "warning" } else { "ok" },
    }))
}

// ---- Compliance checklist ----------------------------------------------------------------------

/// Where the seller identity and the mediator are edited (front-end path).
pub const LEGAL_ENTITY_FORM: &str = "/admin/settings/legal-entity";

/// Checks that must pass before `shop.status` can be set to `open`.
pub async fn checklist(conn: &mut PgConnection, state: &AppState) -> AppResult<Vec<Value>> {
    let s = shop_settings::load(&mut *conn).await?;
    let mut out = Vec::new();
    let mut check = |code: &str, ok: bool, detail: Value| {
        out.push(json!({ "code": code, "ok": ok, "detail": detail }))
    };

    // The seller identity and the mediator are edited in Admin › Settings › Legal entity: the
    // details list the missing fields (codes, shown as labels) and link to that form.
    let identity = legal_entity::load(&mut *conn).await.ok().map(|(_, f)| f);
    let missing: Vec<&str> = match &identity {
        Some(f) => f.missing(),
        None => vec!["shop.legal_entity"],
    };
    check(
        "seller_identity",
        missing.is_empty(),
        json!({ "missing": missing, "link": LEGAL_ENTITY_FORM }),
    );
    let unconfirmed = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM tax_rates WHERE is_active AND NOT is_confirmed
             AND country_code = (SELECT country_code FROM legal_entities WHERE id = shop_entity_id())"#
    )
    .fetch_one(&mut *conn)
    .await?;
    check(
        "vat_rates_confirmed",
        unconfirmed == 0,
        json!({ "unconfirmed": unconfirmed }),
    );

    let mut missing_pages = Vec::new();
    for key in [
        "cgv",
        "notice",
        "privacy",
        "cookies",
        "withdrawal",
        "shipping",
    ] {
        let ok = match s.legal_pages.get(key) {
            Some(page) => sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM pages p JOIN page_translations t ON t.page_id = p.id AND t.locale = 'fr'
                                  WHERE p.id = $1 AND p.status = 'published' AND p.deleted_at IS NULL
                                    AND EXISTS (SELECT 1 FROM page_revisions r WHERE r.page_id = p.id AND r.status = 'published')) AS "x!""#,
                page
            )
            .fetch_one(&mut *conn)
            .await?,
            None => false,
        };
        if !ok {
            missing_pages.push(key);
        }
    }
    check(
        "legal_pages_published",
        missing_pages.is_empty(),
        json!({ "missing": missing_pages }),
    );
    let mediator_missing = identity
        .as_ref()
        .map(|f| f.mediator_missing())
        .unwrap_or_else(|| vec!["mediator.name", "mediator.url"]);
    check(
        "consumer_mediator",
        mediator_missing.is_empty(),
        json!({ "missing": mediator_missing, "link": LEGAL_ENTITY_FORM }),
    );
    let label = shop_settings::ShopSettings::localized(&s.checkout_button_label, "fr");
    let label_ok = s.checkout_button_label["fr"]
        .as_str()
        .is_some_and(|l| l.to_lowercase().contains("obligation de paiement"));
    check("checkout_button_label", label_ok, json!({ "fr": label }));
    check(
        "card_payment",
        s.payments.stripe_enabled && state.shop_payments.card_available(),
        json!({ "enabled": s.payments.stripe_enabled, "keys": state.shop_payments.card_available() }),
    );
    // Mouzoun's own webhook endpoint (the Stripe account is shared): its signing secret is set and
    // at least one of our events arrived through it.
    let secret_set = state
        .config
        .booking
        .stripe
        .as_ref()
        .is_some_and(|c| !c.webhook_secret.trim().is_empty());
    let received = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM payment_webhook_events WHERE provider = 'stripe' AND status = 'received') AS "x!""#
    )
    .fetch_one(&mut *conn)
    .await?;
    // The simulated provider (PAYMENTS_FAKE, development only) has no webhook to configure.
    let simulated =
        !state.shop_payments.stripe_configured() && state.shop_payments.card_available();
    check(
        "stripe_webhook",
        simulated || (secret_set && received),
        json!({ "reason": if !secret_set { Some("STRIPE_WEBHOOK_SECRET is not set: add Mouzoun's endpoint in the Stripe Dashboard and paste its signing secret") }
                          else if !received { Some("no Stripe event received yet on Mouzoun's endpoint") } else { None },
                "secret_set": secret_set, "event_received": received }),
    );

    let incomplete = sqlx::query_scalar!(
        r#"SELECT p.id FROM products p WHERE p.kind = 'shop_product' AND p.status = 'published' AND p.deleted_at IS NULL"#
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut bad = Vec::new();
    for id in incomplete {
        if super::catalog::publish_check(conn, id).await.is_err() {
            bad.push(id);
        }
    }
    check(
        "products_complete_in_french",
        bad.is_empty(),
        json!({ "product_ids": bad }),
    );

    let shipping = sqlx::query!(
        r#"SELECT count(*) FILTER (WHERE NOT r.is_placeholder) AS "rates!", count(*) FILTER (WHERE r.is_placeholder) AS "placeholders!"
           FROM shipping_rates r JOIN shipping_zones z ON z.id = r.zone_id
           WHERE r.is_active AND z.is_active AND 'FR' = ANY(z.country_codes)"#
    )
    .fetch_one(&mut *conn)
    .await?;
    check(
        "shipping_rates_france",
        shipping.rates > 0,
        json!({ "rates": shipping.rates, "placeholders": shipping.placeholders }),
    );
    let food = crate::services::shop::legal::food_info_gaps(&mut *conn).await?;
    check(
        "food_information",
        food.is_empty(),
        json!({ "missing": food.len(), "products": food }),
    );
    let stock = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM stock_locations WHERE is_active AND fulfills_online_orders AND legal_entity_id = shop_entity_id()"#
    )
    .fetch_one(&mut *conn)
    .await?;
    check("stock_location", stock > 0, json!({ "locations": stock }));

    let templates = sqlx::query!(
        r#"SELECT e.code::text AS "code!", t.subject, t.body_html FROM email_templates e
           LEFT JOIN email_template_translations t ON t.email_template_id = e.id AND t.locale = 'fr'
           WHERE e.code = ANY($1)"#,
        &legal::FRENCH_REQUIRED_TEMPLATES
            .iter()
            .map(|c| (*c).to_string())
            .collect::<Vec<_>>()
    )
    .fetch_all(&mut *conn)
    .await?;
    let bad_templates: Vec<String> = templates
        .into_iter()
        .filter(|t| {
            !legal::email_template_french_ok(&t.code, t.subject.as_deref(), t.body_html.as_deref())
        })
        .map(|t| t.code)
        .collect();
    check(
        "emails_in_french",
        bad_templates.is_empty(),
        json!({ "templates": bad_templates }),
    );
    Ok(out)
}

pub async fn compliance(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let mut conn = state.db.acquire().await?;
    let items = checklist(&mut conn, &state).await?;
    let ok = items.iter().all(|i| i["ok"] == json!(true));
    Ok(Json(
        json!({ "ok": ok, "items": items, "oss": oss_status(&state).await? }),
    ))
}

// ---- Accounting exports ------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Period {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

fn csv(s: &str) -> String {
    let s = if s.starts_with(['=', '+', '-', '@']) && s.parse::<f64>().is_err() {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([';', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn amount(minor: i64) -> String {
    let sign = if minor < 0 { "-" } else { "" };
    format!("{sign}{},{:02}", minor.abs() / 100, minor.abs() % 100)
}

fn csv_response(name: &str, body: String) -> Response {
    // UTF-8 BOM and semicolons: opens as-is in French spreadsheets.
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        format!("\u{feff}{body}"),
    )
        .into_response()
}

/// Sales journal: per day, VAT rate and destination country (invoices and credit notes).
pub async fn sales_journal(
    State(state): State<AppState>,
    staff: Staff,
    Query(p): Query<Period>,
) -> AppResult<Response> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT (i.issued_at AT TIME ZONE 'Europe/Paris')::date AS "day!", i.kind, e.code::text AS "entity!",
                  COALESCE(i.billing_details->'billing_address'->>'country_code', '') AS "country!",
                  b->>'rate' AS "rate!", sum((b->>'base_minor')::bigint)::bigint AS "base!", sum((b->>'vat_minor')::bigint)::bigint AS "vat!",
                  sum((b->>'gross_minor')::bigint)::bigint AS "gross!", count(DISTINCT i.id) AS "documents!"
           FROM invoices i JOIN legal_entities e ON e.id = i.legal_entity_id, jsonb_array_elements(i.vat_breakdown) b
           WHERE i.order_id IS NOT NULL AND (i.issued_at AT TIME ZONE 'Europe/Paris')::date BETWEEN $1 AND $2
           GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 3, 2, 4, 5"#,
        p.from,
        p.to
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::from("date;entite;type;pays;taux_tva;base_ht;tva;ttc;documents\n");
    for r in rows {
        out.push_str(&format!(
            "{};{};{};{};{};{};{};{};{}\n",
            r.day,
            csv(&r.entity),
            if r.kind == "credit_note" {
                "avoir"
            } else {
                "facture"
            },
            csv(&r.country),
            csv(&r.rate.replace('.', ",")),
            amount(r.base),
            amount(r.vat),
            amount(r.gross),
            r.documents
        ));
    }
    Ok(csv_response(
        &format!("journal-ventes-{}-{}.csv", p.from, p.to),
        out,
    ))
}

/// Every invoice and credit note of the period, one line each (numbering audit).
pub async fn documents_export(
    State(state): State<AppState>,
    staff: Staff,
    Query(p): Query<Period>,
) -> AppResult<Response> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT i.number, i.kind, i.issued_at, i.total_minor, i.tax_minor, i.pdf_sha256, e.code::text AS "entity!",
                  (SELECT number FROM invoices c WHERE c.id = i.credit_note_for_invoice_id) AS corrected,
                  i.billing_details->>'order_number' AS order_number
           FROM invoices i JOIN legal_entities e ON e.id = i.legal_entity_id
           WHERE i.order_id IS NOT NULL AND (i.issued_at AT TIME ZONE 'Europe/Paris')::date BETWEEN $1 AND $2
           ORDER BY e.code, i.number"#,
        p.from,
        p.to
    )
    .fetch_all(&state.db)
    .await?;
    let mut out =
        String::from("entite;numero;type;date;commande;facture_corrigee;ttc;tva;sha256_pdf\n");
    for r in rows {
        out.push_str(&format!(
            "{};{};{};{};{};{};{};{};{}\n",
            csv(&r.entity),
            csv(&r.number),
            if r.kind == "credit_note" {
                "avoir"
            } else {
                "facture"
            },
            r.issued_at.format("%Y-%m-%d %H:%M:%S"),
            csv(r.order_number.as_deref().unwrap_or("")),
            csv(r.corrected.as_deref().unwrap_or("")),
            amount(r.total_minor),
            amount(r.tax_minor),
            r.pdf_sha256.unwrap_or_default()
        ));
    }
    Ok(csv_response(
        &format!("factures-avoirs-{}-{}.csv", p.from, p.to),
        out,
    ))
}

/// Card payments with their Stripe fees and payouts (from the daily balance import).
pub async fn stripe_export(
    State(state): State<AppState>,
    staff: Staff,
    Query(p): Query<Period>,
) -> AppResult<Response> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT p.provider_payment_id, p.paid_at, p.amount_minor, p.fee_minor, p.net_minor, p.payout_id, p.settled_at, o.number AS "order_number?"
           FROM payments p LEFT JOIN orders o ON o.id = p.order_id
           WHERE p.provider = 'stripe' AND p.paid_at IS NOT NULL AND (p.paid_at AT TIME ZONE 'Europe/Paris')::date BETWEEN $1 AND $2
           ORDER BY p.paid_at"#,
        p.from,
        p.to
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = String::from("date;commande;paiement;brut;frais;net;virement;date_virement\n");
    for r in rows {
        out.push_str(&format!(
            "{};{};{};{};{};{};{};{}\n",
            r.paid_at
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            csv(r.order_number.as_deref().unwrap_or("")),
            csv(r.provider_payment_id.as_deref().unwrap_or("")),
            amount(r.amount_minor),
            r.fee_minor.map(amount).unwrap_or_default(),
            r.net_minor.map(amount).unwrap_or_default(),
            csv(r.payout_id.as_deref().unwrap_or("")),
            r.settled_at
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        ));
    }
    Ok(csv_response(
        &format!("stripe-{}-{}.csv", p.from, p.to),
        out,
    ))
}

/// Withdrawals whose refund deadline is near (dashboard).
pub async fn withdrawals_due(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<Json<Value>> {
    staff.require("shop.read")?;
    let rows = sqlx::query!(
        r#"SELECT r.id, r.number, r.status, r.refund_due_by, r.requested_at, o.id AS order_id, o.number AS order_number
           FROM order_returns r JOIN orders o ON o.id = r.order_id
           WHERE r.reason = 'withdrawal' AND r.status NOT IN ('rejected', 'closed') AND r.refund_due_by IS NOT NULL
           ORDER BY r.refund_due_by LIMIT 100"#
    )
    .fetch_all(&state.db)
    .await?;
    let today = Utc::now().date_naive();
    Ok(Json(json!({ "data": rows.into_iter().map(|r| json!({
        "id": r.id, "number": r.number, "status": r.status, "refund_due_by": r.refund_due_by, "requested_at": r.requested_at,
        "order_id": r.order_id, "order_number": r.order_number,
        "days_left": r.refund_due_by.map(|d| (d - today).num_days()),
    })).collect::<Vec<_>>() })))
}
