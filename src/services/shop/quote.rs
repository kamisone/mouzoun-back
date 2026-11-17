//! Loads everything `pricing::price_cart` needs from the database (one place for the cart, the
//! checkout, draft orders and order edits), plus the display facts of each variant used for the
//! cart view and the order snapshots.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    pricing::{
        CustomerFacts, Destination, LineInput, PricingSettings, PromotionInput, Quote, QuoteInput,
        RateInput, TaxRate, VatPart, ZoneInput, price_cart,
    },
    settings::ShopSettings,
};
use crate::db::{
    pricing::DiscountType,
    shop::{
        CustomerEligibility, DeliveryType, FulfillmentMethod, ShippingRateKind, ShopPaymentMethod,
    },
};

/// One requested line.
#[derive(Debug, Clone)]
pub struct WantedLine {
    pub line_id: Uuid,
    pub variant_id: Uuid,
    pub quantity: i32,
}

/// Who is buying (for first-order / staying-guest offers and per-customer limits).
#[derive(Debug, Clone, Default)]
pub struct Buyer {
    pub customer_id: Option<Uuid>,
    pub email: Option<String>,
    /// The order is delivered to a stay (verified booking).
    pub booking_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default)]
pub struct Fulfillment {
    pub method: Option<FulfillmentMethod>,
    pub destination: Destination,
    pub pickup_location_id: Option<Uuid>,
    pub chosen_rate_ids: Vec<Uuid>,
}

/// Display facts of a variant (cart view, order snapshot) in one locale.
#[derive(Debug, Clone, Serialize)]
pub struct VariantFacts {
    pub variant_id: Uuid,
    pub product_id: Uuid,
    pub sku: String,
    pub barcode: Option<String>,
    pub name: String,
    pub slug: String,
    /// "500 g · Medjool".
    pub options_label: String,
    pub options: Vec<OptionLabel>,
    pub image_key: Option<String>,
    pub weight_grams: i32,
    pub net_quantity: Option<Decimal>,
    pub net_unit: Option<String>,
    pub allergens: Vec<String>,
    pub is_perishable: bool,
    /// `none | perishable | unsealed_hygiene | personalised` (right of withdrawal).
    pub withdrawal_exemption: String,
    pub country_of_origin: Option<String>,
    pub tax_class_code: String,
    pub track_lots: bool,
    pub product_type: String,
    pub unit_price_minor: i64,
    pub compare_at_price_minor: Option<i64>,
    pub currency: String,
    pub cost_minor: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OptionLabel {
    pub option: String,
    pub value: String,
}

pub struct Loaded {
    pub input: QuoteInput,
    pub facts: HashMap<Uuid, VariantFacts>,
}

impl Loaded {
    pub fn price(&self) -> Quote {
        price_cart(&self.input)
    }
}

/// Sellable quantity per variant at `locations` (`None` = not tracked).
pub async fn availability(
    conn: &mut PgConnection,
    variants: &[Uuid],
    pickup_location: Option<Uuid>,
) -> sqlx::Result<HashMap<Uuid, Option<i64>>> {
    let rows = sqlx::query!(
        r#"SELECT v.id, v.track_inventory,
                  COALESCE((SELECT sum(l.on_hand + l.backorder_limit - l.reserved)
                            FROM inventory_levels l JOIN stock_locations s ON s.id = l.location_id
                            WHERE l.variant_id = v.id AND s.is_active
                              AND (s.legal_entity_id IS NULL OR s.legal_entity_id = shop_entity_id())
                              AND (($2::uuid IS NULL AND s.fulfills_online_orders) OR s.id = $2)), 0)::bigint AS "available!"
           FROM product_variants v WHERE v.id = ANY($1)"#,
        variants,
        pickup_location
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.id, r.track_inventory.then_some(r.available)))
        .collect())
}

