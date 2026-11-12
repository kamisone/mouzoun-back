//! Back-office bookings: list (filters, CSV), detail with timeline, every staff action, manual
//! bookings (phone, WhatsApp, walk-in, agency) on the same quote engine, staff tasks.

use std::fmt::Write as _;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, NaiveTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    auth::Staff,
    camp_time,
    db::{
        bookings::{
            BookingSource, BookingStatus, CancelledBy, PaymentOption, PaymentProvider,
            PaymentPurpose,
        },
        types::Locale,
    },
    error::{AppError, AppResult},
    modules::booking::{booking_settings, manage::booking_view},
    services::{
        audit,
        booking::{
            CustomerInput, CustomerRef,
            manage::{self, RefundChoice},
            register::{self, GuestEntry},
            trip::{self, CheckoutInput, TripContext, TripItem, TripRequest},
        },
        outbox::{self, Event},
        payments,
    },
    state::AppState,
    web::{client::ClientInfo, money::Euros, pagination::Paginated},
};

// ---- List -------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Reference, name, email or phone.
    pub q: Option<String>,
    /// Comma-separated statuses.
    pub status: Option<String>,
    pub arrival_from: Option<NaiveDate>,
    pub arrival_to: Option<NaiveDate>,
    pub created_from: Option<NaiveDate>,
    pub created_to: Option<NaiveDate>,
    pub source: Option<String>,
    pub product_id: Option<Uuid>,
    /// `unpaid`, `partial`, `paid`.
    pub payment: Option<String>,
    /// Balance due within 7 days.
    #[serde(default)]
    pub balance_due: bool,
    /// `arrival` (default), `-arrival`, `-created`, `created`, `reference`.
    pub sort: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
}

#[derive(Debug, sqlx::FromRow)]
struct Row {
    id: Uuid,
    reference: String,
    status: String,
    source: String,
    arrival_date: Option<NaiveDate>,
    departure_date: Option<NaiveDate>,
    adults: i16,
    children: i16,
    infants: i16,
    total_minor: i64,
    amount_paid_minor: i64,
    balance_due_on: Option<NaiveDate>,
    hold_expires_at: Option<chrono::DateTime<Utc>>,
    created_at: chrono::DateTime<Utc>,
    first_name: String,
    last_name: Option<String>,
    email: Option<String>,
    phone: Option<String>,
    precheckin: bool,
    products: Option<String>,
    total_count: i64,
}

