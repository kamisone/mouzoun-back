//! Back office of what guests book (`/v1/admin/catalog/...`): stays (accommodation types and
//! their units), activities (schedules and departures), packages and extras, with their prices.
//! See back/docs/catalog/README.md.
//!
//! | area | read | write |
//! |---|---|---|
//! | content, media, SEO, organisation, status, itinerary, package items | products.read | products.write |
//! | price rules, product taxes, simulator, seasons, rate plans, guest categories, policies | products.read / pricing.read | pricing.write |
//! | units, calendar, closures, schedules, departures, resource requirements, resources | inventory.read | inventory.write |
//!
//! Availability and prices are never computed here: the calendar and slot generators live in
//! `services::inventory`, the simulator and the package check call the booking funnel's
//! `services::booking::trip::quote`.

mod activities;
mod packages;
mod pricing;
mod products;
mod settings;
mod stays;

use std::collections::BTreeMap;

use axum::{
    Router,
    http::StatusCode,
    routing::{delete, get, patch, post, put},
};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    error::{AppError, AppResult},
    services::{
        audit::{self, Actor},
        outbox::{self, Event},
    },
    state::AppState,
};

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        // Shared settings first: their static segments win over `{kind}`.
        .route("/catalog/references", get(settings::references))
        .route(
            "/catalog/pricing/seasons",
            get(settings::seasons).post(settings::create_season),
        )
        .route(
            "/catalog/pricing/seasons/{id}",
            put(settings::update_season).delete(settings::delete_season),
        )
        .route(
            "/catalog/pricing/rate-plans",
            get(settings::rate_plans).post(settings::create_rate_plan),
        )
        .route(
            "/catalog/pricing/rate-plans/{id}",
            put(settings::update_rate_plan).delete(settings::delete_rate_plan),
        )
        .route(
            "/catalog/pricing/guest-categories",
            get(settings::guest_categories).post(settings::create_guest_category),
        )
        .route(
            "/catalog/pricing/guest-categories/{id}",
            put(settings::update_guest_category).delete(settings::delete_guest_category),
        )
        .route(
            "/catalog/pricing/cancellation-policies",
            get(settings::policies).post(settings::create_policy),
        )
        .route(
            "/catalog/pricing/cancellation-policies/{id}",
            put(settings::update_policy).delete(settings::delete_policy),
        )
        .route(
            "/catalog/resources",
            get(settings::resources).post(settings::create_resource),
        )
        .route(
            "/catalog/resources/{id}",
            put(settings::update_resource).delete(settings::delete_resource),
        )
        .route("/catalog/guides", get(activities::guides))
        .route("/catalog/slots/{slot_id}", patch(activities::patch_slot))
        // Products of one kind (`stays`, `activities`, `packages`, `extras`).
        .route(
            "/catalog/{kind}",
            get(products::list).post(products::create),
        )
        .route("/catalog/{kind}/order", put(products::reorder))
        .route("/catalog/{kind}/bulk", post(products::bulk))
        .route(
            "/catalog/{kind}/{id}",
            get(products::detail)
                .patch(products::update)
                .delete(products::delete),
        )
        .route("/catalog/{kind}/{id}/duplicate", post(products::duplicate))
        .route("/catalog/{kind}/{id}/publish", post(products::publish))
        .route("/catalog/{kind}/{id}/unpublish", post(products::unpublish))
        .route("/catalog/{kind}/{id}/archive", post(products::archive))
        .route(
            "/catalog/{kind}/{id}/publish-checklist",
            get(products::checklist),
        )
        .route(
            "/catalog/{kind}/{id}/itinerary",
            get(products::itinerary).put(products::put_itinerary),
        )
        // Stays
        .route(
            "/catalog/{kind}/{id}/units",
            get(stays::units).post(stays::create_unit),
        )
        .route("/catalog/{kind}/{id}/units/bulk", post(stays::bulk_units))
        .route(
            "/catalog/{kind}/{id}/units/order",
            put(stays::reorder_units),
        )
        .route(
            "/catalog/{kind}/{id}/units/{unit_id}",
            patch(stays::update_unit).delete(stays::delete_unit),
        )
        .route("/catalog/{kind}/{id}/inventory", put(stays::set_inventory))
        .route(
            "/catalog/{kind}/{id}/calendar",
            get(stays::calendar).patch(stays::patch_calendar),
        )
        .route(
            "/catalog/{kind}/{id}/closures",
            get(stays::closures).post(stays::create_closure),
        )
        .route(
            "/catalog/{kind}/{id}/closures/{closure_id}",
            delete(stays::delete_closure),
        )
        // Activities
        .route(
            "/catalog/{kind}/{id}/schedules",
            get(activities::schedules).put(activities::put_schedules),
        )
        .route(
            "/catalog/{kind}/{id}/schedules/preview",
            get(activities::preview_saved).post(activities::preview),
        )
        .route(
            "/catalog/{kind}/{id}/slots",
            get(activities::slots).post(activities::create_slot),
        )
        .route(
            "/catalog/{kind}/{id}/resource-requirements",
            get(activities::requirements).put(activities::put_requirements),
        )
        // Packages
        .route(
            "/catalog/{kind}/{id}/items",
            get(packages::items).put(packages::put_items),
        )
        .route(
            "/catalog/{kind}/{id}/availability",
            get(packages::availability),
        )
        // Pricing
        .route(
            "/catalog/{kind}/{id}/prices",
            get(pricing::prices).put(pricing::put_prices),
        )
        .route("/catalog/{kind}/{id}/price-quote", post(pricing::quote))
        .route("/catalog/{kind}/{id}/taxes", put(pricing::put_taxes))
}

