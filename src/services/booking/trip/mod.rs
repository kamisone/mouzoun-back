//! A trip = one booking holding stays, activities, extras and packages, with one reference, one
//! total, one payment and one confirmation.
//!
//! The same code prices a basket (`quote`, read-only) and creates the booking (`create`, in one
//! transaction that locks the inventory first), so what the guest sees is exactly what the
//! checkout recomputes. The browser never sends prices the server trusts: checkout compares
//! its own total with the total the guest accepted and answers `price_changed` when they
//! differ.

mod availability;
pub mod catalog;
mod create;
mod model;
pub mod pricing;

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

pub use availability::TripUsage;
pub use create::{CheckoutInput, CreatedTrip, create};
pub use model::*;

use self::{
    availability::Check,
    catalog::{GuestCategory, Product, RatePlan},
    pricing::{PricingInput, TripFacts},
};
use super::{BookingError, settings::BookingSettings};
use crate::{
    camp_time,
    db::{
        catalog::{
            BookingMode, ExtraAppliesTo, PackagePricingMode, ProductKind, ProductStatus,
            SchedulingMode,
        },
        foundation,
        types::{CurrencyCode, Locale},
    },
    services::pricing::{self as engine_pricing, GuestCount, ServiceTime, engine::PromotionRule},
};

/// Who prices, when, and under which settings.
#[derive(Debug, Clone)]
pub struct TripContext {
    pub locale: Locale,
    /// Public website (published products, public rate plans only) vs back-office.
    pub public: bool,
    pub now: DateTime<Utc>,
    pub settings: BookingSettings,
}

impl TripContext {
    pub fn today(&self) -> NaiveDate {
        camp_time::today(self.now)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Service {
    Stay {
        arrival: NaiveDate,
        departure: NaiveDate,
        requested_units: Vec<Uuid>,
    },
    Slot {
        slot_id: Uuid,
    },
    /// `on_request` / `anytime` activities (time = preference), dated extras.
    OnDate {
        date: NaiveDate,
        time: Option<NaiveTime>,
    },
    Undated,
    Package {
        arrival: NaiveDate,
    },
}

/// One line of the trip once products are known and packages / mandatory extras expanded.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub request_index: Option<usize>,
    pub parent: Option<usize>,
    pub product: Product,
    pub mandatory: bool,
    pub in_package: bool,
    pub quantity: i32,
    /// Per-person extras repeated (one per morning...).
    pub repeat: i32,
    pub guests: Vec<GuestCount>,
    pub service: Service,
    pub language: Option<String>,
    pub remaining: Option<i32>,
    pub unit_ids: Vec<Uuid>,
    pub resource_ids: Vec<Uuid>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub issue: Option<ItemIssue>,
}

impl Resolved {
    fn new(product: Product, service: Service, guests: Vec<GuestCount>, quantity: i32) -> Self {
        Self {
            request_index: None,
            parent: None,
            product,
            mandatory: false,
            in_package: false,
            quantity,
            repeat: 1,
            guests,
            service,
            language: None,
            remaining: None,
            unit_ids: vec![],
            resource_ids: vec![],
            starts_at: None,
            ends_at: None,
            issue: None,
        }
    }

    fn fail(&mut self, issue: ItemIssue) {
        if self.issue.is_none() {
            self.issue = Some(issue);
        }
    }

    /// First date the item is used.
    pub fn first_date(&self) -> Option<NaiveDate> {
        match &self.service {
            Service::Stay { arrival, .. } | Service::Package { arrival } => Some(*arrival),
            Service::OnDate { date, .. } => Some(*date),
            Service::Slot { .. } => self.starts_at.map(camp_time::today),
            Service::Undated => None,
        }
    }