async fn rows(state: &AppState, q: &ListQuery, limit: i64, offset: i64) -> AppResult<Vec<Row>> {
    let statuses: Option<Vec<String>> = q
        .status
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect());
    let search =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                format!(
                    "%{}%",
                    s.replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            });
    let today = camp_time::today(Utc::now());
    Ok(sqlx::query_as::<_, Row>(
        "SELECT b.id, b.reference, b.status, b.source, b.arrival_date, b.departure_date, b.adults, b.children,
                b.infants, b.total_minor, b.amount_paid_minor, b.balance_due_on, b.hold_expires_at, b.created_at,
                c.first_name, c.last_name, c.email::text AS email, c.phone,
                b.precheckin_completed_at IS NOT NULL AS precheckin,
                (SELECT string_agg(COALESCE(i.product_snapshot->>'localized_name', i.product_snapshot->>'name'), ', ')
                 FROM booking_items i WHERE i.booking_id = b.id AND i.parent_item_id IS NULL) AS products,
                count(*) OVER () AS total_count
         FROM bookings b JOIN customers c ON c.id = b.customer_id
         WHERE ($1::text IS NULL OR b.reference ILIKE $1 OR c.email::text ILIKE $1 OR c.phone ILIKE $1
                OR (c.first_name || ' ' || COALESCE(c.last_name, '')) ILIKE $1)
           AND ($2::text[] IS NULL OR b.status = ANY($2))
           AND ($3::date IS NULL OR b.arrival_date >= $3)
           AND ($4::date IS NULL OR b.arrival_date <= $4)
           AND ($5::date IS NULL OR b.created_at >= $5::date)
           AND ($6::date IS NULL OR b.created_at < $6::date + 1)
           AND ($7::text IS NULL OR b.source = $7)
           AND ($8::uuid IS NULL OR EXISTS (SELECT 1 FROM booking_items i WHERE i.booking_id = b.id AND i.product_id = $8))
           AND ($9::text IS NULL
                OR ($9 = 'unpaid' AND b.amount_paid_minor <= 0)
                OR ($9 = 'partial' AND b.amount_paid_minor > 0 AND b.amount_paid_minor < b.total_minor)
                OR ($9 = 'paid' AND b.amount_paid_minor >= b.total_minor))
           AND (NOT $10 OR (b.status IN ('confirmed', 'checked_in') AND b.total_minor > b.amount_paid_minor
                            AND COALESCE(b.balance_due_on, b.arrival_date) <= $11))
         ORDER BY
           CASE WHEN $12 = 'arrival' THEN b.arrival_date END ASC NULLS LAST,
           CASE WHEN $12 = '-arrival' THEN b.arrival_date END DESC NULLS LAST,
           CASE WHEN $12 = 'created' THEN b.created_at END ASC,
           CASE WHEN $12 = 'reference' THEN b.reference END ASC,
           b.created_at DESC
         LIMIT $13 OFFSET $14",
    )
    .bind(search)
    .bind(statuses)
    .bind(q.arrival_from)
    .bind(q.arrival_to)
    .bind(q.created_from)
    .bind(q.created_to)
    .bind(q.source.as_deref().filter(|s| !s.is_empty()))
    .bind(q.product_id)
    .bind(q.payment.as_deref().filter(|s| !s.is_empty()))
    .bind(q.balance_due)
    .bind(today + chrono::Duration::days(7))
    .bind(q.sort.as_deref().unwrap_or("-created"))
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.db)
    .await?)
}

fn row_json(r: &Row) -> Value {
    json!({
        "id": r.id, "reference": r.reference, "status": r.status, "source": r.source,
        "arrival": r.arrival_date, "departure": r.departure_date,
        "party": { "adults": r.adults, "children": r.children, "infants": r.infants },
        "total_minor": r.total_minor, "paid_minor": r.amount_paid_minor,
        "balance_minor": (r.total_minor - r.amount_paid_minor).max(0), "balance_due_on": r.balance_due_on,
        "hold_expires_at": r.hold_expires_at, "created_at": r.created_at,
        "customer": { "first_name": r.first_name, "last_name": r.last_name, "email": r.email, "phone": r.phone },
        "precheckin_completed": r.precheckin, "products": r.products,
    })
}

pub async fn list(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Paginated<Value>>> {
    staff.require("bookings.read")?;
    let per_page = q.per_page.unwrap_or(25).clamp(1, 200);
    let page = q.page.unwrap_or(1).max(1);
    let rows = rows(&state, &q, per_page, (page - 1) * per_page).await?;
    let total = rows.first().map_or(0, |r| r.total_count);
    Ok(Json(Paginated {
        data: rows.iter().map(row_json).collect(),
        page,
        per_page,
        total,
    }))
}

fn csv_field(s: &str) -> String {
    // Leading =, +, -, @ would run as formulas in spreadsheets.
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

pub async fn export(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<ListQuery>,
) -> AppResult<Response> {
    staff.require("bookings.read")?;
    let rows = rows(&state, &q, 10_000, 0).await?;
    let mut out = String::from(
        "reference,status,source,arrival,departure,adults,children,infants,first_name,last_name,email,phone,products,total_eur,paid_eur,balance_eur,balance_due_on,created_at\n",
    );
    let euros = |c: i64| format!("{}.{:02}", c / 100, (c % 100).abs());
    for r in &rows {
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            csv_field(&r.reference),
            r.status,
            r.source,
            r.arrival_date.map(|d| d.to_string()).unwrap_or_default(),
            r.departure_date.map(|d| d.to_string()).unwrap_or_default(),
            r.adults,
            r.children,
            r.infants,
            csv_field(&r.first_name),
            csv_field(r.last_name.as_deref().unwrap_or_default()),
            csv_field(r.email.as_deref().unwrap_or_default()),
            csv_field(r.phone.as_deref().unwrap_or_default()),
            csv_field(r.products.as_deref().unwrap_or_default()),
            euros(r.total_minor),
            euros(r.amount_paid_minor),
            euros((r.total_minor - r.amount_paid_minor).max(0)),
            r.balance_due_on.map(|d| d.to_string()).unwrap_or_default(),
            r.created_at.to_rfc3339(),
        );
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"bookings.csv\"".to_string(),
            ),
        ],
        format!("\u{feff}{out}"),
    )
        .into_response())
}