// ---- Kinds ----------------------------------------------------------------------------------------

/// The URL segment of a kind and its `products.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stays,
    Activities,
    Packages,
    Extras,
}

impl Kind {
    pub fn parse(segment: &str) -> AppResult<Self> {
        Ok(match segment {
            "stays" => Self::Stays,
            "activities" => Self::Activities,
            "packages" => Self::Packages,
            "extras" => Self::Extras,
            _ => return Err(AppError::NotFound),
        })
    }

    pub fn product_kind(self) -> &'static str {
        match self {
            Self::Stays => "accommodation",
            Self::Activities => "activity",
            Self::Packages => "package",
            Self::Extras => "extra",
        }
    }

    pub fn from_product_kind(kind: &str) -> Option<Self> {
        Some(match kind {
            "accommodation" => Self::Stays,
            "activity" => Self::Activities,
            "package" => Self::Packages,
            "extra" => Self::Extras,
            _ => return None,
        })
    }

    pub fn segment(self) -> &'static str {
        match self {
            Self::Stays => "stays",
            Self::Activities => "activities",
            Self::Packages => "packages",
            Self::Extras => "extras",
        }
    }

    /// Its sitemap file (`None`: extras have no page).
    pub fn sitemap(self) -> Option<&'static str> {
        match self {
            Self::Stays => Some("accommodations"),
            Self::Activities => Some("activities"),
            Self::Packages => Some("packages"),
            Self::Extras => None,
        }
    }

    /// Entity type in the route registry (`None` for extras).
    pub fn entity(self) -> Option<&'static str> {
        crate::modules::seo::routes::product_entity(self.product_kind())
    }

    /// Pricing units a price rule of this kind may use (`requires_date` extras may use the
    /// nightly ones).
    pub fn allowed_units(self, requires_date: bool) -> &'static [&'static str] {
        match self {
            Self::Stays => &["per_unit_per_night", "per_person_per_night"],
            Self::Activities => &["per_person", "per_unit", "per_booking", "per_hour"],
            Self::Packages => &["per_person", "per_unit", "per_booking"],
            Self::Extras if requires_date => &[
                "per_unit_per_night",
                "per_person_per_night",
                "per_person",
                "per_unit",
                "per_booking",
                "per_hour",
                "per_day",
            ],
            Self::Extras => &[
                "per_person",
                "per_unit",
                "per_booking",
                "per_hour",
                "per_day",
            ],
        }
    }
}

/// The product `id` of `kind`, not deleted (404 otherwise).
pub async fn product_of_kind(conn: &mut PgConnection, kind: Kind, id: Uuid) -> AppResult<()> {
    let found = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM products WHERE id = $1 AND kind = $2 AND deleted_at IS NULL) AS "x!""#,
        id,
        kind.product_kind()
    )
    .fetch_one(conn)
    .await?;
    if found {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

// ---- Errors ---------------------------------------------------------------------------------------

/// `422 invalid_fields` with `details.fields = {"translations.ar.slug": "slug_taken", ...}`.
#[derive(Debug, Default)]
pub struct Fields(BTreeMap<String, String>);

impl Fields {
    pub fn add(&mut self, path: impl Into<String>, code: &str) {
        self.0
            .entry(path.into())
            .or_insert_with(|| code.to_string());
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn check(self) -> AppResult<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid_fields(self.0))
        }
    }
}

pub fn invalid_fields(fields: BTreeMap<String, String>) -> AppError {
    AppError::domain(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_fields",
        "some fields are not valid",
    )
    .with_details(json!({ "fields": fields }))
}

pub fn field_error(path: &str, code: &str) -> AppError {
    invalid_fields(BTreeMap::from([(path.to_string(), code.to_string())]))
}

/// A change refused because bookings depend on it (the list goes to the admin).
pub fn blocked_by_bookings(
    code: &'static str,
    message: &str,
    bookings: &[crate::services::inventory::BookingRef],
    extra: Value,
) -> AppError {
    let mut details = json!({ "bookings": bookings });
    if let (Some(d), Value::Object(e)) = (details.as_object_mut(), extra) {
        d.extend(e);
    }
    AppError::domain(StatusCode::CONFLICT, code, message).with_details(details)
}

// ---- After every write ---------------------------------------------------------------------------

/// Audit row + outbox events: the product page, its kind's listings and "from" prices
/// (`catalog:` cache, `Event::ProductUpdated`) and its kind's sitemap file.
pub async fn changed(
    conn: &mut PgConnection,
    actor: &Actor,
    kind: Kind,
    product_id: Uuid,
    action: &str,
    changes: Value,
) -> AppResult<()> {
    audit::record(
        conn,
        actor,
        action,
        "product",
        &product_id.to_string(),
        changes,
    )
    .await?;
    outbox::enqueue(conn, &Event::ProductUpdated { product_id }).await?;
    if let Some(sitemap) = kind.sitemap() {
        crate::modules::seo::redirects::changed(conn, false, &[sitemap]).await?;
    }
    Ok(())
}

/// Audit row for a shared pricing or inventory setting (seasons, rate plans...): every product
/// price may change, so the whole catalogue cache goes.
pub async fn settings_changed(
    conn: &mut PgConnection,
    actor: &Actor,
    action: &str,
    entity: &str,
    id: Uuid,
    changes: Value,
) -> AppResult<()> {
    audit::record(conn, actor, action, entity, &id.to_string(), changes).await?;
    outbox::enqueue(
        conn,
        &Event::ProductUpdated {
            product_id: Uuid::nil(),
        },
    )
    .await?;
    Ok(())
}
