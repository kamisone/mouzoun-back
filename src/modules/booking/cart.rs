//! "Your trip": the basket, in Redis under an anonymous cart cookie (7 days). Adding to the
//! basket never reserves anything: inventory is held only when the guest starts paying. Every
//! read re-prices and re-checks availability, and tells the guest what changed since they last
//! saw it ("the quad tour went from 40 € to 45 €").

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State},
};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use rand::RngCore;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{booking_settings, trip_context};
use crate::{
    error::{AppError, AppResult},
    services::booking::{
        quote_token,
        trip::{self, TripItem, TripQuote, TripRequest},
    },
    state::AppState,
    web::client::RequestLocale,
};

const TTL_SECS: u64 = 7 * 24 * 3600;
const MAX_ITEMS: usize = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CartItem {
    pub id: Uuid,
    /// The stay (or activity) this item belongs to.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    #[serde(flatten)]
    pub item: TripItem,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cart {
    pub items: Vec<CartItem>,
    #[serde(default)]
    pub promo_code: Option<String>,
    #[serde(default)]
    pub rate_plan_id: Option<Uuid>,
    #[serde(default)]
    pub child_ages: Vec<u8>,
    /// Total of each item the last time the guest saw the basket (change notices).
    #[serde(default)]
    pub seen_totals: HashMap<Uuid, i64>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Cart {
    /// The trip request, and the cart item id of each request index.
    pub fn to_request(&self) -> (TripRequest, Vec<Uuid>) {
        let ids: Vec<Uuid> = self.items.iter().map(|i| i.id).collect();
        let items = self
            .items
            .iter()
            .map(|c| match &c.item {
                TripItem::Extra {
                    product_id,
                    date,
                    quantity,
                    ..
                } => TripItem::Extra {
                    product_id: *product_id,
                    parent: c.parent_id.and_then(|p| ids.iter().position(|i| *i == p)),
                    date: *date,
                    quantity: *quantity,
                },
                other => other.clone(),
            })
            .collect();
        (
            TripRequest {
                items,
                rate_plan_id: self.rate_plan_id,
                promo_code: self.promo_code.clone(),
                child_ages: self.child_ages.clone(),
            },
            ids,
        )
    }
}

fn cookie_name(secure: bool) -> &'static str {
    if secure { "__Host-mz_cart" } else { "mz_cart" }
}

fn redis_key(state: &AppState, id: &str) -> String {
    state.cache.key(&format!("cart:{id}"))
}

fn valid_id(id: &str) -> bool {
    id.len() == 22
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) async fn load(state: &AppState, jar: &CookieJar) -> (Option<String>, Cart) {
    let Some(id) = jar
        .get(cookie_name(state.config.cookie_secure))
        .map(|c| c.value().to_string())
        .filter(|id| valid_id(id))
    else {
        return (None, Cart::default());
    };
    let mut conn = state.cache.connection();
    let raw: Option<String> = conn.get(redis_key(state, &id)).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cart: redis read failed");
        None
    });
    let cart = raw
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    (Some(id), cart)
}

pub(crate) async fn save(
    state: &AppState,
    jar: CookieJar,
    id: Option<String>,
    cart: &Cart,
) -> AppResult<CookieJar> {
    let id = id.unwrap_or_else(|| {
        let mut raw = [0u8; 16];
        rand::rng().fill_bytes(&mut raw);
        URL_SAFE_NO_PAD.encode(raw)
    });
    let mut conn = state.cache.connection();
    let _: () = conn
        .set_ex(
            redis_key(state, &id),
            serde_json::to_string(cart).unwrap_or_default(),
            TTL_SECS,
        )
        .await?;
    let cookie = Cookie::build((cookie_name(state.config.cookie_secure), id))
        .http_only(true)
        .secure(state.config.cookie_secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::seconds(TTL_SECS as i64))
        .build();
    Ok(jar.add(cookie))
}

pub(crate) async fn clear(state: &AppState, jar: CookieJar) -> CookieJar {
    if let Some(id) = jar
        .get(cookie_name(state.config.cookie_secure))
        .map(|c| c.value().to_string())
    {
        let mut conn = state.cache.connection();
        let _: redis::RedisResult<()> = conn.del(redis_key(state, &id)).await;
    }
    jar.remove(
        Cookie::build(cookie_name(state.config.cookie_secure))
            .path("/")
            .build(),
    )
}

#[derive(Debug, Serialize)]
pub struct Notice {
    pub item_id: Uuid,
    /// `price_changed` or `unavailable`.
    pub code: &'static str,
    pub name: String,
    pub old_minor: Option<i64>,
    pub new_minor: Option<i64>,
    pub issue: Option<trip::ItemIssue>,
}