// ---- Detail -----------------------------------------------------------------------------------

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.read")?;
    let mut view = booking_view(&state, id).await.map_err(|e| match e {
        AppError::Database(sqlx::Error::RowNotFound) => AppError::NotFound,
        e => e,
    })?;
    let db = &state.db;
    let b = sqlx::query!(
        r#"SELECT b.source, b.internal_notes, b.customer_id, b.terms_version, b.terms_accepted_at, b.checked_in_at,
                  b.checked_out_at, b.skip_guest_emails, b.ip_address::text AS "ip?", c.marketing_consent,
                  c.whatsapp_preferred, c.internal_notes AS customer_notes,
                  (SELECT count(*) FROM bookings o WHERE o.customer_id = b.customer_id) AS "customer_bookings!"
           FROM bookings b JOIN customers c ON c.id = b.customer_id WHERE b.id = $1"#,
        id
    )
    .fetch_one(db)
    .await?;
    let history: Vec<Value> = sqlx::query!(
        r#"SELECT h.from_status, h.to_status, h.note, h.created_at, u.full_name AS "by?"
           FROM booking_status_history h LEFT JOIN users u ON u.id = h.changed_by
           WHERE h.booking_id = $1 ORDER BY h.created_at, h.id"#,
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|h| json!({ "kind": "status", "from": h.from_status, "to": h.to_status, "note": h.note, "at": h.created_at, "by": h.by }))
    .collect();
    let emails: Vec<Value> = sqlx::query!(
        "SELECT template_code, to_email::text AS to_email, status, subject, created_at, sent_at, error
         FROM email_log WHERE booking_id = $1 ORDER BY created_at",
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|e| json!({
        "kind": "email", "template": e.template_code, "to": e.to_email, "status": e.status, "subject": e.subject,
        "at": e.sent_at.unwrap_or(e.created_at), "error": e.error,
    }))
    .collect();
    let audit_entries: Vec<Value> = sqlx::query!(
        r#"SELECT a.action, a.actor_type, a.changes, a.created_at, u.full_name AS "by?"
           FROM audit_logs a LEFT JOIN users u ON u.id = a.actor_user_id
           WHERE a.entity_type = 'booking' AND a.entity_id = $1 AND a.action <> 'booking.status_changed'
           ORDER BY a.created_at DESC LIMIT 100"#,
        id.to_string()
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|a| json!({ "kind": "audit", "action": a.action, "actor": a.actor_type, "changes": a.changes, "at": a.created_at, "by": a.by }))
    .collect();
    let notes: Vec<Value> = sqlx::query!(
        r#"SELECT n.id, n.body, n.created_at, u.full_name AS "by?" FROM booking_notes n
           LEFT JOIN users u ON u.id = n.author_id WHERE n.booking_id = $1 ORDER BY n.created_at DESC"#,
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|n| json!({ "id": n.id, "body": n.body, "at": n.created_at, "by": n.by }))
    .collect();
    let tasks: Vec<Value> = sqlx::query!(
        "SELECT id, kind, status, title, details, created_at, resolved_at FROM staff_tasks
         WHERE booking_id = $1 ORDER BY created_at DESC",
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|t| json!({ "id": t.id, "kind": t.kind, "status": t.status, "title": t.title, "details": t.details, "at": t.created_at, "resolved_at": t.resolved_at }))
    .collect();
    let refunds: Vec<Value> = sqlx::query!(
        "SELECT r.id, r.payment_id, r.amount_minor, r.status, r.reason, r.created_at, r.failure_reason
         FROM refunds r JOIN payments p ON p.id = r.payment_id WHERE p.booking_id = $1 ORDER BY r.created_at",
        id
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| json!({ "id": r.id, "payment_id": r.payment_id, "amount_minor": r.amount_minor, "status": r.status, "reason": r.reason, "failure_reason": r.failure_reason, "at": r.created_at }))
    .collect();
    let mut timeline: Vec<Value> = history
        .into_iter()
        .chain(emails)
        .chain(audit_entries)
        .collect();
    timeline.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));

    view["id"] = json!(id);
    view["source"] = json!(b.source);
    view["internal_notes"] = json!(b.internal_notes);
    view["customer"]["id"] = json!(b.customer_id);
    view["customer"]["marketing_consent"] = json!(b.marketing_consent);
    view["customer"]["whatsapp_preferred"] = json!(b.whatsapp_preferred);
    view["customer"]["notes"] = json!(b.customer_notes);
    view["customer"]["bookings_count"] = json!(b.customer_bookings);
    view["terms"] = json!({ "version": b.terms_version, "accepted_at": b.terms_accepted_at });
    view["checked_in_at"] = json!(b.checked_in_at);
    view["checked_out_at"] = json!(b.checked_out_at);
    view["skip_guest_emails"] = json!(b.skip_guest_emails);
    view["ip_address"] = json!(b.ip);
    view["timeline"] = json!(timeline);
    view["notes"] = json!(notes);
    view["tasks"] = json!(tasks);
    view["refunds"] = json!(refunds);
    view["permissions"] = json!({
        "write": staff.can("bookings.write"), "cancel": staff.can("bookings.cancel"),
        "refund": staff.can("payments.refund"), "record_offline": staff.can("payments.record_offline"),
        "identity": staff.can("guests.read_identity"),
    });
    Ok(Json(view))
}