    pub fn last_date(&self) -> Option<NaiveDate> {
        match &self.service {
            Service::Stay { departure, .. } => Some(*departure),
            _ => self.first_date(),
        }
    }
}

/// Everything computed for a trip: the quote shown to the guest and the facts checkout needs.
pub(crate) struct Built {
    pub quote: TripQuote,
    pub resolved: Vec<Resolved>,
    pub rate_plan: Option<RatePlan>,
    pub promotions: Vec<(PromotionRule, Vec<(usize, i64)>)>,
    pub categories: Vec<GuestCategory>,
}

/// Prices a trip (read-only). Unavailable items are reported in `items[].issue`.
pub async fn quote(
    conn: &mut PgConnection,
    request: &TripRequest,
    ctx: &TripContext,
) -> Result<TripQuote, BookingError> {
    Ok(build(conn, request, ctx, false).await?.quote)
}

pub(crate) async fn build(
    conn: &mut PgConnection,
    request: &TripRequest,
    ctx: &TripContext,
    lock: bool,
) -> Result<Built, BookingError> {
    if request.items.is_empty() {
        return Err(BookingError::Invalid("the trip is empty".into()));
    }
    if request.items.len() > 30 {
        return Err(BookingError::Invalid("too many items".into()));
    }
    let ids: Vec<Uuid> = request.items.iter().map(TripItem::product_id).collect();
    let products = catalog::load_products(conn, &ids, &ctx.locale).await?;
    let categories = catalog::guest_categories(conn).await?;
    let mut resolved = resolve(conn, request, &products, &categories, ctx).await?;

    if lock {
        prelock(conn, &resolved).await?;
    }
    check_availability(conn, &mut resolved, ctx, lock).await?;

    let currency = foundation::default_currency(&mut *conn).await?.code;
    let rate_plan =
        catalog::rate_plan(conn, request.rate_plan_id, &ctx.locale, !ctx.public).await?;
    let built = price(
        conn, request, ctx, resolved, rate_plan, categories, &currency,
    )
    .await?;
    Ok(built)
}

fn capacity_guests(guests: &[GuestCount], categories: &[GuestCategory]) -> i32 {
    guests
        .iter()
        .filter(|g| {
            categories
                .iter()
                .find(|c| c.id == g.guest_category_id)
                .is_none_or(|c| c.counts_toward_capacity)
        })
        .map(|g| g.count)
        .sum()
}

fn validate_guests(guests: &[GuestCount], categories: &[GuestCategory]) -> Option<ItemIssue> {
    if guests.iter().any(|g| g.count < 0)
        || guests.iter().all(|g| g.count == 0)
        || guests.iter().map(|g| g.count).sum::<i32>() > 200
    {
        return Some(ItemIssue::new(
            "invalid_guests",
            "at least one guest is required",
        ));
    }
    if guests
        .iter()
        .any(|g| !categories.iter().any(|c| c.id == g.guest_category_id))
    {
        return Some(ItemIssue::new("invalid_guests", "unknown guest category"));
    }
    None
}

fn bookable(product: &Product, kind: ProductKind, ctx: &TripContext) -> Option<ItemIssue> {
    if product.kind != kind {
        return Some(ItemIssue::new("invalid_item", "wrong product kind"));
    }
    if product.booking_mode == BookingMode::NotBookable
        || (ctx.public && product.status != ProductStatus::Published)
    {
        return Some(ItemIssue::new(
            "not_bookable",
            "this cannot be booked online",
        ));
    }
    None
}

async fn resolve(
    conn: &mut PgConnection,
    request: &TripRequest,
    products: &HashMap<Uuid, Product>,
    categories: &[GuestCategory],
    ctx: &TripContext,
) -> Result<Vec<Resolved>, BookingError> {
    let missing = || ItemIssue::new("not_bookable", "this product no longer exists");
    let placeholder = |id: Uuid| Product {
        id,
        kind: ProductKind::Extra,
        status: ProductStatus::Archived,
        booking_mode: BookingMode::NotBookable,
        name: String::new(),
        slug: None,
        accommodation: None,
        activity: None,
        extra: None,
        package: None,
        mandatory_extras: vec![],
    };
    let mut out: Vec<Resolved> = Vec::new();
    let mut by_request: HashMap<usize, usize> = HashMap::new();
    let has_stay = request
        .items
        .iter()
        .any(|i| matches!(i, TripItem::Stay { .. } | TripItem::Package { .. }));

    for (index, item) in request.items.iter().enumerate() {
        let product = products.get(&item.product_id()).cloned();
        let position = out.len();
        by_request.insert(index, position);
        let mut r = match item {
            TripItem::Stay {
                arrival,
                departure,
                quantity,
                guests,
                unit_ids,
                ..
            } => {
                let mut r = Resolved::new(
                    product
                        .clone()
                        .unwrap_or_else(|| placeholder(item.product_id())),
                    Service::Stay {
                        arrival: *arrival,
                        departure: *departure,
                        requested_units: if ctx.settings.pitch_selection || !ctx.public {
                            unit_ids.clone()
                        } else {
                            vec![]
                        },
                    },
                    guests.clone(),
                    *quantity,
                );
                if let Some(p) = &product {
                    if let Some(issue) = bookable(p, ProductKind::Accommodation, ctx)
                        .or_else(|| validate_guests(guests, categories))
                    {
                        r.fail(issue);
                    } else if let Some(acc) = &p.accommodation {
                        let party = capacity_guests(guests, categories);
                        let max = acc.max_guests * (*quantity).max(1);
                        if party > max {
                            let units = (party + acc.max_guests - 1) / acc.max_guests;
                            r.fail(
                                ItemIssue::new(
                                    "too_many_guests",
                                    format!("sleeps up to {} guests", acc.max_guests),
                                )
                                .with(serde_json::json!({
                                    "max_guests": acc.max_guests, "suggested_quantity": units
                                })),
                            );
                        }
                        let count_of = |code: &str| -> i32 {
                            guests
                                .iter()
                                .filter(|g| {
                                    categories
                                        .iter()
                                        .any(|c| c.id == g.guest_category_id && c.code == code)
                                })
                                .map(|g| g.count)
                                .sum()
                        };
                        if let Some(m) = acc.max_adults
                            && count_of("adult") > m * (*quantity).max(1)
                        {
                            r.fail(
                                ItemIssue::new("too_many_adults", format!("at most {m} adults"))
                                    .with(serde_json::json!({ "max_adults": m })),
                            );
                        }
                        if let Some(m) = acc.max_children
                            && count_of("child") > m * (*quantity).max(1)
                        {
                            r.fail(
                                ItemIssue::new(
                                    "too_many_children",
                                    format!("at most {m} children"),
                                )
                                .with(serde_json::json!({ "max_children": m })),
                            );
                        }
                    }
                } else {
                    r.fail(missing());
                }
                r
            }
            TripItem::Activity {
                slot_id,
                date,
                time,
                guests,
                quantity,
                language,
                ..
            } => {
                let p = product
                    .clone()
                    .unwrap_or_else(|| placeholder(item.product_id()));
                let service = match (slot_id, date) {
                    (Some(slot_id), _) => Service::Slot { slot_id: *slot_id },
                    (None, Some(date)) => Service::OnDate {
                        date: *date,
                        time: *time,
                    },
                    (None, None) => Service::Undated,
                };
                let mut r = Resolved::new(p.clone(), service, guests.clone(), *quantity);
                r.language = language.clone();
                if product.is_none() {
                    r.fail(missing());
                } else if let Some(issue) = bookable(&p, ProductKind::Activity, ctx)
                    .or_else(|| validate_guests(guests, categories))
                {
                    r.fail(issue);
                } else if let Some(act) = &p.activity {
                    let participants = capacity_guests(guests, categories);
                    match (&r.service, act.scheduling_mode) {
                        (Service::Undated, _) => {
                            r.fail(ItemIssue::new("date_required", "choose a date"));
                        }
                        (Service::Slot { .. }, SchedulingMode::FixedSlots) => {}
                        (Service::Slot { .. }, _) => {
                            r.fail(ItemIssue::new(
                                "invalid_item",
                                "this activity has no fixed departures",
                            ));
                        }
                        (Service::OnDate { .. }, SchedulingMode::FixedSlots) => {
                            r.fail(ItemIssue::new("slot_required", "choose a departure time"));
                        }
                        _ => {}
                    }
                    if participants < act.min_participants {
                        r.fail(
                            ItemIssue::new(
                                "min_participants",
                                format!("at least {} participants", act.min_participants),
                            )
                            .with(serde_json::json!({ "min": act.min_participants })),
                        );
                    }
                    if let Some(max) = act.max_participants
                        && participants > max
                    {
                        r.fail(
                            ItemIssue::new(
                                "max_participants",
                                format!("at most {max} participants"),
                            )
                            .with(serde_json::json!({ "max": max })),
                        );
                    }
                    if let Some(min_age) = act.min_age {
                        let too_young_category = guests.iter().filter(|g| g.count > 0).any(|g| {
                            categories.iter().any(|c| {
                                c.id == g.guest_category_id
                                    && c.max_age.is_some_and(|m| m < min_age)
                            })
                        });
                        let too_young_child =
                            request.child_ages.iter().any(|a| i16::from(*a) < min_age);
                        if too_young_category || too_young_child {
                            r.fail(
                                ItemIssue::new("min_age", format!("minimum age {min_age}"))
                                    .with(serde_json::json!({ "min_age": min_age })),
                            );
                        }
                    }
                    if let Some(lang) = language
                        && !act.guide_languages.is_empty()
                        && !act.guide_languages.contains(lang)
                    {
                        r.fail(ItemIssue::new(
                            "language_unavailable",
                            "this language is not offered",
                        ));
                    }
                    if ctx.public
                        && !has_stay
                        && (!act.standalone_bookable || !ctx.settings.standalone_activities)
                    {
                        r.fail(ItemIssue::new("needs_stay", "only bookable with a stay"));
                    }
                }
                r
            }
            TripItem::Extra {
                parent,
                date,
                quantity,
                ..
            } => {
                let p = product
                    .clone()
                    .unwrap_or_else(|| placeholder(item.product_id()));
                let parent_position =
                    parent.and_then(|i| (i < index).then(|| by_request.get(&i).copied()).flatten());
                let parent_guests = parent_position
                    .map(|i| out[i].guests.clone())
                    .or_else(|| {
                        out.iter()
                            .find(|o| matches!(o.service, Service::Stay { .. }))
                            .map(|o| o.guests.clone())
                    })
                    .unwrap_or_default();
                let service = match date {
                    Some(d) => Service::OnDate {
                        date: *d,
                        time: None,
                    },
                    None => Service::Undated,
                };
                let mut r = Resolved::new(p.clone(), service, parent_guests, *quantity);
                r.parent = parent_position;
                if product.is_none() {
                    r.fail(missing());
                } else if let Some(issue) = bookable(&p, ProductKind::Extra, ctx) {
                    r.fail(issue);
                } else if parent.is_some() && parent_position.is_none() {
                    r.fail(ItemIssue::new("invalid_item", "unknown parent item"));
                } else if let Some(extra) = &p.extra {
                    if extra.requires_date && date.is_none() {
                        r.fail(ItemIssue::new("date_required", "choose a date"));
                    }
                    let parent_kind = parent_position.map(|i| out[i].product.kind);
                    let fits = match extra.applies_to {
                        ExtraAppliesTo::Any | ExtraAppliesTo::Booking => true,
                        ExtraAppliesTo::Accommodation => {
                            parent_kind.is_none_or(|k| k == ProductKind::Accommodation)
                        }
                        ExtraAppliesTo::Activity => {
                            parent_kind.is_none_or(|k| k == ProductKind::Activity)
                        }
                    };
                    if !fits {
                        r.fail(ItemIssue::new(
                            "invalid_item",
                            "this extra does not apply here",
                        ));
                    }
                    if let (Some(d), Some(pi)) = (date, parent_position)
                        && let Service::Stay {
                            arrival, departure, ..
                        } = out[pi].service
                        && (*d < arrival || *d > departure)
                    {
                        r.fail(ItemIssue::new(
                            "outside_stay",
                            "this date is outside the stay",
                        ));
                    }
                }
                r
            }
            TripItem::Package {
                arrival, guests, ..
            } => {
                let p = product
                    .clone()
                    .unwrap_or_else(|| placeholder(item.product_id()));
                let mut r = Resolved::new(
                    p.clone(),
                    Service::Package { arrival: *arrival },
                    guests.clone(),
                    1,
                );
                if product.is_none() {
                    r.fail(missing());
                } else if let Some(issue) = bookable(&p, ProductKind::Package, ctx)
                    .or_else(|| validate_guests(guests, categories))
                {
                    r.fail(issue);
                }
                r
            }
        };
        r.request_index = Some(index);
        let product = r.product.clone();
        out.push(r);

        // Package parts follow the package; mandatory extras and the pitch fee follow their item.
        if let Some(pk) = &product.package
            && let TripItem::Package {
                arrival, guests, ..
            } = item
        {
            let participants = capacity_guests(guests, categories);
            for part in pk.items.iter().filter(|p| !p.is_optional) {
                let Some(part_product) = products.get(&part.product_id).cloned() else {
                    out[position].fail(missing());
                    continue;
                };
                let day = *arrival + Duration::days(i64::from(part.day_number.unwrap_or(1) - 1));
                let mut child = match part_product.kind {
                    ProductKind::Accommodation => {
                        let max_guests = part_product
                            .accommodation
                            .as_ref()
                            .map_or(1, |a| a.max_guests.max(1));
                        let nights = pk.duration_nights.unwrap_or(part.quantity).max(1);
                        Resolved::new(
                            part_product,
                            Service::Stay {
                                arrival: day,
                                departure: day + Duration::days(i64::from(nights)),
                                requested_units: vec![],
                            },
                            guests.clone(),
                            ((participants + max_guests - 1) / max_guests).max(1),
                        )
                    }
                    ProductKind::Activity => {
                        let service =
                            match part_product.activity.as_ref().map(|a| a.scheduling_mode) {
                                Some(SchedulingMode::FixedSlots) => {
                                    match availability::first_slot_on(
                                        conn,
                                        part_product.id,
                                        day,
                                        participants,
                                    )
                                    .await?
                                    {
                                        Some(slot_id) => Service::Slot { slot_id },
                                        None => Service::Undated,
                                    }
                                }
                                _ => Service::OnDate {
                                    date: day,
                                    time: None,
                                },
                            };
                        let mut c = Resolved::new(part_product, service, guests.clone(), 1);
                        if matches!(c.service, Service::Undated) {
                            c.fail(ItemIssue::new("full", "no departure left on this day"));
                        }
                        c
                    }
                    _ => {
                        let mut c = Resolved::new(
                            part_product,
                            Service::OnDate {
                                date: day,
                                time: None,
                            },
                            guests.clone(),
                            1,
                        );
                        c.repeat = part.quantity.max(1);
                        c
                    }
                };
                child.parent = Some(position);
                child.in_package = true;
                out.push(child);
            }
        }
        let mut fees: Vec<(Uuid, i32)> =
            product.mandatory_extras.iter().map(|id| (*id, 1)).collect();
        if let (Some(acc), TripItem::Stay { unit_ids, .. }) = (&product.accommodation, item)
            && !unit_ids.is_empty()
            && let Some(fee) = acc.pitch_selection_extra_id
        {
            fees.push((fee, i32::try_from(unit_ids.len()).unwrap_or(1)));
        }
        let parent_service = out[position].service.clone();
        let parent_guests = out[position].guests.clone();
        for (extra_id, quantity) in fees {
            let Some(extra) = products.get(&extra_id).cloned() else {
                continue;
            };
            let service = match parent_service {
                Service::Stay { arrival, .. } | Service::Package { arrival } => Service::OnDate {
                    date: arrival,
                    time: None,
                },
                Service::OnDate { date, .. } => Service::OnDate { date, time: None },
                _ => Service::Undated,
            };
            let mut child = Resolved::new(extra, service, parent_guests.clone(), quantity);
            child.parent = Some(position);
            child.mandatory = true;
            out.push(child);
        }
    }
    Ok(out)
}

/// Locks every inventory row the trip touches, in the global order (nights by product then
/// day, slots by id, limited extras by id), before any row is read for checking.
async fn prelock(conn: &mut PgConnection, items: &[Resolved]) -> Result<(), BookingError> {
    let mut stays: Vec<(Uuid, NaiveDate, NaiveDate)> = Vec::new();
    let mut slots: Vec<Uuid> = Vec::new();
    let mut extras: Vec<Uuid> = Vec::new();
    for item in items.iter().filter(|i| i.issue.is_none()) {
        match &item.service {
            Service::Stay {
                arrival, departure, ..
            } => match stays.iter_mut().find(|s| s.0 == item.product.id) {
                Some(s) => {
                    s.1 = s.1.min(*arrival);
                    s.2 = s.2.max(*departure);
                }
                None => stays.push((item.product.id, *arrival, *departure)),
            },
            Service::Slot { slot_id } => slots.push(*slot_id),
            _ => {}
        }
        if item
            .product
            .extra
            .as_ref()
            .is_some_and(|e| e.stock_quantity.is_some())
        {
            extras.push(item.product.id);
        }
    }
    stays.sort();
    for (product_id, from, to) in stays {
        sqlx::query!(
            "SELECT day FROM inventory_calendar WHERE product_id = $1 AND day >= $2 AND day <= $3
             ORDER BY day FOR UPDATE",
            product_id,
            from,
            to
        )
        .fetch_all(&mut *conn)
        .await?;
    }
    slots.sort();
    slots.dedup();
    if !slots.is_empty() {
        sqlx::query!(
            "SELECT id FROM activity_slots WHERE id = ANY($1) ORDER BY id FOR UPDATE",
            &slots
        )
        .fetch_all(&mut *conn)
        .await?;
    }
    extras.sort();
    extras.dedup();
    if !extras.is_empty() {
        sqlx::query!(
            "SELECT product_id FROM extras WHERE product_id = ANY($1) ORDER BY product_id FOR UPDATE",
            &extras
        )
        .fetch_all(&mut *conn)
        .await?;
    }
    Ok(())
}

// Index loop on purpose: each branch writes back into `items[i]` after awaiting a check.
#[allow(clippy::needless_range_loop)]
async fn check_availability(
    conn: &mut PgConnection,
    items: &mut [Resolved],
    ctx: &TripContext,
    lock: bool,
) -> Result<(), BookingError> {
    let mut usage = TripUsage::default();
    let categories = catalog::guest_categories(conn).await?;
    let today = ctx.today();
    for i in 0..items.len() {
        if items[i].issue.is_some() {
            continue;
        }
        let item = items[i].clone();
        let outcome: Check<()> = match &item.service {
            Service::Stay {
                arrival,
                departure,
                requested_units,
            } => availability::stay(
                conn,
                &item.product,
                *arrival,
                *departure,
                item.quantity,
                requested_units,
                &mut usage,
                lock,
                today,
                ctx.settings.max_days_ahead,
            )
            .await
            .map(|r| {
                r.map(|ok| {
                    items[i].remaining = Some(ok.remaining);
                    items[i].unit_ids = ok.unit_ids;
                })
            }),
            Service::Slot { slot_id } => {
                let participants = capacity_guests(&item.guests, &categories);
                match availability::slot(
                    conn,
                    &item.product,
                    *slot_id,
                    participants,
                    &mut usage,
                    lock,
                    ctx.now,
                )
                .await?
                {
                    Err(issue) => Ok(Err(issue)),
                    Ok(slot) => {
                        items[i].starts_at = Some(slot.starts_at);
                        items[i].ends_at = Some(slot.ends_at);
                        items[i].remaining = Some(slot.remaining);
                        if items[i].language.is_none() {
                            items[i].language = slot.language.clone();
                        }
                        availability::resources(
                            conn,
                            item.product.id,
                            participants,
                            slot.starts_at,
                            slot.ends_at,
                            &mut usage,
                            lock,
                        )
                        .await
                        .map(|r| r.map(|ids| items[i].resource_ids = ids))
                    }
                }
            }
            Service::OnDate { date, time } if item.product.kind == ProductKind::Activity => {
                let act = item.product.activity.clone();
                let duration = Duration::minutes(i64::from(
                    act.as_ref().and_then(|a| a.duration_minutes).unwrap_or(60),
                ));
                let local = date
                    .and_time(time.unwrap_or(NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default()));
                let starts_at = camp_time::to_utc(local);
                let cutoff = act.as_ref().map_or(0, |a| a.booking_cutoff_hours);
                if *date < today || starts_at - Duration::hours(i64::from(cutoff)) < ctx.now {
                    Ok(Err(ItemIssue::new(
                        "too_late",
                        "too late to book this date",
                    )
                    .with(serde_json::json!({ "cutoff_hours": cutoff }))))
                } else if availability::closed_on(conn, item.product.id, *date).await? {
                    Ok(Err(ItemIssue::new("closed", "closed on this date")))
                } else {
                    items[i].starts_at = Some(starts_at);
                    items[i].ends_at = Some(starts_at + duration);
                    let on_request = act
                        .as_ref()
                        .is_some_and(|a| a.scheduling_mode == SchedulingMode::OnRequest);
                    if on_request || time.is_none() {
                        // The camp confirms the time: resources are assigned by staff.
                        Ok(Ok(()))
                    } else {
                        let participants = capacity_guests(&item.guests, &categories);
                        availability::resources(
                            conn,
                            item.product.id,
                            participants,
                            starts_at,
                            starts_at + duration,
                            &mut usage,
                            lock,
                        )
                        .await
                        .map(|r| r.map(|ids| items[i].resource_ids = ids))
                    }
                }
            }
            Service::OnDate { date, .. } => {
                if *date < today {
                    Ok(Err(ItemIssue::new("past_date", "this date is in the past")))
                } else {
                    availability::extra_stock(
                        conn,
                        &item.product,
                        Some(*date),
                        item.quantity,
                        &mut usage,
                        lock,
                    )
                    .await
                    .map(|r| r.map(|left| items[i].remaining = left))
                }
            }
            Service::Undated if item.product.kind == ProductKind::Extra => {
                availability::extra_stock(
                    conn,
                    &item.product,
                    None,
                    item.quantity,
                    &mut usage,
                    lock,
                )
                .await
                .map(|r| r.map(|left| items[i].remaining = left))
            }
            Service::Undated => Ok(Err(ItemIssue::new("date_required", "choose a date"))),
            Service::Package { arrival } => {
                if *arrival < today {
                    Ok(Err(ItemIssue::new("past_date", "arrival is in the past")))
                } else {
                    Ok(Ok(()))
                }
            }
        };
        if let Err(issue) = outcome? {
            items[i].fail(issue);
        }
    }
    // A package is unavailable when one of its parts is.
    let failed_parts: Vec<(usize, ItemIssue)> = items
        .iter()
        .filter(|i| i.in_package)
        .filter_map(|i| Some((i.parent?, i.issue.clone()?)))
        .collect();
    for (parent, issue) in failed_parts {
        items[parent].fail(issue);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn price(
    conn: &mut PgConnection,
    request: &TripRequest,
    ctx: &TripContext,
    resolved: Vec<Resolved>,
    rate_plan: Option<RatePlan>,
    categories: Vec<GuestCategory>,
    currency: &CurrencyCode,
) -> Result<Built, BookingError> {
    let rate_plan_id = rate_plan.as_ref().map(|p| p.id);
    let inputs: Vec<PricingInput> = resolved
        .iter()
        .map(|r| PricingInput {
            product_id: r.product.id,
            kind: r.product.kind,
            service: match &r.service {
                Service::Stay {
                    arrival, departure, ..
                } => ServiceTime::Stay {
                    arrival: *arrival,
                    departure: *departure,
                },
                other => {
                    let date = match other {
                        Service::OnDate { date, .. } | Service::Package { arrival: date } => *date,
                        _ => r
                            .first_date()
                            .or_else(|| r.parent.and_then(|p| resolved[p].first_date()))
                            .unwrap_or_else(|| ctx.today()),
                    };
                    let minutes = r.product.activity.as_ref().and_then(|a| a.duration_minutes);
                    ServiceTime::Date {
                        date,
                        hours: minutes.filter(|m| m % 60 == 0).map(|m| m / 60),
                        days: Some(1),
                    }
                }
            },
            guests: r.guests.clone(),
            quantity: r.quantity.max(1),
            standard_occupancy: r
                .product
                .accommodation
                .as_ref()
                .map(|a| a.standard_occupancy),
            repeat: r.repeat,
            in_package: r.in_package,
            package: r
                .product
                .package
                .as_ref()
                .map(|p| (p.pricing_mode, p.discount_percent)),
            parent: r.parent,
        })
        .collect();

    // Base lines per item; an item without price becomes an issue, not an error.
    let mut base: Vec<(Vec<engine_pricing::engine::PriceLine>, i64)> =
        Vec::with_capacity(inputs.len());
    let mut issues: Vec<Option<ItemIssue>> = resolved.iter().map(|r| r.issue.clone()).collect();
    for (i, input) in inputs.iter().enumerate() {
        let priced = if input
            .package
            .is_some_and(|p| p.0 != PackagePricingMode::Fixed)
        {
            Ok((vec![], 0))
        } else {
            pricing::base_lines(conn, input, rate_plan_id, currency, ctx.now).await
        };
        match priced {
            Ok(b) => base.push(b),
            Err(BookingError::Pricing(e)) => {
                if issues[i].is_none() {
                    issues[i] = Some(ItemIssue::new("no_price", e.to_string()));
                }
                base.push((vec![], 0));
            }
            Err(e) => return Err(e),
        }
    }
    // Packages priced as the sum of their parts take the parts' prices.
    let mut package_discount: HashMap<usize, i64> = HashMap::new();
    for (i, input) in inputs.iter().enumerate() {
        if let Some((mode, percent)) = input.package
            && mode != PackagePricingMode::Fixed
        {
            let mut lines = Vec::new();
            let mut subtotal = 0;
            for (j, child) in inputs.iter().enumerate() {
                if child.parent == Some(i) && child.in_package {
                    let mut unpackaged = child.clone();
                    unpackaged.in_package = false;
                    match pricing::base_lines(conn, &unpackaged, rate_plan_id, currency, ctx.now)
                        .await
                    {
                        Ok((l, s)) => {
                            lines.extend(l);
                            subtotal += s;
                        }
                        Err(BookingError::Pricing(e)) => {
                            issues[j] = Some(ItemIssue::new("no_price", e.to_string()));
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            base[i] = (lines, subtotal);
            if mode == PackagePricingMode::SumMinusDiscount {
                package_discount.insert(
                    i,
                    crate::db::types::percent_of(subtotal, percent).unwrap_or(0),
                );
            }
        }
    }

    let starts_on = resolved.iter().filter_map(Resolved::first_date).min();
    let ends_on = resolved.iter().filter_map(Resolved::last_date).max();
    let stay_nights = resolved
        .iter()
        .filter_map(|r| match r.service {
            Service::Stay {
                arrival, departure, ..
            } => Some((departure - arrival).num_days()),
            Service::Package { .. } => r
                .product
                .package
                .as_ref()
                .and_then(|p| p.duration_nights)
                .map(i64::from),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    // Promotions (automatic offers + the code typed by the guest).
    let automatic = engine_pricing::load_automatic_promotions(conn).await?;
    let typed = request
        .promo_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    let code_rule = match typed {
        Some(code) => engine_pricing::load_promotions(conn, Some(code))
            .await?
            .into_iter()
            .next(),
        None => None,
    };
    let mut promo_ids: Vec<Uuid> = automatic.iter().map(|p| p.id).collect();
    promo_ids.extend(code_rule.iter().map(|p| p.id));
    let meta = pricing::promotion_meta(conn, &promo_ids, ctx.locale.as_str()).await?;
    let trip = TripFacts {
        currency,
        starts_on,
        stay_nights,
        booked_at: ctx.now,
        booked_on: ctx.today(),
    };
    // Items with issues never take a discount.
    let priceable: Vec<(Vec<engine_pricing::engine::PriceLine>, i64)> = base
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if issues[i].is_some() {
                (vec![], 0)
            } else {
                b.clone()
            }
        })
        .collect();
    let (chosen, promo_status) = pricing::choose(
        &automatic,
        typed.map(|t| (t, code_rule.as_ref())),
        &meta,
        &inputs,
        &priceable,
        &trip,
    );

    // Finish every item: discount, taxes, totals.
    let mut items: Vec<QuotedItem> = Vec::with_capacity(resolved.len());
    for (i, r) in resolved.iter().enumerate() {
        let mut discounts: Vec<AppliedDiscount> = chosen
            .iter()
            .filter_map(|(promo, amounts)| {
                amounts.iter().find(|a| a.0 == i).map(|a| AppliedDiscount {
                    promotion_id: promo.id,
                    code: promo.code.clone(),
                    name: meta.get(&promo.id).and_then(|m| m.name.clone()),
                    amount_minor: a.1,
                })
            })
            .collect();
        if let Some(saving) = package_discount.get(&i).filter(|s| **s > 0) {
            discounts.push(AppliedDiscount {
                promotion_id: Uuid::nil(),
                code: None,
                name: Some(r.product.name.clone()),
                amount_minor: *saving,
            });
        }
        let (lines, subtotal) = base[i].clone();
        let discount: i64 = discounts
            .iter()
            .map(|d| d.amount_minor)
            .sum::<i64>()
            .min(subtotal);
        let quote = if subtotal == 0 && lines.is_empty() {
            None
        } else {
            match pricing::finish_item(
                conn,
                &inputs[i],
                rate_plan_id,
                currency,
                lines.clone(),
                subtotal,
                discount,
                ctx.now,
            )
            .await
            {
                Ok(q) => Some(q),
                Err(BookingError::Pricing(e)) => {
                    if issues[i].is_none() {
                        issues[i] = Some(ItemIssue::new("no_price", e.to_string()));
                    }
                    None
                }
                Err(e) => return Err(e),
            }
        };
        let (taxes, tax_minor, included_tax_minor, total_minor) = match &quote {
            Some(q) => (
                q.taxes.clone(),
                q.tax_minor,
                q.taxes
                    .iter()
                    .filter(|t| t.inclusive)
                    .map(|t| t.amount_minor)
                    .sum(),
                q.total_minor,
            ),
            None => (vec![], 0, 0, 0),
        };
        let (arrival, departure, date, slot_id, time) = match &r.service {
            Service::Stay {
                arrival, departure, ..
            } => (Some(*arrival), Some(*departure), None, None, None),
            Service::Slot { slot_id } => (None, None, r.first_date(), Some(*slot_id), None),
            Service::OnDate { date, time } => (None, None, Some(*date), None, *time),
            Service::Package { arrival } => {
                let nights = r
                    .product
                    .package
                    .as_ref()
                    .and_then(|p| p.duration_nights)
                    .unwrap_or(0);
                (
                    Some(*arrival),
                    Some(*arrival + Duration::days(i64::from(nights))),
                    None,
                    None,
                    None,
                )
            }
            Service::Undated => (None, None, None, None, None),
        };
        items.push(QuotedItem {
            index: i,
            request_index: r.request_index,
            parent_index: r.parent,
            kind: r.product.kind,
            product_id: r.product.id,
            name: r.product.name.clone(),
            slug: r.product.slug.clone(),
            mandatory: r.mandatory,
            in_package: r.in_package,
            on_request: r.product.on_request(),
            quantity: r.quantity,
            guests: r.guests.clone(),
            arrival,
            departure,
            date,
            starts_at: r.starts_at,
            ends_at: r.ends_at,
            slot_id,
            time,
            language: r.language.clone(),
            unit_ids: r.unit_ids.clone(),
            remaining: r.remaining,
            lines,
            subtotal_minor: subtotal,
            discount_minor: if quote.is_some() { discount } else { 0 },
            discounts: if quote.is_some() { discounts } else { vec![] },
            tax_minor,
            included_tax_minor,
            taxes,
            total_minor,
            issue: issues[i].clone(),
        });
    }

    let sum = |f: fn(&QuotedItem) -> i64| -> i64 {
        items.iter().filter(|i| i.issue.is_none()).map(f).sum()
    };
    let subtotal_minor = sum(|i| i.subtotal_minor);
    let discount_minor = sum(|i| i.discount_minor);
    let tax_minor = sum(|i| i.tax_minor);
    let included_tax_minor = sum(|i| i.included_tax_minor);
    let total_minor = sum(|i| i.total_minor);
    let mut discounts: Vec<AppliedDiscount> = Vec::new();
    for d in items
        .iter()
        .filter(|i| i.issue.is_none())
        .flat_map(|i| i.discounts.iter())
    {
        match discounts
            .iter_mut()
            .find(|x| x.promotion_id == d.promotion_id && x.name == d.name)
        {
            Some(x) => x.amount_minor += d.amount_minor,
            None => discounts.push(d.clone()),
        }
    }
    let is_request = items.iter().any(|i| i.on_request && i.issue.is_none());
    let has_issues = items.iter().any(|i| i.issue.is_some());
    let trip_start = trip_start(&resolved);
    let cancellation = pricing::cancellation_steps(
        rate_plan
            .as_ref()
            .and_then(|p| p.cancellation_rules.as_ref()),
        trip_start,
    );
    let payment_options = pricing::payment_options(
        rate_plan.as_ref(),
        total_minor,
        currency,
        starts_on,
        ctx.today(),
        ctx.settings.balance_due_days,
    );
    let quote = TripQuote {
        currency: currency.clone(),
        items,
        rate_plan: rate_plan.as_ref().map(|p| RatePlanSummary {
            id: p.id,
            code: p.code.clone(),
            name: p.name.clone(),
            meal_plan: p.meal_plan,
            payment_mode: p.payment_mode,
        }),
        subtotal_minor,
        discount_minor,
        discounts,
        tax_minor,
        included_tax_minor,
        total_minor,
        payment_options,
        is_request,
        instant_confirmation: !is_request,
        cancellation,
        promo: promo_status,
        starts_on,
        ends_on,
        has_issues,
    };
    let promotions = if has_issues { vec![] } else { chosen };
    Ok(Built {
        quote,
        resolved,
        rate_plan,
        promotions,
        categories,
    })
}

/// The moment the trip starts in absolute time: check-in of the first stay (camp time) or the
/// first activity departure.
pub(crate) fn trip_start(items: &[Resolved]) -> Option<DateTime<Utc>> {
    items.iter().filter_map(item_start).min()
}

pub(crate) fn item_start(item: &Resolved) -> Option<DateTime<Utc>> {
    match &item.service {
        Service::Stay { arrival, .. } | Service::Package { arrival } => {
            let at = item
                .product
                .accommodation
                .as_ref()
                .and_then(|a| a.check_in_from)
                .unwrap_or(pricing::DEFAULT_CHECK_IN);
            Some(camp_time::to_utc(arrival.and_time(at)))
        }
        Service::Slot { .. } | Service::OnDate { .. } => item.starts_at.or_else(|| {
            item.first_date()
                .map(|d| camp_time::to_utc(d.and_time(pricing::DEFAULT_CHECK_IN)))
        }),
        Service::Undated => None,
    }
}
