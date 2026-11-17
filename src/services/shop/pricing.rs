//! The shop pricing engine: one pure function, `price_cart`, used for the cart, the checkout, draft
//! orders and order edits. The browser only sends variant ids and quantities; every price,
//! discount, shipping amount and tax is computed here from data loaded by the caller.
//!
//! Money is integer cents. Rules:
//! - per line: `subtotal = unit × quantity`; order-level discounts are allocated across the
//!   eligible lines proportionally to their subtotals (floored), and the remainder cents go to
//!   the largest lines, so allocations always sum to the discount exactly;
//! - tax is computed once per line after discounts, rounded half up:
//!   inclusive `tax = gross − round(gross / (1 + rate))`, exclusive `tax = round(net × rate)`;
//!   several taxes on one line share the combined amount proportionally to their rates;
//! - shipping: lines that ship are grouped by shipping profile; each group gets the chosen rate
//!   of its profile (else the cheapest) in the zone of the destination; a group without a rate
//!   makes its lines `not_shippable_to_country`;
//! - promotions (`scope` shop or all): the best non-stackable one, then every stackable one.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{
    pricing::DiscountType,
    shop::{
        CustomerEligibility, DeliveryType, FulfillmentMethod, ShippingRateKind, ShopPaymentMethod,
    },
};

// ---- Input -----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxRate {
    pub tax_id: Uuid,
    pub code: String,
    /// Percent with up to 3 decimals (20.000 = 20 %).
    pub rate_percent: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VatPart {
    pub tax: TaxRate,
    pub share_bp: u32,
}