// ---- Quote and manual bookings ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct QuoteBody {
    pub items: Vec<TripItem>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub promo_code: Option<String>,
    #[serde(default)]
    pub child_ages: Vec<u8>,
    #[serde(default)]
    pub locale: Option<String>,
}

async fn staff_context(state: &AppState, locale: Option<&str>) -> AppResult<TripContext> {
    let settings = booking_settings(state).await?;
    Ok(TripContext {
        locale: Locale::new(locale.unwrap_or("fr")),
        public: false,
        now: Utc::now(),
        settings,
    })
}

/// Same engine as the website, with private rate plans and unpublished products.
pub async fn quote(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<QuoteBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    let ctx = staff_context(&state, body.locale.as_deref()).await?;
    let mut conn = state.db.acquire().await?;
    let request = TripRequest {
        items: body.items,
        rate_plan_id: body.rate_plan_id,
        promo_code: body.promo_code,
        child_ages: body.child_ages,
    };
    let quote = trip::quote(&mut conn, &request, &ctx).await?;
    Ok(Json(json!({ "quote": quote })))
}

#[derive(Debug, Deserialize)]
pub struct NewCustomer {
    pub first_name: String,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Override {
    /// New total in euros (lower than the quote: manual discount).
    pub total: Euros,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct InitialPayment {
    pub provider: PaymentProvider,
    pub amount: Euros,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub items: Vec<TripItem>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub promo_code: Option<String>,
    #[serde(default)]
    pub child_ages: Vec<u8>,
    #[serde(default)]
    pub customer_id: Option<Uuid>,
    #[serde(default)]
    pub customer: Option<NewCustomer>,
    pub source: BookingSource,
    pub payment_option: PaymentOption,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub special_requests: Option<String>,
    #[serde(default)]
    pub estimated_arrival_time: Option<NaiveTime>,
    #[serde(default)]
    pub skip_guest_emails: bool,
    #[serde(default)]
    pub price_override: Option<Override>,
    #[serde(default)]
    pub payment: Option<InitialPayment>,
    #[serde(default)]
    pub internal_note: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    client: ClientInfo,
    Json(body): Json<CreateBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("bookings.write")?;
    if body.price_override.is_some() {
        staff.require("bookings.override_price")?;
    }
    if body.payment.is_some() {
        staff.require("payments.record_offline")?;
    }
    let customer = match (body.customer_id, &body.customer) {
        (Some(id), _) => CustomerRef::Existing(id),
        (None, Some(c)) => CustomerRef::New(CustomerInput {
            email: c
                .email
                .clone()
                .map(|e| e.trim().to_lowercase())
                .filter(|e| !e.is_empty()),
            first_name: c.first_name.trim().to_string(),
            last_name: c.last_name.clone().filter(|l| !l.trim().is_empty()),
            phone: c.phone.clone().filter(|p| !p.trim().is_empty()),
            country_code: c
                .country_code
                .clone()
                .map(|c| c.to_ascii_uppercase())
                .filter(|c| c.len() == 2),
            preferred_locale: body.locale.as_deref().map(Locale::new),
        }),
        (None, None) => {
            return Err(AppError::BadRequest(
                "customer_id or customer is required".into(),
            ));
        }
    };
    let ctx = staff_context(&state, body.locale.as_deref()).await?;
    let input = CheckoutInput {
        request: TripRequest {
            items: body.items,
            rate_plan_id: body.rate_plan_id,
            promo_code: body.promo_code,
            child_ages: body.child_ages,
        },
        customer,
        payment_option: body.payment_option,
        expected_total_minor: None,
        special_requests: body.special_requests,
        estimated_arrival_time: body.estimated_arrival_time,
        terms_version: None,
        marketing_consent: false,
        whatsapp_preferred: false,
        save_card_for_balance: false,
        source: body.source,
        actor: client.actor(staff.actor()),
        skip_guest_emails: body.skip_guest_emails,
        request_id: client.request_id.clone(),
        ip_address: None,
        user_agent: None,
    };
    let created = trip::create(&state.db, &input, &ctx).await?;
    let now = Utc::now();
    if let Some(o) = &body.price_override {
        if o.reason.trim().is_empty() {
            return Err(AppError::BadRequest(
                "a price override needs a reason".into(),
            ));
        }
        apply_override(
            &state,
            created.booking_id,
            o.total.0,
            o.reason.trim(),
            &staff,
        )
        .await?;
    }
    if let Some(note) = body
        .internal_note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        sqlx::query!(
            "INSERT INTO booking_notes (booking_id, author_id, body) VALUES ($1, $2, $3)",
            created.booking_id,
            staff.id,
            note
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(p) = &body.payment {
        let purpose = if p.amount.0 >= created.total_minor {
            PaymentPurpose::Full
        } else {
            PaymentPurpose::Deposit
        };
        payments::record_offline(
            &state.db,
            created.booking_id,
            p.provider,
            purpose,
            p.amount.0,
            staff.id,
            p.note.as_deref(),
            now,
        )
        .await?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": created.booking_id, "reference": created.reference, "status": created.status,
            "total_minor": created.total_minor,
        })),
    ))
}