/// Display facts of `variants` in `locale` (falling back to the default locale).
pub async fn variant_facts(
    conn: &mut PgConnection,
    variants: &[Uuid],
    locale: &str,
) -> sqlx::Result<HashMap<Uuid, VariantFacts>> {
    let rows = sqlx::query!(
        r#"SELECT v.id, v.product_id, v.sku::text AS "sku!", v.barcode, v.weight_grams, v.net_quantity, v.net_unit,
                  v.price_minor, v.compare_at_price_minor, v.currency::text AS "currency!", v.cost_minor,
                  sp.allergens, sp.is_perishable, sp.withdrawal_exemption, sp.track_lots, sp.product_type,
                  sp.tax_class_code::text AS "tax_class_code!", v.country_of_origin::text AS country_of_origin,
                  COALESCE(t.name, d.name, v.sku::text) AS "name!", COALESCE(t.slug, d.slug, '') AS "slug!",
                  COALESCE(vm.object_key, cm.object_key) AS image_key,
                  COALESCE((SELECT array_agg(COALESCE(ot.name, od.name, o.code::text) || '=' || COALESCE(vt.label, vd.label, ov.code::text)
                                             ORDER BY o.sort_order, o.code)
                            FROM product_variant_option_values pvo
                            JOIN product_options o ON o.id = pvo.option_id
                            JOIN product_option_values ov ON ov.id = pvo.option_value_id
                            LEFT JOIN product_option_translations ot ON ot.option_id = o.id AND ot.locale = $2
                            LEFT JOIN product_option_translations od ON od.option_id = o.id
                                 AND od.locale = (SELECT code FROM locales WHERE is_default)
                            LEFT JOIN product_option_value_translations vt ON vt.option_value_id = ov.id AND vt.locale = $2
                            LEFT JOIN product_option_value_translations vd ON vd.option_value_id = ov.id
                                 AND vd.locale = (SELECT code FROM locales WHERE is_default)
                            WHERE pvo.variant_id = v.id), '{}') AS "options!"
           FROM product_variants v
           JOIN shop_products sp ON sp.product_id = v.product_id
           JOIN products p ON p.id = v.product_id
           LEFT JOIN product_translations t ON t.product_id = p.id AND t.locale = $2
           LEFT JOIN product_translations d ON d.product_id = p.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           LEFT JOIN media_assets vm ON vm.id = v.cover_media_id AND vm.deleted_at IS NULL
           LEFT JOIN media_assets cm ON cm.id = p.cover_media_id AND cm.deleted_at IS NULL
           WHERE v.id = ANY($1)"#,
        variants,
        locale
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let options: Vec<OptionLabel> = r
                .options
                .iter()
                .filter_map(|o| o.split_once('='))
                .map(|(a, b)| OptionLabel {
                    option: a.to_string(),
                    value: b.to_string(),
                })
                .collect();
            let options_label = options
                .iter()
                .map(|o| o.value.as_str())
                .collect::<Vec<_>>()
                .join(" · ");
            (
                r.id,
                VariantFacts {
                    variant_id: r.id,
                    product_id: r.product_id,
                    sku: r.sku,
                    barcode: r.barcode,
                    name: r.name,
                    slug: r.slug,
                    options_label,
                    options,
                    image_key: r.image_key,
                    weight_grams: r.weight_grams,
                    net_quantity: r.net_quantity,
                    net_unit: r.net_unit,
                    allergens: r.allergens,
                    is_perishable: r.is_perishable,
                    withdrawal_exemption: r.withdrawal_exemption,
                    country_of_origin: r.country_of_origin,
                    tax_class_code: r.tax_class_code,
                    track_lots: r.track_lots,
                    product_type: r.product_type,
                    unit_price_minor: r.price_minor,
                    compare_at_price_minor: r.compare_at_price_minor,
                    currency: r.currency,
                    cost_minor: r.cost_minor,
                },
            )
        })
        .collect())
}