impl VatPart {
    pub fn whole(tax: TaxRate) -> Self {
        Self {
            tax,
            share_bp: 10_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LineInput {
    /// Cart item or order item id: issues and results refer to it.
    pub line_id: Uuid,
    pub variant_id: Uuid,
    pub product_id: Uuid,
    pub category_ids: Vec<Uuid>,
    pub quantity: i32,
    pub unit_price_minor: i64,
    pub compare_at_price_minor: Option<i64>,
    /// VAT of the line: one part, or several for mixed-rate bundles (gift boxes), each with its
    /// share of the price in basis points (sum 10 000). Resolved by the loader from the tax
    /// class and the destination country. Empty on a taxable line = no rate found (blocking).
    pub vat: Vec<VatPart>,
    pub is_taxable: bool,
    pub weight_grams: i32,
    pub requires_shipping: bool,
    pub shipping_profile_id: Uuid,
    /// Sellable quantity (`None` = not tracked, unlimited).
    pub available: Option<i64>,
    pub max_per_order: Option<i32>,
    /// Published, active variant, in season.
    pub purchasable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Destination {
    pub country_code: Option<String>,
    pub region: Option<String>,
    pub postal_code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ZoneInput {
    pub id: Uuid,
    pub country_codes: Vec<String>,
    pub region_codes: Option<Vec<String>>,
    pub postal_code_patterns: Option<Vec<String>>,
    pub is_export: bool,
    pub sort_order: i32,
}

#[derive(Debug, Clone)]
pub struct RateInput {
    pub id: Uuid,
    pub zone_id: Uuid,
    pub profile_id: Uuid,
    pub code: String,
    pub name: String,
    pub kind: ShippingRateKind,
    /// Home delivery or relay point (Mondial Relay, Colissimo pick-up points).
    pub delivery_type: DeliveryType,
    pub carrier: Option<String>,
    pub amount_minor: Option<i64>,
    pub per_kg_minor: Option<i64>,
    pub min_weight_grams: Option<i32>,
    pub max_weight_grams: Option<i32>,
    pub min_subtotal_minor: Option<i64>,
    pub max_subtotal_minor: Option<i64>,
    pub free_over_minor: Option<i64>,
    pub delivery_days_min: Option<i32>,
    pub delivery_days_max: Option<i32>,
    pub allows_cash_on_delivery: bool,
    pub cod_fee_minor: Option<i64>,
    pub sort_order: i32,
}

/// Validity window `[from, to)` (either end open).
pub type Window = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

#[derive(Debug, Clone)]
pub struct PromotionInput {
    pub id: Uuid,
    /// `None` = automatic offer.
    pub code: Option<String>,
    pub discount_type: DiscountType,
    pub percent: Option<Decimal>,
    pub amount_minor: Option<i64>,
    pub min_subtotal_minor: Option<i64>,
    pub min_quantity: Option<i32>,
    pub window: Option<Window>,
    pub applies_to_all: bool,
    pub product_ids: Vec<Uuid>,
    pub category_ids: Vec<Uuid>,
    pub applies_to_shipping: bool,
    pub is_stackable: bool,
    pub eligibility: CustomerEligibility,
    /// `max_redemptions` reached.
    pub exhausted: bool,
    pub max_per_customer: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct CustomerFacts {
    /// No earlier order that was paid or confirmed.
    pub first_order: bool,
    /// Has a confirmed booking covering today or later (or the order is for a stay delivery).
    pub staying_guest: bool,
    /// Earlier redemptions of each promotion by this customer.
    pub redemptions: HashMap<Uuid, i64>,
}

#[derive(Debug, Clone)]
pub struct PricingSettings {
    pub prices_include_tax: bool,
    pub min_order_minor: i64,
    pub max_quantity_per_line: i32,
    pub stay_delivery_enabled: bool,
    pub stay_delivery_fee_minor: i64,
}

impl Default for PricingSettings {
    fn default() -> Self {
        Self {
            prices_include_tax: true,
            min_order_minor: 0,
            max_quantity_per_line: 20,
            stay_delivery_enabled: true,
            stay_delivery_fee_minor: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuoteInput {
    pub lines: Vec<LineInput>,
    pub method: Option<FulfillmentMethod>,
    pub destination: Destination,
    pub zones: Vec<ZoneInput>,
    pub rates: Vec<RateInput>,
    /// The customer's chosen rate per profile (others get the cheapest).
    pub chosen_rate_ids: Vec<Uuid>,
    pub promotions: Vec<PromotionInput>,
    /// Codes entered by the customer (case-insensitive).
    pub codes: Vec<String>,
    pub customer: CustomerFacts,
    pub payment_method: Option<ShopPaymentMethod>,
    pub settings: PricingSettings,
    pub now: DateTime<Utc>,
}

// ---- Output ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueCode {
    OutOfStock,
    QuantityLimited,
    NotShippableToCountry,
    PromotionNotApplicable,
    BelowMinOrder,
    ProductUnavailable,
    /// Shipping chosen but the address (country) is missing.
    AddressRequired,
    /// No fulfilment method chosen yet.
    FulfillmentRequired,
    /// The payment method cannot be used for this order (e.g. COD on a rate without COD).
    PaymentMethodUnavailable,
    EmptyCart,
    /// No VAT rate for a product's tax class in the destination country (or a bundle split
    /// that does not add up): the sale cannot be priced.
    TaxUnavailable,
}

impl IssueCode {
    /// Blocks the checkout (the others are informative).
    pub fn is_blocking(self) -> bool {
        !matches!(self, Self::PromotionNotApplicable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub code: IssueCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_id: Option<Uuid>,
    #[serde(skip_serializing_if = "serde_json::Value::is_null", default)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxLine {
    pub tax_id: Uuid,
    pub code: String,
    pub rate: Decimal,
    pub amount_minor: i64,
    pub inclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscountAllocation {
    pub promotion_id: Uuid,
    pub code: Option<String>,
    pub amount_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteLine {
    pub line_id: Uuid,
    pub variant_id: Uuid,
    pub product_id: Uuid,
    pub quantity: i32,
    pub unit_price_minor: i64,
    pub compare_at_price_minor: Option<i64>,
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub tax_minor: i64,
    pub total_minor: i64,
    pub tax_lines: Vec<TaxLine>,
    pub discount_allocations: Vec<DiscountAllocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShippingOption {
    pub rate_id: Uuid,
    pub code: String,
    pub name: String,
    pub carrier: Option<String>,
    pub amount_minor: i64,
    pub delivery_days_min: Option<i32>,
    pub delivery_days_max: Option<i32>,
    pub allows_cash_on_delivery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShippingLine {
    pub profile_id: Uuid,
    pub rate_id: Option<Uuid>,
    pub code: String,
    pub name: String,
    pub carrier: Option<String>,
    pub amount_minor: i64,
    pub discount_minor: i64,
    pub tax_minor: i64,
    pub tax_lines: Vec<TaxLine>,
    pub allows_cash_on_delivery: bool,
    pub cod_fee_minor: i64,
    /// Every rate this group could use (the customer may switch).
    pub options: Vec<ShippingOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedPromotion {
    pub promotion_id: Uuid,
    pub code: Option<String>,
    pub amount_minor: i64,
    pub shipping_amount_minor: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Totals {
    pub subtotal_minor: i64,
    pub discount_minor: i64,
    pub shipping_minor: i64,
    pub shipping_discount_minor: i64,
    pub tax_minor: i64,
    pub cod_fee_minor: i64,
    pub total_minor: i64,
    pub taxes_included: bool,
    /// Per VAT rate: base (excl. VAT), VAT and amount incl. VAT, goods, shipping and fees
    /// together. The gross amounts sum to the total exactly (French invoices need this).
    pub vat_breakdown: Vec<VatBucket>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VatBucket {
    pub rate: Decimal,
    pub base_minor: i64,
    pub vat_minor: i64,
    pub gross_minor: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    pub lines: Vec<QuoteLine>,
    pub shipping_lines: Vec<ShippingLine>,
    pub promotions: Vec<AppliedPromotion>,
    pub totals: Totals,
    pub issues: Vec<Issue>,
}

impl Quote {
    pub fn blocking_issues(&self) -> Vec<&Issue> {
        self.issues
            .iter()
            .filter(|i| i.code.is_blocking())
            .collect()
    }
}

// ---- Arithmetic --------------------------------------------------------------------------------

/// `round(a / b)` half up, for `a >= 0`, `b > 0`.
fn div_round(a: i128, b: i128) -> i128 {
    (2 * a + b) / (2 * b)
}

/// A tax rate in thousandths of a percent (20 % = 20_000).
fn milli_percent(rate: Decimal) -> i128 {
    (rate * Decimal::from(1000))
        .round()
        .to_i128()
        .unwrap_or(0)
        .max(0)
}

/// Splits `amount` across `weights` proportionally (floored), the remainder cents going to the
/// largest weights first. `caps` bounds each share (a line cannot get more discount than its
/// remaining amount); the sum of `caps` must be ≥ `amount`.
pub fn allocate(amount: i64, weights: &[i64], caps: &[i64]) -> Vec<i64> {
    let n = weights.len();
    let mut out = vec![0i64; n];
    if amount <= 0 || n == 0 {
        return out;
    }
    let total: i128 = weights.iter().map(|w| i128::from((*w).max(0))).sum();
    if total == 0 {
        // No weight: everything on the first lines that can take it.
        let mut left = amount;
        for i in 0..n {
            let take = left.min(caps[i]);
            out[i] = take;
            left -= take;
        }
        return out;
    }
    for i in 0..n {
        let share = (i128::from(amount) * i128::from(weights[i].max(0)) / total) as i64;
        out[i] = share.min(caps[i]);
    }
    let mut left = amount - out.iter().sum::<i64>();
    // Largest weight first (stable on ties: earlier line first).
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|a, b| weights[*b].cmp(&weights[*a]).then(a.cmp(b)));
    while left > 0 {
        let mut moved = false;
        for &i in &order {
            if left == 0 {
                break;
            }
            if out[i] < caps[i] {
                out[i] += 1;
                left -= 1;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    out
}

/// Tax of `amount` for `taxes` (combined), split per tax; `inclusive`: `amount` contains the tax.
fn tax_lines(amount: i64, taxes: &[TaxRate], inclusive: bool) -> Vec<TaxLine> {
    let rates: Vec<i128> = taxes
        .iter()
        .map(|t| milli_percent(t.rate_percent))
        .collect();
    let combined: i128 = rates.iter().sum();
    if amount <= 0 || combined == 0 {
        return taxes
            .iter()
            .map(|t| TaxLine {
                tax_id: t.tax_id,
                code: t.code.clone(),
                rate: t.rate_percent,
                amount_minor: 0,
                inclusive,
            })
            .collect();
    }
    let a = i128::from(amount);
    let total = if inclusive {
        a - div_round(a * 100_000, 100_000 + combined)
    } else {
        div_round(a * combined, 100_000)
    } as i64;
    let weights: Vec<i64> = rates.iter().map(|r| *r as i64).collect();
    let caps = vec![total; taxes.len()];
    let shares = allocate(total, &weights, &caps);
    taxes
        .iter()
        .zip(shares)
        .map(|(t, amount_minor)| TaxLine {
            tax_id: t.tax_id,
            code: t.code.clone(),
            rate: t.rate_percent,
            amount_minor,
            inclusive,
        })
        .collect()
}

fn percent_of(amount: i64, percent: Decimal) -> i64 {
    let bp = (percent * Decimal::from(100))
        .round()
        .to_i128()
        .unwrap_or(0)
        .clamp(0, 10_000);
    div_round(i128::from(amount.max(0)) * bp, 10_000) as i64
}

// ---- Shipping -------------------------------------------------------------------------------

fn pattern_matches(pattern: &str, postal: &str) -> bool {
    // `*` suffix = prefix match ("40*"), otherwise exact (case and spaces ignored).
    let norm = |s: &str| s.replace(' ', "").to_ascii_uppercase();
    let (p, v) = (norm(pattern), norm(postal));
    match p.strip_suffix('*') {
        Some(prefix) => v.starts_with(prefix),
        None => v == p,
    }
}

/// The zone of a destination: the first active zone (by sort order) listing the country and
/// matching the region / postal code restrictions when it has them.
pub fn zone_for<'a>(zones: &'a [ZoneInput], dest: &Destination) -> Option<&'a ZoneInput> {
    let country = dest.country_code.as_deref()?.to_ascii_uppercase();
    let mut candidates: Vec<&ZoneInput> = zones
        .iter()
        .filter(|z| {
            z.country_codes
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&country))
        })
        .filter(|z| match (&z.region_codes, &dest.region) {
            (Some(regions), Some(r)) if !regions.is_empty() => {
                regions.iter().any(|x| x.eq_ignore_ascii_case(r))
            }
            (Some(regions), None) if !regions.is_empty() => false,
            _ => true,
        })
        .filter(|z| match (&z.postal_code_patterns, &dest.postal_code) {
            (Some(p), Some(code)) if !p.is_empty() => p.iter().any(|x| pattern_matches(x, code)),
            (Some(p), None) if !p.is_empty() => false,
            _ => true,
        })
        .collect();
    // The most specific zone wins, then sort order.
    candidates.sort_by_key(|z| {
        let specific = z.region_codes.as_ref().is_some_and(|r| !r.is_empty())
            || z.postal_code_patterns
                .as_ref()
                .is_some_and(|p| !p.is_empty());
        (!specific, z.sort_order)
    });
    candidates.first().copied()
}

/// Price of `rate` for a group weighing `weight_grams` with `subtotal_minor` of goods, `None`
/// when the rate does not apply.
pub fn rate_amount(rate: &RateInput, weight_grams: i64, subtotal_minor: i64) -> Option<i64> {
    if rate
        .min_weight_grams
        .is_some_and(|m| weight_grams < i64::from(m))
        || rate
            .max_weight_grams
            .is_some_and(|m| weight_grams > i64::from(m))
        || rate.min_subtotal_minor.is_some_and(|m| subtotal_minor < m)
        || rate.max_subtotal_minor.is_some_and(|m| subtotal_minor > m)
    {
        return None;
    }
    if rate.free_over_minor.is_some_and(|f| subtotal_minor >= f) {
        return Some(0);
    }
    Some(match rate.kind {
        ShippingRateKind::Free => 0,
        ShippingRateKind::Flat | ShippingRateKind::PriceBased => rate.amount_minor.unwrap_or(0),
        ShippingRateKind::WeightBased => {
            // Started kilograms are charged in full.
            let kg = (weight_grams + 999) / 1000;
            rate.amount_minor.unwrap_or(0) + rate.per_kg_minor.unwrap_or(0) * kg.max(1)
        }
    })
}

// ---- Engine ----------------------------------------------------------------------------------

struct Candidate<'a> {
    promo: &'a PromotionInput,
    /// Indexes of the lines it applies to.
    lines: Vec<usize>,
    goods_amount: i64,
    shipping_share: bool,
}

pub fn price_cart(input: &QuoteInput) -> Quote {
    let mut issues = Vec::new();
    let s = &input.settings;

    if input.lines.is_empty() {
        issues.push(Issue {
            code: IssueCode::EmptyCart,
            line_id: None,
            data: serde_json::Value::Null,
        });
    }

    // -- Lines: availability and limits.
    for l in &input.lines {
        if !l.purchasable {
            issues.push(issue(IssueCode::ProductUnavailable, Some(l.line_id), None));
            continue;
        }
        if let Some(available) = l.available {
            if available <= 0 {
                issues.push(issue(IssueCode::OutOfStock, Some(l.line_id), None));
                continue;
            }
            if i64::from(l.quantity) > available {
                issues.push(issue(
                    IssueCode::QuantityLimited,
                    Some(l.line_id),
                    Some(serde_json::json!({ "max": available })),
                ));
                continue;
            }
        }
        let max = l
            .max_per_order
            .map_or(s.max_quantity_per_line, |m| m.min(s.max_quantity_per_line));
        if l.quantity > max {
            issues.push(issue(
                IssueCode::QuantityLimited,
                Some(l.line_id),
                Some(serde_json::json!({ "max": max })),
            ));
        }
    }

    let subtotals: Vec<i64> = input
        .lines
        .iter()
        .map(|l| l.unit_price_minor * i64::from(l.quantity))
        .collect();
    let subtotal: i64 = subtotals.iter().sum();

    // -- Shipping (before promotions: free shipping needs the amount).
    let mut shipping_lines: Vec<ShippingLine> = Vec::new();
    match input.method {
        None => {
            if !input.lines.is_empty() {
                issues.push(issue(IssueCode::FulfillmentRequired, None, None));
            }
        }
        Some(FulfillmentMethod::Pickup) => {}
        Some(FulfillmentMethod::StayDelivery) => {
            if !s.stay_delivery_enabled {
                issues.push(issue(IssueCode::FulfillmentRequired, None, None));
            } else if s.stay_delivery_fee_minor > 0 {
                shipping_lines.push(ShippingLine {
                    profile_id: Uuid::nil(),
                    rate_id: None,
                    code: "stay_delivery".into(),
                    name: "stay_delivery".into(),
                    carrier: None,
                    amount_minor: s.stay_delivery_fee_minor,
                    discount_minor: 0,
                    tax_minor: 0,
                    tax_lines: vec![],
                    allows_cash_on_delivery: false,
                    cod_fee_minor: 0,
                    options: vec![],
                });
            }
        }
        Some(method @ (FulfillmentMethod::Shipping | FulfillmentMethod::RelayPoint)) => {
            let wanted = if method == FulfillmentMethod::RelayPoint {
                DeliveryType::RelayPoint
            } else {
                DeliveryType::Home
            };
            let shipped: Vec<usize> = (0..input.lines.len())
                .filter(|i| input.lines[*i].requires_shipping)
                .collect();
            if input.destination.country_code.is_none() {
                if !shipped.is_empty() {
                    issues.push(issue(IssueCode::AddressRequired, None, None));
                }
            } else {
                let zone = zone_for(&input.zones, &input.destination);
                // Group by profile (BTreeMap: stable order).
                let mut groups: BTreeMap<Uuid, Vec<usize>> = BTreeMap::new();
                for i in shipped {
                    groups
                        .entry(input.lines[i].shipping_profile_id)
                        .or_default()
                        .push(i);
                }
                for (profile_id, idx) in groups {
                    let weight: i64 = idx
                        .iter()
                        .map(|i| {
                            i64::from(input.lines[*i].weight_grams)
                                * i64::from(input.lines[*i].quantity)
                        })
                        .sum();
                    let group_subtotal: i64 = idx.iter().map(|i| subtotals[*i]).sum();
                    let mut options: Vec<(&RateInput, i64)> = match zone {
                        Some(z) => input
                            .rates
                            .iter()
                            .filter(|r| {
                                r.zone_id == z.id
                                    && r.profile_id == profile_id
                                    && r.delivery_type == wanted
                            })
                            .filter_map(|r| rate_amount(r, weight, group_subtotal).map(|a| (r, a)))
                            .collect(),
                        None => vec![],
                    };
                    options.sort_by_key(|(r, a)| (*a, r.sort_order));
                    if options.is_empty() {
                        for i in &idx {
                            issues.push(issue(
                                IssueCode::NotShippableToCountry,
                                Some(input.lines[*i].line_id),
                                Some(serde_json::json!({ "country": input.destination.country_code })),
                            ));
                        }
                        continue;
                    }
                    let chosen = options
                        .iter()
                        .find(|(r, _)| input.chosen_rate_ids.contains(&r.id))
                        .unwrap_or(&options[0]);
                    shipping_lines.push(ShippingLine {
                        profile_id,
                        rate_id: Some(chosen.0.id),
                        code: chosen.0.code.clone(),
                        name: chosen.0.name.clone(),
                        carrier: chosen.0.carrier.clone(),
                        amount_minor: chosen.1,
                        discount_minor: 0,
                        tax_minor: 0,
                        tax_lines: vec![],
                        allows_cash_on_delivery: chosen.0.allows_cash_on_delivery,
                        cod_fee_minor: chosen.0.cod_fee_minor.unwrap_or(0),
                        options: options
                            .iter()
                            .map(|(r, a)| ShippingOption {
                                rate_id: r.id,
                                code: r.code.clone(),
                                name: r.name.clone(),
                                carrier: r.carrier.clone(),
                                amount_minor: *a,
                                delivery_days_min: r.delivery_days_min,
                                delivery_days_max: r.delivery_days_max,
                                allows_cash_on_delivery: r.allows_cash_on_delivery,
                            })
                            .collect(),
                    });
                }
            }
        }
    }
    let shipping_total: i64 = shipping_lines.iter().map(|l| l.amount_minor).sum();

    // -- Promotions.
    let codes: Vec<String> = input.codes.iter().map(|c| c.to_lowercase()).collect();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut matched_codes: Vec<String> = Vec::new();
    for p in &input.promotions {
        let by_code = p.code.as_ref().map(|c| c.to_lowercase());
        if let Some(code) = &by_code {
            if !codes.contains(code) {
                continue;
            }
            matched_codes.push(code.clone());
        }
        let refuse = |reason: &str, issues: &mut Vec<Issue>| {
            if let Some(code) = &p.code {
                issues.push(issue(
                    IssueCode::PromotionNotApplicable,
                    None,
                    Some(serde_json::json!({ "code": code, "reason": reason })),
                ));
            }
        };
        let in_window = p.window.is_none_or(|(from, to)| {
            from.is_none_or(|f| input.now >= f) && to.is_none_or(|t| input.now < t)
        });
        if !in_window {
            refuse("expired", &mut issues);
            continue;
        }
        if p.exhausted {
            refuse("exhausted", &mut issues);
            continue;
        }
        if p.max_per_customer.is_some_and(|m| {
            input.customer.redemptions.get(&p.id).copied().unwrap_or(0) >= i64::from(m)
        }) {
            refuse("already_used", &mut issues);
            continue;
        }
        let eligible = match p.eligibility {
            CustomerEligibility::All => true,
            CustomerEligibility::FirstOrder => input.customer.first_order,
            CustomerEligibility::StayingGuests => input.customer.staying_guest,
        };
        if !eligible {
            refuse("not_eligible", &mut issues);
            continue;
        }
        let lines: Vec<usize> = (0..input.lines.len())
            .filter(|i| {
                let l = &input.lines[*i];
                p.applies_to_all
                    || p.product_ids.contains(&l.product_id)
                    || l.category_ids.iter().any(|c| p.category_ids.contains(c))
            })
            .collect();
        let eligible_subtotal: i64 = lines.iter().map(|i| subtotals[*i]).sum();
        let eligible_qty: i64 = lines
            .iter()
            .map(|i| i64::from(input.lines[*i].quantity))
            .sum();
        if lines.is_empty() && p.discount_type != DiscountType::FreeShipping {
            refuse("no_eligible_product", &mut issues);
            continue;
        }
        // Free shipping looks at the whole cart; other offers at the products they discount.
        let basis = if p.discount_type == DiscountType::FreeShipping {
            subtotal
        } else {
            eligible_subtotal
        };
        if p.min_subtotal_minor.is_some_and(|m| basis < m) {
            refuse("min_subtotal", &mut issues);
            continue;
        }
        if p.min_quantity.is_some_and(|m| eligible_qty < i64::from(m)) {
            refuse("min_quantity", &mut issues);
            continue;
        }
        let goods_amount = match p.discount_type {
            DiscountType::Percent => percent_of(eligible_subtotal, p.percent.unwrap_or_default()),
            DiscountType::FixedAmount => p.amount_minor.unwrap_or(0).min(eligible_subtotal),
            DiscountType::FreeShipping | DiscountType::FreeNights => 0,
        };
        let shipping_share = match p.discount_type {
            DiscountType::FreeShipping => true,
            DiscountType::Percent => p.applies_to_shipping,
            _ => false,
        };
        if p.discount_type == DiscountType::FreeShipping && shipping_total == 0 {
            let ships = matches!(
                input.method,
                Some(FulfillmentMethod::Shipping | FulfillmentMethod::RelayPoint)
            );
            if ships && !shipping_lines.is_empty() {
                // Shipping already free: nothing to discount, still "applied".
            } else if !ships {
                refuse("shipping_only", &mut issues);
                continue;
            }
        }
        if p.discount_type == DiscountType::FreeNights {
            refuse("not_applicable", &mut issues);
            continue;
        }
        candidates.push(Candidate {
            promo: p,
            lines,
            goods_amount,
            shipping_share,
        });
    }
    for code in &input.codes {
        if !matched_codes.contains(&code.to_lowercase()) {
            issues.push(issue(
                IssueCode::PromotionNotApplicable,
                None,
                Some(serde_json::json!({ "code": code, "reason": "unknown" })),
            ));
        }
    }

    let shipping_value = |c: &Candidate| -> i64 {
        if !c.shipping_share {
            0
        } else if c.promo.discount_type == DiscountType::FreeShipping {
            shipping_total
        } else {
            percent_of(shipping_total, c.promo.percent.unwrap_or_default())
        }
    };
    // Best non-stackable (largest value; ties: earliest id), then every stackable.
    let best = candidates
        .iter()
        .filter(|c| !c.promo.is_stackable)
        .max_by(|a, b| {
            (a.goods_amount + shipping_value(a))
                .cmp(&(b.goods_amount + shipping_value(b)))
                .then(b.promo.id.cmp(&a.promo.id))
        })
        .map(|c| c.promo.id);
    for c in &candidates {
        if !c.promo.is_stackable
            && Some(c.promo.id) != best
            && let Some(code) = &c.promo.code
        {
            issues.push(issue(
                IssueCode::PromotionNotApplicable,
                None,
                Some(serde_json::json!({ "code": code, "reason": "not_combinable" })),
            ));
        }
    }
    let applied: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.promo.is_stackable || Some(c.promo.id) == best)
        .collect();

    // -- Allocate goods discounts across lines.
    let mut line_discounts = vec![0i64; input.lines.len()];
    let mut allocations: Vec<Vec<DiscountAllocation>> = vec![vec![]; input.lines.len()];
    let mut shipping_discount_left = shipping_total;
    let mut applied_out = Vec::new();
    for c in &applied {
        let caps: Vec<i64> = c
            .lines
            .iter()
            .map(|i| subtotals[*i] - line_discounts[*i])
            .collect();
        let room: i64 = caps.iter().sum();
        let amount = c.goods_amount.min(room);
        let weights: Vec<i64> = c.lines.iter().map(|i| subtotals[*i]).collect();
        let shares = allocate(amount, &weights, &caps);
        for (k, i) in c.lines.iter().enumerate() {
            if shares[k] > 0 {
                line_discounts[*i] += shares[k];
                allocations[*i].push(DiscountAllocation {
                    promotion_id: c.promo.id,
                    code: c.promo.code.clone(),
                    amount_minor: shares[k],
                });
            }
        }
        let ship = shipping_value(c).min(shipping_discount_left);
        shipping_discount_left -= ship;
        applied_out.push(AppliedPromotion {
            promotion_id: c.promo.id,
            code: c.promo.code.clone(),
            amount_minor: amount,
            shipping_amount_minor: ship,
        });
    }
    let shipping_discount = shipping_total - shipping_discount_left;
    // Spread the shipping discount over the shipping lines (largest first).
    {
        let weights: Vec<i64> = shipping_lines.iter().map(|l| l.amount_minor).collect();
        let shares = allocate(shipping_discount, &weights, &weights);
        for (l, d) in shipping_lines.iter_mut().zip(shares) {
            l.discount_minor = d;
        }
    }

    // -- Taxes: per line, split by VAT part (bundles), then per rate for the breakdown.
    let inclusive = s.prices_include_tax;
    let mut buckets: BTreeMap<String, (TaxRate, i64)> = BTreeMap::new(); // rate → (rate, goods amount)
    let mut breakdown: BTreeMap<String, VatBucket> = BTreeMap::new();
    let mut add_bucket = |rate: &Decimal, amount: i64, vat: i64| {
        let b = breakdown
            .entry(rate.normalize().to_string())
            .or_insert_with(|| VatBucket {
                rate: rate.normalize(),
                ..VatBucket::default()
            });
        if inclusive {
            b.gross_minor += amount;
            b.base_minor += amount - vat;
        } else {
            b.base_minor += amount;
            b.gross_minor += amount + vat;
        }
        b.vat_minor += vat;
    };
    let zero = TaxRate {
        tax_id: Uuid::nil(),
        code: "none".into(),
        rate_percent: Decimal::ZERO,
    };
    let mut lines_out = Vec::with_capacity(input.lines.len());
    for (i, l) in input.lines.iter().enumerate() {
        let net = subtotals[i] - line_discounts[i];
        let shares_ok = l.vat.iter().map(|p| u64::from(p.share_bp)).sum::<u64>() == 10_000;
        let parts: Vec<VatPart> = if !l.is_taxable {
            vec![VatPart::whole(zero.clone())]
        } else if l.vat.is_empty() || !shares_ok {
            issues.push(issue(IssueCode::TaxUnavailable, Some(l.line_id), None));
            vec![VatPart::whole(zero.clone())]
        } else {
            l.vat.clone()
        };
        let weights: Vec<i64> = parts.iter().map(|p| i64::from(p.share_bp)).collect();
        let portions = allocate(net, &weights, &vec![net; parts.len()]);
        let mut tl = Vec::new();
        for (part, portion) in parts.iter().zip(portions) {
            let t = tax_lines(portion, std::slice::from_ref(&part.tax), inclusive);
            let vat: i64 = t.iter().map(|x| x.amount_minor).sum();
            add_bucket(&part.tax.rate_percent, portion, vat);
            buckets
                .entry(part.tax.rate_percent.normalize().to_string())
                .or_insert_with(|| (part.tax.clone(), 0))
                .1 += portion;
            if l.is_taxable {
                tl.extend(t);
            }
        }
        let tax: i64 = tl.iter().map(|t| t.amount_minor).sum();
        lines_out.push(QuoteLine {
            line_id: l.line_id,
            variant_id: l.variant_id,
            product_id: l.product_id,
            quantity: l.quantity,
            unit_price_minor: l.unit_price_minor,
            compare_at_price_minor: l.compare_at_price_minor,
            subtotal_minor: subtotals[i],
            discount_minor: line_discounts[i],
            tax_minor: tax,
            total_minor: net + if inclusive { 0 } else { tax },
            tax_lines: tl,
            discount_allocations: std::mem::take(&mut allocations[i]),
        });
    }
    // Shipping (and fees) follow the goods: their VAT is split across the goods' rates in
    // proportion to the goods' value at each rate (accessory to the main supply).
    let goods_rates: Vec<(TaxRate, i64)> = buckets.values().cloned().collect();
    let split = |amount: i64| -> Vec<(TaxRate, i64)> {
        let weights: Vec<i64> = goods_rates.iter().map(|(_, w)| *w).collect();
        if amount <= 0 || weights.iter().sum::<i64>() <= 0 {
            return vec![(zero.clone(), amount.max(0))];
        }
        allocate(amount, &weights, &vec![amount; weights.len()])
            .into_iter()
            .zip(&goods_rates)
            .map(|(portion, (rate, _))| (rate.clone(), portion))
            .collect()
    };
    for sl in &mut shipping_lines {
        let mut tl = Vec::new();
        for (rate, portion) in split(sl.amount_minor - sl.discount_minor) {
            let t = tax_lines(portion, std::slice::from_ref(&rate), inclusive);
            add_bucket(
                &rate.rate_percent,
                portion,
                t.iter().map(|x| x.amount_minor).sum(),
            );
            if rate.rate_percent > Decimal::ZERO {
                tl.extend(t);
            }
        }
        sl.tax_minor = tl.iter().map(|t| t.amount_minor).sum();
        sl.tax_lines = tl;
    }

    // -- Cash on delivery.
    let mut cod_fee = 0;
    if input.payment_method == Some(ShopPaymentMethod::CashOnDelivery) {
        let allowed = matches!(
            input.method,
            Some(FulfillmentMethod::Shipping | FulfillmentMethod::RelayPoint)
        ) && !shipping_lines.is_empty()
            && shipping_lines.iter().all(|l| l.allows_cash_on_delivery);
        if allowed {
            cod_fee = shipping_lines
                .iter()
                .map(|l| l.cod_fee_minor)
                .max()
                .unwrap_or(0);
        } else {
            issues.push(issue(
                IssueCode::PaymentMethodUnavailable,
                None,
                Some(serde_json::json!({ "method": "cash_on_delivery" })),
            ));
        }
    }
    if input.payment_method == Some(ShopPaymentMethod::PayAtPickup)
        && input.method != Some(FulfillmentMethod::Pickup)
        && input.method != Some(FulfillmentMethod::StayDelivery)
    {
        issues.push(issue(
            IssueCode::PaymentMethodUnavailable,
            None,
            Some(serde_json::json!({ "method": "pay_at_pickup" })),
        ));
    }

    // -- Totals.
    let mut cod_tax = 0;
    for (rate, portion) in split(cod_fee) {
        let vat: i64 = tax_lines(portion, std::slice::from_ref(&rate), inclusive)
            .iter()
            .map(|x| x.amount_minor)
            .sum();
        add_bucket(&rate.rate_percent, portion, vat);
        cod_tax += vat;
    }
    let discount: i64 = line_discounts.iter().sum();
    let line_tax: i64 = lines_out.iter().map(|l| l.tax_minor).sum();
    let shipping_tax: i64 = shipping_lines.iter().map(|l| l.tax_minor).sum();
    let tax = line_tax + shipping_tax + cod_tax;
    let total = subtotal - discount + shipping_total - shipping_discount
        + cod_fee
        + if inclusive { 0 } else { tax };
    if !input.lines.is_empty() && subtotal - discount < s.min_order_minor {
        issues.push(issue(
            IssueCode::BelowMinOrder,
            None,
            Some(serde_json::json!({ "min_minor": s.min_order_minor })),
        ));
    }

    Quote {
        lines: lines_out,
        shipping_lines,
        promotions: applied_out,
        totals: Totals {
            subtotal_minor: subtotal,
            discount_minor: discount,
            shipping_minor: shipping_total,
            shipping_discount_minor: shipping_discount,
            tax_minor: tax,
            cod_fee_minor: cod_fee,
            total_minor: total,
            taxes_included: inclusive,
            vat_breakdown: breakdown
                .into_values()
                .filter(|b| b.gross_minor != 0 || b.vat_minor != 0)
                .collect(),
        },
        issues,
    }
}

fn issue(code: IssueCode, line_id: Option<Uuid>, data: Option<serde_json::Value>) -> Issue {
    Issue {
        code,
        line_id,
        data: data.unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
#[path = "pricing_tests.rs"]
mod tests;