/// Lowers the total of a booking (manual discount spread on its items), with the reason in the
/// audit log. Never raises a price.
async fn apply_override(
    state: &AppState,
    booking_id: Uuid,
    new_total: i64,
    reason: &str,
    staff: &Staff,
) -> AppResult<()> {
    let mut tx = state.db.begin().await?;
    let items = sqlx::query!(
        "SELECT id, total_minor FROM booking_items WHERE booking_id = $1 AND status = 'active' AND total_minor > 0
         ORDER BY created_at, id FOR UPDATE",
        booking_id
    )
    .fetch_all(&mut *tx)
    .await?;
    let total: i64 = items.iter().map(|i| i.total_minor).sum();
    if new_total < 0 || new_total > total {
        return Err(AppError::BadRequest(
            "the new total must be between 0 and the quoted total".into(),
        ));
    }
    let discount = total - new_total;
    let weights: Vec<i64> = items.iter().map(|i| i.total_minor).collect();
    let parts = trip::pricing::allocate(discount, &weights);
    for (item, part) in items.iter().zip(parts) {
        sqlx::query!(
            "UPDATE booking_items SET discount_minor = discount_minor + $2, total_minor = total_minor - $2 WHERE id = $1",
            item.id,
            part
        )
        .execute(&mut *tx)
        .await?;
    }
    manage::recompute_totals(&mut tx, booking_id).await?;
    sqlx::query!(
        "UPDATE bookings SET deposit_due_minor = LEAST(deposit_due_minor, total_minor) WHERE id = $1",
        booking_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "booking.price_overridden",
        "booking",
        &booking_id.to_string(),
        json!({ "before": { "total_minor": total }, "after": { "total_minor": new_total }, "reason": reason }),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CustomersQuery {
    pub q: String,
}

pub async fn customers(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<CustomersQuery>,
) -> AppResult<Json<Value>> {
    staff.require("customers.read")?;
    let term = format!("%{}%", q.q.trim().replace('%', "\\%").replace('_', "\\_"));
    let rows = sqlx::query!(
        r#"SELECT c.id, c.first_name, c.last_name, c.email::text AS "email?", c.phone, c.country_code,
                  (SELECT count(*) FROM bookings b WHERE b.customer_id = c.id) AS "bookings!"
           FROM customers c
           WHERE c.deleted_at IS NULL AND c.anonymized_at IS NULL
             AND (c.email::text ILIKE $1 OR c.phone ILIKE $1 OR (c.first_name || ' ' || COALESCE(c.last_name, '')) ILIKE $1)
           ORDER BY c.updated_at DESC LIMIT 20"#,
        term
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|c| json!({ "id": c.id, "first_name": c.first_name, "last_name": c.last_name, "email": c.email, "phone": c.phone, "country_code": c.country_code, "bookings": c.bookings }))
        .collect::<Vec<_>>())))
}