/// VAT rates by tax class in `country` on `day` (active rates only).
pub async fn rates_for(
    conn: &mut PgConnection,
    country: &str,
    day: chrono::NaiveDate,
) -> sqlx::Result<HashMap<String, TaxRate>> {
    Ok(sqlx::query!(
        r#"SELECT id, tax_class_code::text AS "class!", rate_percent FROM tax_rates
           WHERE country_code = $1 AND is_active AND valid_period @> $2::date"#,
        country,
        day
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(|r| {
        (
            r.class.clone(),
            TaxRate {
                tax_id: r.id,
                code: format!("{}_{}", r.class, country.to_ascii_lowercase()),
                rate_percent: r.rate_percent,
            },
        )
    })
    .collect())
}

/// Where VAT is due for a sale from `home` (the seller's country) to `destination`:
/// same country → home; EU destination → its own rates with OSS, else home rates (under the EU
/// distance-sales threshold); outside the EU → exempt export (`zero` class).
pub fn vat_country(
    settings: &ShopSettings,
    home: &str,
    destination: Option<&str>,
) -> (String, bool) {
    match destination {
        None => (home.to_string(), false),
        Some(d) if d.eq_ignore_ascii_case(home) => (home.to_string(), false),
        Some(d) if settings.is_eu(d) => {
            if settings.tax.oss_enabled {
                (d.to_ascii_uppercase(), false)
            } else {
                (home.to_string(), false)
            }
        }
        Some(_) => (home.to_string(), true),
    }
}

/// Splits of a product: one class, or a bundle's `[{tax_class_code, share_percent}]`.
fn class_shares(class: &str, bundle: Option<&serde_json::Value>) -> Vec<(String, u32)> {
    let parts: Vec<(String, u32)> = bundle
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let c = p["tax_class_code"].as_str()?.to_string();
                    let pct = p["share_percent"].as_f64()?;
                    Some((c, (pct * 100.0).round() as u32))
                })
                .collect()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        vec![(class.to_string(), 10_000)]
    } else {
        parts
    }
}

pub async fn customer_facts(
    conn: &mut PgConnection,
    buyer: &Buyer,
    now: DateTime<Utc>,
) -> sqlx::Result<CustomerFacts> {
    let email = buyer
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());
    let row = sqlx::query!(
        r#"SELECT
              NOT EXISTS (SELECT 1 FROM orders o
                          WHERE (o.customer_id = $1 OR o.email = $2::text::citext)
                            AND o.status IN ('open', 'completed')) AS "first_order!",
              ($3::uuid IS NOT NULL OR EXISTS (
                  SELECT 1 FROM bookings b JOIN customers c ON c.id = b.customer_id
                  WHERE (c.id = $1 OR c.email = $2::text::citext)
                    AND b.status IN ('confirmed', 'checked_in') AND b.departure_date >= $4)) AS "staying!""#,
        buyer.customer_id,
        email,
        buyer.booking_id,
        crate::camp_time::today(now)
    )
    .fetch_one(&mut *conn)
    .await?;
    let redemptions = sqlx::query!(
        r#"SELECT r.promotion_id, count(*) AS "n!"
           FROM promotion_redemptions r
           LEFT JOIN orders o ON o.id = r.order_id
           LEFT JOIN bookings b ON b.id = r.booking_id
           LEFT JOIN customers c ON c.id = r.customer_id
           WHERE (r.customer_id = $1 OR c.email = $2::text::citext OR o.email = $2::text::citext)
             AND COALESCE(o.status, b.status) NOT IN ('cancelled', 'expired')
           GROUP BY r.promotion_id"#,
        buyer.customer_id,
        email
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| (r.promotion_id, r.n))
    .collect();
    Ok(CustomerFacts {
        first_order: row.first_order,
        staying_guest: row.staying,
        redemptions,
    })
}

