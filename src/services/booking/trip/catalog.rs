//! What the trip engine needs to know about products, guest categories and rate plans.

use std::collections::HashMap;

use chrono::NaiveTime;
use rust_decimal::Decimal;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{
    db::{
        catalog::{
            BookingMode, ExtraAppliesTo, InventoryMode, PackagePricingMode, ProductKind,
            ProductStatus, SchedulingMode,
        },
        pricing::{Deposit, DepositType, MealPlan, PaymentMode},
        types::{CurrencyCode, Locale},
    },
    services::booking::BookingError,
};

#[derive(Debug, Clone)]
pub struct Accommodation {
    pub inventory_mode: InventoryMode,
    pub max_guests: i32,
    pub max_adults: Option<i32>,
    pub max_children: Option<i32>,
    pub standard_occupancy: i32,
    pub min_nights: i32,
    pub max_nights: Option<i32>,
    pub check_in_from: Option<NaiveTime>,
    pub check_out_until: Option<NaiveTime>,
    pub pitch_selection_extra_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub scheduling_mode: SchedulingMode,
    pub duration_minutes: Option<i32>,
    pub min_participants: i32,
    pub max_participants: Option<i32>,
    pub min_age: Option<i16>,
    pub booking_cutoff_hours: i32,
    pub guide_languages: Vec<String>,
    pub standalone_bookable: bool,
}

#[derive(Debug, Clone)]
pub struct Extra {
    pub applies_to: ExtraAppliesTo,
    pub max_quantity: Option<i32>,
    pub requires_date: bool,
    pub stock_quantity: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub duration_nights: Option<i32>,
    pub pricing_mode: PackagePricingMode,
    pub discount_percent: Decimal,
    pub items: Vec<PackagePart>,
}

#[derive(Debug, Clone)]
pub struct PackagePart {
    pub product_id: Uuid,
    pub quantity: i32,
    pub day_number: Option<i32>,
    pub is_optional: bool,
}

#[derive(Debug, Clone)]
pub struct Product {
    pub id: Uuid,
    pub kind: ProductKind,
    pub status: ProductStatus,
    pub booking_mode: BookingMode,
    pub name: String,
    pub slug: Option<String>,
    pub accommodation: Option<Accommodation>,
    pub activity: Option<Activity>,
    pub extra: Option<Extra>,
    pub package: Option<Package>,
    /// Extras added automatically to this product (`product_extras.is_mandatory`).
    pub mandatory_extras: Vec<Uuid>,
}

impl Product {
    pub fn on_request(&self) -> bool {
        self.booking_mode == BookingMode::OnRequest
            || self
                .activity
                .as_ref()
                .is_some_and(|a| a.scheduling_mode == SchedulingMode::OnRequest)
    }
}

/// Loads the products (and, recursively, their package parts and mandatory extras).
pub async fn load_products(
    conn: &mut PgConnection,
    ids: &[Uuid],
    locale: &Locale,
) -> Result<HashMap<Uuid, Product>, BookingError> {
    let mut products = HashMap::new();
    let mut pending: Vec<Uuid> = ids.to_vec();
    // Two rounds are enough (package → parts → their mandatory extras), a third is harmless.
    for _ in 0..3 {
        pending.retain(|id| !products.contains_key(id));
        pending.sort_unstable();
        pending.dedup();
        if pending.is_empty() {
            break;
        }
        let loaded = load_round(conn, &pending, locale).await?;
        pending = loaded
            .values()
            .flat_map(|p| {
                p.mandatory_extras
                    .iter()
                    .copied()
                    .chain(
                        p.package
                            .iter()
                            .flat_map(|pk| pk.items.iter().map(|i| i.product_id)),
                    )
                    .chain(
                        p.accommodation
                            .as_ref()
                            .and_then(|a| a.pitch_selection_extra_id),
                    )
            })
            .collect();
        products.extend(loaded);
    }
    Ok(products)
}