// ---- Actions ----------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DetailsBody {
    #[serde(default)]
    pub estimated_arrival_time: Option<NaiveTime>,
    #[serde(default)]
    pub special_requests: Option<String>,
    #[serde(default)]
    pub internal_notes: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<DetailsBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    manage::update_details(
        &state.db,
        id,
        body.estimated_arrival_time,
        body.special_requests,
        &staff.actor(),
    )
    .await?;
    if let Some(notes) = body.internal_notes {
        sqlx::query!(
            "UPDATE bookings SET internal_notes = NULLIF(btrim($2), '') WHERE id = $1",
            id,
            notes
        )
        .execute(&state.db)
        .await?;
    }
    detail(State(state), staff, Path(id)).await
}

pub async fn accept(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    let settings = booking_settings(&state).await?;
    let status = manage::accept_request(
        &state.db,
        id,
        &staff.actor(),
        settings.payment_link_hours,
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "status": status })))
}

#[derive(Debug, Deserialize)]
pub struct ReasonBody {
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn decline(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<ReasonBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    manage::decline_request(
        &state.db,
        id,
        &staff.actor(),
        body.reason.filter(|r| !r.trim().is_empty()),
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "status": BookingStatus::Cancelled })))
}

#[derive(Debug, Deserialize)]
pub struct StatusBody {
    /// `checked_in`, `completed` (check-out), `no_show`, `confirmed` (confirm without payment).
    pub status: BookingStatus,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn set_status(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<StatusBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    manage::set_status(
        &state.db,
        id,
        body.status,
        &staff.actor(),
        body.note.as_deref(),
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "status": body.status })))
}