pub async fn promotions(
    conn: &mut PgConnection,
    codes: &[String],
) -> sqlx::Result<Vec<PromotionInput>> {
    let codes: Vec<String> = codes.iter().map(|c| c.trim().to_string()).collect();
    let rows = sqlx::query!(
        r#"SELECT p.id, p.code::text AS code, p.discount_type AS "discount_type: DiscountType",
                  p.discount_percent, p.discount_amount_minor, p.min_subtotal_minor, p.min_quantity,
                  lower(p.booking_window) AS window_start, upper(p.booking_window) AS window_end,
                  p.booking_window IS NOT NULL AS "has_window!",
                  p.applies_to_all, p.applies_to_shipping, p.is_stackable,
                  p.customer_eligibility AS "eligibility: CustomerEligibility", p.max_per_customer,
                  (p.max_redemptions IS NOT NULL AND (
                      SELECT count(*) FROM promotion_redemptions r
                      LEFT JOIN orders o ON o.id = r.order_id
                      LEFT JOIN bookings b ON b.id = r.booking_id
                      WHERE r.promotion_id = p.id AND COALESCE(o.status, b.status) NOT IN ('cancelled', 'expired')
                  ) >= p.max_redemptions) AS "exhausted!",
                  COALESCE((SELECT array_agg(product_id) FROM promotion_products WHERE promotion_id = p.id), '{}') AS "product_ids!",
                  COALESCE((SELECT array_agg(category_id) FROM promotion_categories WHERE promotion_id = p.id), '{}') AS "category_ids!"
           FROM promotions p
           WHERE p.is_active AND p.scope IN ('shop', 'all')
             AND (p.code IS NULL OR p.code = ANY($1::text[]::citext[]))
           ORDER BY p.id"#,
        &codes
    )
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|p| PromotionInput {
            id: p.id,
            code: p.code,
            discount_type: p.discount_type,
            percent: p.discount_percent,
            amount_minor: p.discount_amount_minor,
            min_subtotal_minor: p.min_subtotal_minor,
            min_quantity: p.min_quantity,
            window: p.has_window.then_some((p.window_start, p.window_end)),
            applies_to_all: p.applies_to_all,
            product_ids: p.product_ids,
            category_ids: p.category_ids,
            applies_to_shipping: p.applies_to_shipping,
            is_stackable: p.is_stackable,
            eligibility: p.eligibility,
            exhausted: p.exhausted,
            max_per_customer: p.max_per_customer,
        })
        .collect())
}

pub async fn shipping(
    conn: &mut PgConnection,
    locale: &str,
) -> sqlx::Result<(Vec<ZoneInput>, Vec<RateInput>)> {
    let zones = sqlx::query!(
        r#"SELECT id, country_codes::text[] AS "country_codes!", region_codes, postal_code_patterns, is_export, sort_order
           FROM shipping_zones WHERE is_active ORDER BY sort_order, id"#
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|z| ZoneInput {
        id: z.id,
        country_codes: z.country_codes.into_iter().map(|c| c.trim().to_string()).collect(),
        region_codes: z.region_codes,
        postal_code_patterns: z.postal_code_patterns,
        is_export: z.is_export,
        sort_order: z.sort_order,
    })
    .collect();
    let rates = sqlx::query!(
        r#"SELECT r.id, r.zone_id, r.profile_id, r.code::text AS "code!", r.kind AS "kind: ShippingRateKind", r.carrier,
                  r.amount_minor, r.per_kg_minor, r.min_weight_grams, r.max_weight_grams, r.min_subtotal_minor,
                  r.max_subtotal_minor, r.free_over_minor, r.delivery_days_min, r.delivery_days_max,
                  r.allows_cash_on_delivery, r.cod_fee_minor, r.sort_order, r.delivery_type AS "delivery_type: DeliveryType",
                  COALESCE(t.name, d.name, r.code::text) AS "name!"
           FROM shipping_rates r
           LEFT JOIN shipping_rate_translations t ON t.rate_id = r.id AND t.locale = $1
           LEFT JOIN shipping_rate_translations d ON d.rate_id = r.id AND d.locale = (SELECT code FROM locales WHERE is_default)
           WHERE r.is_active ORDER BY r.sort_order, r.id"#,
        locale
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| RateInput {
        id: r.id,
        zone_id: r.zone_id,
        profile_id: r.profile_id,
        code: r.code,
        name: r.name,
        kind: r.kind,
        delivery_type: r.delivery_type,
        carrier: r.carrier,
        amount_minor: r.amount_minor,
        per_kg_minor: r.per_kg_minor,
        min_weight_grams: r.min_weight_grams,
        max_weight_grams: r.max_weight_grams,
        min_subtotal_minor: r.min_subtotal_minor,
        max_subtotal_minor: r.max_subtotal_minor,
        free_over_minor: r.free_over_minor,
        delivery_days_min: r.delivery_days_min,
        delivery_days_max: r.delivery_days_max,
        allows_cash_on_delivery: r.allows_cash_on_delivery,
        cod_fee_minor: r.cod_fee_minor,
        sort_order: r.sort_order,
    })
    .collect();
    Ok((zones, rates))
}