async fn load_round(
    conn: &mut PgConnection,
    ids: &[Uuid],
    locale: &Locale,
) -> Result<HashMap<Uuid, Product>, BookingError> {
    let rows = sqlx::query!(
        r#"SELECT p.id, p.kind AS "kind: ProductKind", p.status AS "status: ProductStatus",
                  p.booking_mode AS "booking_mode: BookingMode",
                  COALESCE(NULLIF(t.name, ''), d.name, p.sku::text, p.id::text) AS "name!",
                  COALESCE(t.slug, d.slug) AS "slug?",
                  a.inventory_mode AS "inventory_mode?: InventoryMode", a.max_guests AS "max_guests?",
                  a.max_adults, a.max_children, a.standard_occupancy AS "standard_occupancy?",
                  a.min_nights AS "min_nights?", a.max_nights, a.check_in_from, a.check_out_until,
                  a.pitch_selection_extra_id,
                  act.scheduling_mode AS "scheduling_mode?: SchedulingMode", act.duration_minutes,
                  act.min_participants AS "min_participants?", act.max_participants, act.min_age,
                  act.booking_cutoff_hours AS "booking_cutoff_hours?", act.guide_languages AS "guide_languages?",
                  act.standalone_bookable AS "standalone_bookable?",
                  e.applies_to AS "applies_to?: ExtraAppliesTo", e.max_quantity, e.requires_date AS "requires_date?",
                  e.stock_quantity,
                  pk.duration_nights, pk.pricing_mode AS "pricing_mode?: PackagePricingMode",
                  pk.discount_percent AS "discount_percent?",
                  COALESCE((SELECT array_agg(pe.extra_product_id ORDER BY pe.sort_order, pe.extra_product_id)
                            FROM product_extras pe WHERE pe.product_id = p.id AND pe.is_mandatory), '{}') AS "mandatory_extras!"
           FROM products p
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = p.id
                 AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN accommodation_types a ON a.product_id = p.id
           LEFT JOIN activities act ON act.product_id = p.id
           LEFT JOIN extras e ON e.product_id = p.id
           LEFT JOIN packages pk ON pk.product_id = p.id
           WHERE p.id = ANY($1) AND p.deleted_at IS NULL"#,
        ids,
        locale.as_str()
    )
    .fetch_all(&mut *conn)
    .await?;

    let package_ids: Vec<Uuid> = rows
        .iter()
        .filter(|r| r.kind == ProductKind::Package)
        .map(|r| r.id)
        .collect();
    let mut parts: HashMap<Uuid, Vec<PackagePart>> = HashMap::new();
    if !package_ids.is_empty() {
        for row in sqlx::query!(
            "SELECT package_product_id, item_product_id, quantity, day_number, is_optional
             FROM package_items WHERE package_product_id = ANY($1)
             ORDER BY package_product_id, day_number NULLS FIRST, sort_order, id",
            &package_ids
        )
        .fetch_all(&mut *conn)
        .await?
        {
            parts
                .entry(row.package_product_id)
                .or_default()
                .push(PackagePart {
                    product_id: row.item_product_id,
                    quantity: row.quantity,
                    day_number: row.day_number,
                    is_optional: row.is_optional,
                });
        }
    }

    Ok(rows
        .into_iter()
        .map(|r| {
            let accommodation = match (r.inventory_mode, r.max_guests, r.min_nights) {
                (Some(inventory_mode), Some(max_guests), Some(min_nights)) => Some(Accommodation {
                    inventory_mode,
                    max_guests,
                    max_adults: r.max_adults,
                    max_children: r.max_children,
                    standard_occupancy: r.standard_occupancy.unwrap_or(max_guests),
                    min_nights,
                    max_nights: r.max_nights,
                    check_in_from: r.check_in_from,
                    check_out_until: r.check_out_until,
                    pitch_selection_extra_id: r.pitch_selection_extra_id,
                }),
                _ => None,
            };
            let activity = r.scheduling_mode.map(|scheduling_mode| Activity {
                scheduling_mode,
                duration_minutes: r.duration_minutes,
                min_participants: r.min_participants.unwrap_or(1),
                max_participants: r.max_participants,
                min_age: r.min_age,
                booking_cutoff_hours: r.booking_cutoff_hours.unwrap_or(0),
                guide_languages: r.guide_languages.unwrap_or_default(),
                standalone_bookable: r.standalone_bookable.unwrap_or(true),
            });
            let extra = r.applies_to.map(|applies_to| Extra {
                applies_to,
                max_quantity: r.max_quantity,
                requires_date: r.requires_date.unwrap_or(false),
                stock_quantity: r.stock_quantity,
            });
            let package = r.pricing_mode.map(|pricing_mode| Package {
                duration_nights: r.duration_nights,
                pricing_mode,
                discount_percent: r.discount_percent.unwrap_or_default(),
                items: parts.remove(&r.id).unwrap_or_default(),
            });
            (
                r.id,
                Product {
                    id: r.id,
                    kind: r.kind,
                    status: r.status,
                    booking_mode: r.booking_mode,
                    name: r.name,
                    slug: r.slug,
                    accommodation,
                    activity,
                    extra,
                    package,
                    mandatory_extras: r.mandatory_extras,
                },
            )
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct GuestCategory {
    pub id: Uuid,
    pub code: String,
    pub min_age: Option<i16>,
    pub max_age: Option<i16>,
    pub counts_toward_capacity: bool,
}

pub async fn guest_categories(conn: &mut PgConnection) -> Result<Vec<GuestCategory>, BookingError> {
    Ok(sqlx::query_as!(
        GuestCategory,
        r#"SELECT id, code::text AS "code!", min_age, max_age, counts_toward_capacity
           FROM guest_categories WHERE is_active ORDER BY sort_order, code"#
    )
    .fetch_all(conn)
    .await?)
}

#[derive(Debug, Clone)]
pub struct RatePlan {
    pub id: Uuid,
    pub code: String,
    pub name: Option<String>,
    pub meal_plan: MealPlan,
    pub payment_mode: PaymentMode,
    pub deposit: Deposit,
    pub is_public: bool,
    /// `cancellation_policies.rules` (array of `{min_hours_before, refund_percent}`).
    pub cancellation_rules: Option<Value>,
}

/// The requested plan, or the default one (first public active plan). `None` when the camp
/// has no rate plan at all (pay at the camp, no cancellation policy).
pub async fn rate_plan(
    conn: &mut PgConnection,
    id: Option<Uuid>,
    locale: &Locale,
    allow_private: bool,
) -> Result<Option<RatePlan>, BookingError> {
    let row = sqlx::query!(
        r#"SELECT rp.id, rp.code::text AS "code!", rp.meal_plan AS "meal_plan: MealPlan",
                  rp.payment_mode AS "payment_mode: PaymentMode", rp.deposit_type AS "deposit_type: DepositType",
                  rp.deposit_percent, rp.deposit_amount_minor, rp.deposit_currency AS "deposit_currency: CurrencyCode",
                  rp.is_public, cp.rules AS "rules?",
                  COALESCE(t.name, d.name) AS "name?"
           FROM rate_plans rp
           LEFT JOIN cancellation_policies cp ON cp.id = rp.cancellation_policy_id
           LEFT JOIN rate_plan_translations t ON t.rate_plan_id = rp.id AND t.locale = $2
           LEFT JOIN rate_plan_translations d ON d.rate_plan_id = rp.id
                 AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE rp.is_active AND ($1::uuid IS NULL AND rp.is_public OR rp.id = $1)
           ORDER BY rp.sort_order, rp.code
           LIMIT 1"#,
        id,
        locale.as_str()
    )
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else {
        return match id {
            Some(_) => Err(BookingError::Invalid(
                "unknown or inactive rate plan".into(),
            )),
            None => Ok(None),
        };
    };
    if !row.is_public && !allow_private {
        return Err(BookingError::Invalid(
            "unknown or inactive rate plan".into(),
        ));
    }
    let deposit = Deposit::from_columns(
        row.deposit_type,
        row.deposit_percent,
        row.deposit_amount_minor,
        row.deposit_currency,
    )
    .ok_or_else(|| BookingError::Invalid("rate plan has an inconsistent deposit".into()))?;
    Ok(Some(RatePlan {
        id: row.id,
        code: row.code,
        name: row.name,
        meal_plan: row.meal_plan,
        payment_mode: row.payment_mode,
        deposit,
        is_public: row.is_public,
        cancellation_rules: row.rules,
    }))
}