#[derive(Debug, Deserialize)]
pub struct CancelBody {
    #[serde(default)]
    pub reason: Option<String>,
    /// `policy` (default), `custom` (with `amount`, in euros) or `none`.
    #[serde(default)]
    pub refund: Option<String>,
    #[serde(default)]
    pub amount: Option<Euros>,
    #[serde(default)]
    pub item_ids: Vec<Uuid>,
}

fn refund_choice(staff: &Staff, body: &CancelBody) -> AppResult<RefundChoice> {
    let choice = match body.refund.as_deref().unwrap_or("policy") {
        "policy" => RefundChoice::Policy,
        "none" => RefundChoice::None,
        "custom" => RefundChoice::Custom(
            body.amount
                .ok_or_else(|| AppError::BadRequest("amount required".into()))?
                .0,
        ),
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown refund choice {other}"
            )));
        }
    };
    if !matches!(choice, RefundChoice::None) {
        staff.require("payments.refund")?;
    }
    Ok(choice)
}

pub async fn cancel(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<CancelBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.cancel")?;
    let choice = refund_choice(&staff, &body)?;
    let outcome = manage::cancel(
        &state.db,
        &state.payments,
        id,
        CancelledBy::Staff,
        &staff.actor(),
        body.reason.clone(),
        choice,
        Utc::now(),
    )
    .await?;
    Ok(Json(json!(outcome)))
}

pub async fn cancel_items(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<CancelBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.cancel")?;
    if body.item_ids.is_empty() {
        return Err(AppError::BadRequest("item_ids required".into()));
    }
    let choice = refund_choice(&staff, &body)?;
    let outcome = manage::cancel_items(
        &state.db,
        &state.payments,
        id,
        &body.item_ids,
        &staff.actor(),
        choice,
        Utc::now(),
    )
    .await?;
    Ok(Json(json!(outcome)))
}

#[derive(Debug, Deserialize)]
pub struct NoteBody {
    pub body: String,
}

pub async fn add_note(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<NoteBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("bookings.write")?;
    let text = body.body.trim();
    if text.is_empty() || text.chars().count() > 5000 {
        return Err(AppError::BadRequest("1 to 5000 characters".into()));
    }
    let note = sqlx::query!(
        "INSERT INTO booking_notes (booking_id, author_id, body) VALUES ($1, $2, $3) RETURNING id, created_at",
        id,
        staff.id,
        text
    )
    .fetch_one(&state.db)
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": note.id, "at": note.created_at })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct PaymentBody {
    /// `cash`, `bank_transfer`, `card_terminal`, `other`.
    pub provider: PaymentProvider,
    pub purpose: PaymentPurpose,
    pub amount: Euros,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn record_payment(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<PaymentBody>,
) -> AppResult<(StatusCode, Json<Value>)> {
    staff.require("payments.record_offline")?;
    if !matches!(
        body.provider,
        PaymentProvider::Cash
            | PaymentProvider::BankTransfer
            | PaymentProvider::CardTerminal
            | PaymentProvider::Other
    ) {
        return Err(AppError::BadRequest(
            "online payments are recorded by their provider".into(),
        ));
    }
    let payment_id = payments::record_offline(
        &state.db,
        id,
        body.provider,
        body.purpose,
        body.amount.0,
        staff.id,
        body.note.as_deref(),
        Utc::now(),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "payment_id": payment_id })),
    ))
}