/// Prices the basket, builds the change notices and remembers the totals shown.
async fn view(
    state: &AppState,
    locale: &crate::db::types::Locale,
    cart: &mut Cart,
) -> AppResult<Value> {
    if cart.items.is_empty() {
        cart.seen_totals.clear();
        return Ok(
            json!({ "items": [], "count": 0, "quote": null, "quote_token": null, "notices": [] }),
        );
    }
    let settings = booking_settings(state).await?;
    let ctx = trip_context(locale.clone(), settings);
    let (request, ids) = cart.to_request();
    let mut conn = state.db.acquire().await?;
    let quote: TripQuote = trip::quote(&mut conn, &request, &ctx).await?;
    let mut notices = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        // The item and everything derived from it (mandatory extras, package parts).
        let parts: Vec<&trip::QuotedItem> = quote
            .items
            .iter()
            .filter(|q| {
                q.request_index == Some(index)
                    || q.parent_index.is_some_and(|p| {
                        quote.items[p].request_index == Some(index) && q.request_index.is_none()
                    })
            })
            .collect();
        let Some(main) = parts.iter().find(|p| p.request_index == Some(index)) else {
            continue;
        };
        if let Some(issue) = &main.issue {
            notices.push(Notice {
                item_id: *id,
                code: "unavailable",
                name: main.name.clone(),
                old_minor: cart.seen_totals.get(id).copied(),
                new_minor: None,
                issue: Some(issue.clone()),
            });
            continue;
        }
        let total: i64 = parts.iter().map(|p| p.total_minor).sum();
        if let Some(old) = cart.seen_totals.get(id)
            && *old != total
        {
            notices.push(Notice {
                item_id: *id,
                code: "price_changed",
                name: main.name.clone(),
                old_minor: Some(*old),
                new_minor: Some(total),
                issue: None,
            });
        }
        cart.seen_totals.insert(*id, total);
    }
    cart.seen_totals.retain(|k, _| ids.contains(k));
    let token = (!quote.has_issues).then(|| {
        quote_token::sign(
            &state.config.jwt_secret,
            &request,
            quote.total_minor,
            quote.currency.as_str(),
            Utc::now(),
        )
    });
    Ok(json!({
        "items": cart.items,
        "count": cart.items.iter().filter(|i| i.parent_id.is_none()).count(),
        "quote": quote,
        "quote_token": token,
        "notices": notices,
    }))
}

pub async fn get(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    jar: CookieJar,
) -> AppResult<(CookieJar, Json<Value>)> {
    let (id, mut cart) = load(&state, &jar).await;
    let body = view(&state, &locale, &mut cart).await?;
    let jar = if id.is_some() {
        save(&state, jar, id, &cart).await?
    } else {
        jar
    };
    Ok((jar, Json(body)))
}

#[derive(Debug, Deserialize)]
pub struct ItemBody {
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    #[serde(flatten)]
    pub item: TripItem,
}

fn check_parent(cart: &Cart, parent: Option<Uuid>) -> AppResult<()> {
    if let Some(p) = parent
        && !cart.items.iter().any(|i| i.id == p)
    {
        return Err(AppError::BadRequest("unknown parent item".into()));
    }
    Ok(())
}

pub async fn add(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    jar: CookieJar,
    Json(body): Json<ItemBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let (id, mut cart) = load(&state, &jar).await;
    if cart.items.len() >= MAX_ITEMS {
        return Err(AppError::BadRequest("the trip is full".into()));
    }
    check_parent(&cart, body.parent_id)?;
    let item_id = Uuid::now_v7();
    cart.items.push(CartItem {
        id: item_id,
        parent_id: body.parent_id,
        item: body.item,
    });
    cart.updated_at = Some(Utc::now());
    let mut view = view(&state, &locale, &mut cart).await?;
    view["added_id"] = json!(item_id);
    let jar = save(&state, jar, id, &cart).await?;
    Ok((jar, Json(view)))
}

pub async fn update_item(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    jar: CookieJar,
    Path(item_id): Path<Uuid>,
    Json(body): Json<ItemBody>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let (id, mut cart) = load(&state, &jar).await;
    check_parent(&cart, body.parent_id)?;
    let item = cart
        .items
        .iter_mut()
        .find(|i| i.id == item_id)
        .ok_or(AppError::NotFound)?;
    item.item = body.item;
    item.parent_id = body.parent_id.filter(|p| *p != item_id);
    cart.updated_at = Some(Utc::now());
    let view = view(&state, &locale, &mut cart).await?;
    let jar = save(&state, jar, id, &cart).await?;
    Ok((jar, Json(view)))
}

pub async fn remove_item(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    jar: CookieJar,
    Path(item_id): Path<Uuid>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let (id, mut cart) = load(&state, &jar).await;
    let before = cart.items.len();
    cart.items
        .retain(|i| i.id != item_id && i.parent_id != Some(item_id));
    if cart.items.len() == before {
        return Err(AppError::NotFound);
    }
    let view = view(&state, &locale, &mut cart).await?;
    let jar = save(&state, jar, id, &cart).await?;
    Ok((jar, Json(view)))
}

#[derive(Debug, Deserialize)]
pub struct CartOptions {
    /// `Some("")` removes the code.
    #[serde(default)]
    pub promo_code: Option<String>,
    #[serde(default)]
    pub rate_plan_id: Option<Option<Uuid>>,
    #[serde(default)]
    pub child_ages: Option<Vec<u8>>,
}

pub async fn update(
    State(state): State<AppState>,
    RequestLocale(locale): RequestLocale,
    jar: CookieJar,
    Json(body): Json<CartOptions>,
) -> AppResult<(CookieJar, Json<Value>)> {
    let (id, mut cart) = load(&state, &jar).await;
    if let Some(code) = body.promo_code {
        let code = code.trim().to_string();
        if code.len() > 40 {
            return Err(AppError::BadRequest("promo code too long".into()));
        }
        cart.promo_code = (!code.is_empty()).then_some(code);
    }
    if let Some(plan) = body.rate_plan_id {
        cart.rate_plan_id = plan;
    }
    if let Some(ages) = body.child_ages {
        cart.child_ages = ages.into_iter().take(20).collect();
    }
    let view = view(&state, &locale, &mut cart).await?;
    let jar = save(&state, jar, id, &cart).await?;
    Ok((jar, Json(view)))
}

pub async fn delete(State(state): State<AppState>, jar: CookieJar) -> (CookieJar, Json<Value>) {
    let jar = clear(&state, jar).await;
    (
        jar,
        Json(json!({ "items": [], "count": 0, "quote": null, "notices": [] })),
    )
}