#[allow(clippy::too_many_arguments)]
pub async fn load(
    conn: &mut PgConnection,
    settings: &ShopSettings,
    lines: &[WantedLine],
    fulfillment: &Fulfillment,
    codes: &[String],
    buyer: &Buyer,
    payment_method: Option<ShopPaymentMethod>,
    locale: &str,
    now: DateTime<Utc>,
) -> sqlx::Result<Loaded> {
    let variant_ids: Vec<Uuid> = lines.iter().map(|l| l.variant_id).collect();
    let rows = sqlx::query!(
        r#"SELECT v.id, v.product_id, v.price_minor, v.compare_at_price_minor, v.is_taxable, v.weight_grams,
                  v.requires_shipping, sp.shipping_profile_id, sp.max_per_order,
                  (p.status = 'published' AND p.deleted_at IS NULL AND v.status = 'active' AND v.deleted_at IS NULL
                   AND (sp.available_period IS NULL OR sp.available_period @> $2::timestamptz)) AS "purchasable!",
                  COALESCE((SELECT array_agg(category_id) FROM product_categories pc WHERE pc.product_id = p.id), '{}') AS "category_ids!",
                  sp.tax_class_code::text AS "tax_class!", sp.bundle_vat_split
           FROM product_variants v
           JOIN shop_products sp ON sp.product_id = v.product_id
           JOIN products p ON p.id = v.product_id
           WHERE v.id = ANY($1)"#,
        &variant_ids,
        now
    )
    .fetch_all(&mut *conn)
    .await?;
    let by_id: HashMap<Uuid, _> = rows.into_iter().map(|r| (r.id, r)).collect();

    // VAT: rates of the destination (or home) country on the sale date.
    let home = sqlx::query_scalar!(
        r#"SELECT country_code::text AS "c!" FROM legal_entities WHERE code = $1::text::citext"#,
        settings.legal_entity
    )
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or_else(|| "FR".into());
    let today = crate::camp_time::today(now);
    let (tax_country, export) = vat_country(
        settings,
        &home,
        fulfillment.destination.country_code.as_deref(),
    );
    let rates_dest = rates_for(conn, &tax_country, today).await?;
    let rates_home = if tax_country == home {
        rates_dest.clone()
    } else {
        rates_for(conn, &home, today).await?
    };
    let parts_of = |class: &str,
                    bundle: Option<&serde_json::Value>,
                    rates: &HashMap<String, TaxRate>|
     -> Vec<VatPart> {
        let shares = class_shares(class, bundle);
        let mut out = Vec::new();
        for (c, share) in shares {
            let key = if export { "zero" } else { c.as_str() };
            match rates.get(key) {
                Some(t) => out.push(VatPart {
                    tax: t.clone(),
                    share_bp: share,
                }),
                None => return vec![],
            }
        }
        out
    };
    // Effective rate of a split (weighted), for the gross-price policy.
    let effective = |parts: &[VatPart]| -> Decimal {
        parts
            .iter()
            .map(|p| p.tax.rate_percent * Decimal::from(p.share_bp))
            .sum::<Decimal>()
            / Decimal::from(10_000)
    };
    let pickup = (fulfillment.method == Some(FulfillmentMethod::Pickup))
        .then_some(fulfillment.pickup_location_id)
        .flatten();
    let avail = availability(conn, &variant_ids, pickup).await?;

    let line_inputs = lines
        .iter()
        .map(|l| match by_id.get(&l.variant_id) {
            Some(r) => LineInput {
                line_id: l.line_id,
                variant_id: l.variant_id,
                product_id: r.product_id,
                category_ids: r.category_ids.clone(),
                quantity: l.quantity,
                unit_price_minor: {
                    // Prices are set incl. the home VAT. With `keep_gross_price_constant` off,
                    // the net price stays constant and the gross follows the destination rate.
                    let dest = parts_of(&r.tax_class, r.bundle_vat_split.as_ref(), &rates_dest);
                    let base = parts_of(&r.tax_class, r.bundle_vat_split.as_ref(), &rates_home);
                    if settings.tax.keep_gross_price_constant
                        || tax_country == home
                        || dest.is_empty()
                        || base.is_empty()
                    {
                        r.price_minor
                    } else {
                        let hundred = Decimal::from(100);
                        let gross = Decimal::from(r.price_minor) * (hundred + effective(&dest))
                            / (hundred + effective(&base));
                        gross.round().to_string().parse().unwrap_or(r.price_minor)
                    }
                },
                compare_at_price_minor: r.compare_at_price_minor,
                vat: parts_of(&r.tax_class, r.bundle_vat_split.as_ref(), &rates_dest),
                is_taxable: r.is_taxable,
                weight_grams: r.weight_grams,
                requires_shipping: r.requires_shipping,
                shipping_profile_id: r.shipping_profile_id,
                available: avail.get(&l.variant_id).copied().flatten(),
                max_per_order: r.max_per_order,
                purchasable: r.purchasable,
            },
            None => LineInput {
                line_id: l.line_id,
                variant_id: l.variant_id,
                product_id: Uuid::nil(),
                category_ids: vec![],
                quantity: l.quantity,
                unit_price_minor: 0,
                compare_at_price_minor: None,
                vat: vec![],
                is_taxable: false,
                weight_grams: 1,
                requires_shipping: false,
                shipping_profile_id: Uuid::nil(),
                available: Some(0),
                max_per_order: None,
                purchasable: false,
            },
        })
        .collect();

    let (zones, rates) = shipping(conn, locale).await?;
    let facts = variant_facts(conn, &variant_ids, locale).await?;
    let promos = promotions(conn, codes).await?;
    let customer = customer_facts(conn, buyer, now).await?;
    let input = QuoteInput {
        lines: line_inputs,
        method: fulfillment.method,
        destination: fulfillment.destination.clone(),
        zones,
        rates,
        chosen_rate_ids: fulfillment.chosen_rate_ids.clone(),
        promotions: promos,
        codes: codes.to_vec(),
        customer,
        payment_method,
        settings: PricingSettings {
            prices_include_tax: settings.prices_include_tax,
            min_order_minor: settings.min_order_minor,
            max_quantity_per_line: i32::try_from(settings.max_quantity_per_line).unwrap_or(20),
            stay_delivery_enabled: settings.stay_delivery_enabled,
            stay_delivery_fee_minor: settings.stay_delivery_fee_minor,
        },
        now,
    };
    Ok(Loaded { input, facts })
}