#[derive(Debug, Deserialize)]
pub struct RefundBody {
    pub payment_id: Uuid,
    pub amount: Euros,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn refund(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<RefundBody>,
) -> AppResult<Json<Value>> {
    staff.require("payments.refund")?;
    let belongs = sqlx::query_scalar!(
        "SELECT booking_id = $2 AS \"ok!\" FROM payments WHERE id = $1",
        body.payment_id,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !belongs {
        return Err(AppError::NotFound);
    }
    let outcome = payments::refund(
        &state.db,
        &state.payments,
        body.payment_id,
        body.amount.0,
        body.reason.as_deref(),
        Some(staff.id),
        Utc::now(),
    )
    .await?;
    Ok(Json(json!(outcome)))
}

#[derive(Debug, Deserialize)]
pub struct ResendBody {
    /// `confirmation`, `payment_link`, `balance`, `precheckin`.
    pub email: String,
}

/// Sends an email again (a new outbox event: the email log keeps one row per event).
pub async fn resend(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<ResendBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    let event = match body.email.as_str() {
        "confirmation" => Event::BookingConfirmed { booking_id: id },
        "payment_link" => Event::BookingRequestAccepted { booking_id: id },
        "balance" => Event::BookingBalanceDue { booking_id: id },
        "precheckin" => Event::BookingPrecheckinDue { booking_id: id },
        "received" => Event::BookingCreated { booking_id: id },
        _ => return Err(AppError::BadRequest("unknown email".into())),
    };
    let mut tx = state.db.begin().await?;
    outbox::enqueue(&mut tx, &event).await?;
    audit::record(
        &mut tx,
        &staff.actor(),
        "booking.email_resent",
        "booking",
        &id.to_string(),
        json!({ "email": body.email }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "queued": true })))
}

pub async fn guests(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.read")?;
    let reveal = staff.can("guests.read_identity");
    let guests = register::load(
        &state.db,
        state.config.booking.guest_data_key.as_ref(),
        id,
        reveal,
    )
    .await?;
    if reveal {
        let mut conn = state.db.acquire().await?;
        audit::record(
            &mut conn,
            &staff.actor(),
            "booking.identity_viewed",
            "booking",
            &id.to_string(),
            json!({}),
        )
        .await?;
    }
    Ok(Json(json!({ "guests": guests, "revealed": reveal })))
}

#[derive(Debug, Deserialize)]
pub struct GuestsBody {
    pub guests: Vec<GuestEntry>,
}

pub async fn save_guests(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<GuestsBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    let complete = register::save(
        &state.db,
        state.config.booking.guest_data_key.as_ref(),
        id,
        &body.guests,
        &staff.actor(),
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "complete": complete })))
}

// ---- Tasks ------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TasksQuery {
    #[serde(default)]
    pub status: Option<String>,
}

pub async fn tasks(
    State(state): State<AppState>,
    staff: Staff,
    Query(q): Query<TasksQuery>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.read")?;
    let rows = sqlx::query!(
        r#"SELECT t.id, t.kind, t.status, t.title, t.details, t.due_at, t.created_at, t.booking_id,
                  b.reference AS "reference?"
           FROM staff_tasks t LEFT JOIN bookings b ON b.id = t.booking_id
           WHERE t.status = COALESCE($1, 'open') ORDER BY t.created_at DESC LIMIT 200"#,
        q.status
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|t| json!({ "id": t.id, "kind": t.kind, "status": t.status, "title": t.title, "details": t.details, "due_at": t.due_at, "created_at": t.created_at, "booking_id": t.booking_id, "reference": t.reference }))
        .collect::<Vec<_>>())))
}

#[derive(Debug, Deserialize)]
pub struct TaskBody {
    /// `done` or `dismissed` (or `open` to reopen).
    pub status: String,
}

pub async fn update_task(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<TaskBody>,
) -> AppResult<Json<Value>> {
    staff.require("bookings.write")?;
    if !matches!(body.status.as_str(), "open" | "done" | "dismissed") {
        return Err(AppError::BadRequest("open, done or dismissed".into()));
    }
    let updated = sqlx::query!(
        "UPDATE staff_tasks SET status = $2,
                resolved_by = CASE WHEN $2 = 'open' THEN NULL ELSE $3::uuid END,
                resolved_at = CASE WHEN $2 = 'open' THEN NULL ELSE now() END
         WHERE id = $1",
        id,
        body.status,
        staff.id
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Json(json!({ "status": body.status })))
}
