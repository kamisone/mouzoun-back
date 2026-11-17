use std::str::FromStr;

use chrono::TimeZone;
use proptest::prelude::*;

use super::*;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn tva(rate: &str) -> TaxRate {
    TaxRate {
        tax_id: id(900),
        code: "tva".into(),
        rate_percent: Decimal::from_str(rate).unwrap(),
    }
}

const STANDARD: Uuid = Uuid::from_u128(500);
const LIQUIDS: Uuid = Uuid::from_u128(501);
const MOROCCO: Uuid = Uuid::from_u128(600);
const EUROPE: Uuid = Uuid::from_u128(601);

fn line(n: u128, unit: i64, qty: i32) -> LineInput {
    LineInput {
        line_id: id(n),
        variant_id: id(100 + n),
        product_id: id(200 + n),
        category_ids: vec![],
        quantity: qty,
        unit_price_minor: unit,
        compare_at_price_minor: None,
        vat: vec![VatPart::whole(tva("20"))],
        is_taxable: true,
        weight_grams: 600,
        requires_shipping: true,
        shipping_profile_id: STANDARD,
        available: None,
        max_per_order: None,
        purchasable: true,
    }
}

fn rate(n: u128, zone: Uuid, profile: Uuid, kind: ShippingRateKind, amount: i64) -> RateInput {
    RateInput {
        id: id(n),
        zone_id: zone,
        profile_id: profile,
        code: format!("r{n}"),
        name: format!("Rate {n}"),
        kind,
        delivery_type: DeliveryType::Home,
        carrier: Some("amana".into()),
        amount_minor: Some(amount),
        per_kg_minor: None,
        min_weight_grams: None,
        max_weight_grams: None,
        min_subtotal_minor: None,
        max_subtotal_minor: None,
        free_over_minor: None,
        delivery_days_min: Some(2),
        delivery_days_max: Some(4),
        allows_cash_on_delivery: false,
        cod_fee_minor: None,
        sort_order: 0,
    }
}

fn zones() -> Vec<ZoneInput> {
    vec![
        ZoneInput {
            id: MOROCCO,
            country_codes: vec!["MA".into()],
            region_codes: None,
            postal_code_patterns: None,
            is_export: false,
            sort_order: 0,
        },
        ZoneInput {
            id: EUROPE,
            country_codes: vec!["FR".into(), "ES".into(), "DE".into()],
            region_codes: None,
            postal_code_patterns: None,
            is_export: true,
            sort_order: 1,
        },
    ]
}

fn promo(n: u128, code: Option<&str>, kind: DiscountType) -> PromotionInput {
    PromotionInput {
        id: id(n),
        code: code.map(str::to_string),
        discount_type: kind,
        percent: None,
        amount_minor: None,
        min_subtotal_minor: None,
        min_quantity: None,
        window: None,
        applies_to_all: true,
        product_ids: vec![],
        category_ids: vec![],
        applies_to_shipping: false,
        is_stackable: false,
        eligibility: CustomerEligibility::All,
        exhausted: false,
        max_per_customer: None,
    }
}

fn input(lines: Vec<LineInput>) -> QuoteInput {
    QuoteInput {
        lines,
        method: Some(FulfillmentMethod::Pickup),
        destination: Destination::default(),
        zones: zones(),
        rates: vec![],
        chosen_rate_ids: vec![],
        promotions: vec![],
        codes: vec![],
        customer: CustomerFacts::default(),
        payment_method: None,
        settings: PricingSettings::default(),
        now: Utc.with_ymd_and_hms(2027, 3, 1, 10, 0, 0).unwrap(),
    }
}

fn shipping_to(mut i: QuoteInput, country: &str) -> QuoteInput {
    i.method = Some(FulfillmentMethod::Shipping);
    i.destination.country_code = Some(country.into());
    i
}

fn assert_consistent(q: &Quote) {
    let t = &q.totals;
    assert_eq!(
        t.subtotal_minor,
        q.lines.iter().map(|l| l.subtotal_minor).sum::<i64>()
    );
    assert_eq!(
        t.discount_minor,
        q.lines.iter().map(|l| l.discount_minor).sum::<i64>()
    );
    for l in &q.lines {
        assert_eq!(
            l.discount_minor,
            l.discount_allocations
                .iter()
                .map(|a| a.amount_minor)
                .sum::<i64>()
        );
        assert!(l.discount_minor <= l.subtotal_minor);
        assert_eq!(
            l.tax_minor,
            l.tax_lines.iter().map(|t| t.amount_minor).sum::<i64>()
        );
    }
    // The VAT breakdown covers the whole order, to the cent.
    assert_eq!(
        t.vat_breakdown.iter().map(|b| b.vat_minor).sum::<i64>(),
        t.tax_minor
    );
    assert_eq!(
        t.vat_breakdown.iter().map(|b| b.gross_minor).sum::<i64>(),
        t.total_minor
    );
    for b in &t.vat_breakdown {
        assert_eq!(b.base_minor + b.vat_minor, b.gross_minor);
    }
    let tax = if t.taxes_included { 0 } else { t.tax_minor };
    assert_eq!(
        t.total_minor,
        t.subtotal_minor - t.discount_minor + t.shipping_minor - t.shipping_discount_minor
            + t.cod_fee_minor
            + tax
    );
}

#[test]
fn inclusive_tax_is_computed_per_line_after_discount() {
    // 12.00 incl. 20 % → tax = 1200 − round(1200 / 1.2) = 200.
    let q = price_cart(&input(vec![line(1, 1200, 1), line(2, 999, 1)]));
    assert_eq!(q.lines[0].tax_minor, 200);
    // 9.99 → 999 − round(832.5) = 999 − 833 = 166.
    assert_eq!(q.lines[1].tax_minor, 166);
    assert_eq!(q.totals.tax_minor, 366);
    assert_eq!(q.totals.total_minor, 2199, "inclusive: tax is not added");
    assert_consistent(&q);

    let mut i = input(vec![line(1, 1000, 1)]);
    i.settings.prices_include_tax = false;
    let q = price_cart(&i);
    assert_eq!(q.lines[0].tax_minor, 200);
    assert_eq!(q.totals.total_minor, 1200, "exclusive: tax is added");
}

#[test]
fn a_discount_is_allocated_exactly_across_lines() {
    let mut i = input(vec![line(1, 333, 1), line(2, 333, 1), line(3, 334, 1)]);
    let mut p = promo(10, Some("TEN"), DiscountType::FixedAmount);
    p.amount_minor = Some(100);
    i.promotions = vec![p];
    i.codes = vec!["ten".into()];
    let q = price_cart(&i);
    let shares: Vec<i64> = q.lines.iter().map(|l| l.discount_minor).collect();
    assert_eq!(shares.iter().sum::<i64>(), 100);
    assert_eq!(shares, vec![33, 33, 34], "remainder to the largest line");
    assert_consistent(&q);
}

#[test]
fn the_best_non_stackable_promotion_wins_then_stackables_add_up() {
    let mut i = input(vec![line(1, 5000, 1)]);
    let mut ten = promo(10, Some("TEN"), DiscountType::Percent);
    ten.percent = Some(Decimal::from(10));
    let mut five = promo(11, Some("FIVE"), DiscountType::FixedAmount);
    five.amount_minor = Some(500);
    five.percent = None;
    let mut fifteen = promo(12, Some("BIG"), DiscountType::Percent);
    fifteen.percent = Some(Decimal::from(15));
    let mut stack = promo(13, None, DiscountType::FixedAmount);
    stack.amount_minor = Some(100);
    stack.is_stackable = true;
    i.promotions = vec![ten, five, fifteen, stack];
    i.codes = vec!["TEN".into(), "FIVE".into(), "BIG".into()];
    let q = price_cart(&i);
    assert_eq!(q.totals.discount_minor, 750 + 100);
    let ids: Vec<Uuid> = q.promotions.iter().map(|p| p.promotion_id).collect();
    assert_eq!(ids, vec![id(12), id(13)]);
    let refused: Vec<&serde_json::Value> = q
        .issues
        .iter()
        .filter(|x| x.code == IssueCode::PromotionNotApplicable)
        .map(|x| &x.data["code"])
        .collect();
    assert_eq!(
        refused.len(),
        2,
        "TEN and FIVE are not combinable: {:?}",
        q.issues
    );
    assert_consistent(&q);
}

#[test]
fn free_shipping_discounts_the_whole_shipping() {
    let mut i = shipping_to(input(vec![line(1, 2000, 1)]), "MA");
    i.rates = vec![rate(1, MOROCCO, STANDARD, ShippingRateKind::Flat, 450)];
    let mut p = promo(10, Some("SHIP"), DiscountType::FreeShipping);
    p.min_subtotal_minor = Some(1500);
    i.promotions = vec![p];
    i.codes = vec!["ship".into()];
    let q = price_cart(&i);
    assert_eq!(q.totals.shipping_minor, 450);
    assert_eq!(q.totals.shipping_discount_minor, 450);
    assert_eq!(q.totals.total_minor, 2000);
    assert_consistent(&q);
}

#[test]
fn weight_based_rates_are_summed_per_profile() {
    let mut oil = line(2, 1800, 2);
    oil.shipping_profile_id = LIQUIDS;
    oil.weight_grams = 1100;
    let mut i = shipping_to(input(vec![line(1, 1000, 1), oil]), "FR");
    let mut std_rate = rate(1, EUROPE, STANDARD, ShippingRateKind::WeightBased, 900);
    std_rate.per_kg_minor = Some(300);
    let mut liq_rate = rate(2, EUROPE, LIQUIDS, ShippingRateKind::WeightBased, 1200);
    liq_rate.per_kg_minor = Some(500);
    i.rates = vec![std_rate, liq_rate];
    let q = price_cart(&i);
    // standard: 0.6 kg → 1 kg → 900 + 300; liquids: 2.2 kg → 3 kg → 1200 + 1500.
    let amounts: Vec<i64> = q.shipping_lines.iter().map(|l| l.amount_minor).collect();
    assert_eq!(amounts, vec![1200, 2700]);
    assert_eq!(q.totals.shipping_minor, 3900);
    assert!(q.blocking_issues().is_empty(), "{:?}", q.issues);
    assert_consistent(&q);
}

#[test]
fn a_liquid_to_a_zone_without_a_liquids_rate_cannot_ship() {
    let mut oil = line(2, 1800, 1);
    oil.shipping_profile_id = LIQUIDS;
    let mut i = shipping_to(input(vec![line(1, 1000, 1), oil]), "FR");
    i.rates = vec![rate(1, EUROPE, STANDARD, ShippingRateKind::Flat, 900)];
    let q = price_cart(&i);
    let issues: Vec<(&IssueCode, Option<Uuid>)> =
        q.issues.iter().map(|x| (&x.code, x.line_id)).collect();
    assert_eq!(
        issues,
        vec![(&IssueCode::NotShippableToCountry, Some(id(2)))]
    );
    assert_eq!(
        q.shipping_lines.len(),
        1,
        "the standard group still has its rate"
    );
}

#[test]
fn cash_on_delivery_adds_the_fee_of_the_rates() {
    let mut i = shipping_to(input(vec![line(1, 2000, 1)]), "MA");
    let mut r = rate(1, MOROCCO, STANDARD, ShippingRateKind::Flat, 450);
    r.allows_cash_on_delivery = true;
    r.cod_fee_minor = Some(150);
    i.rates = vec![r.clone()];
    i.payment_method = Some(ShopPaymentMethod::CashOnDelivery);
    let q = price_cart(&i);
    assert_eq!(q.totals.cod_fee_minor, 150);
    assert_eq!(q.totals.total_minor, 2000 + 450 + 150);
    assert_consistent(&q);

    r.allows_cash_on_delivery = false;
    i.rates = vec![r];
    let q = price_cart(&i);
    assert_eq!(
        q.blocking_issues()[0].code,
        IssueCode::PaymentMethodUnavailable
    );
}

#[test]
fn free_over_and_cheapest_rate_are_respected() {
    let mut i = shipping_to(input(vec![line(1, 6000, 1)]), "MA");
    let mut flat = rate(1, MOROCCO, STANDARD, ShippingRateKind::Flat, 450);
    flat.free_over_minor = Some(5000);
    let express = rate(2, MOROCCO, STANDARD, ShippingRateKind::Flat, 900);
    i.rates = vec![flat, express];
    let q = price_cart(&i);
    assert_eq!(q.shipping_lines[0].amount_minor, 0);
    assert_eq!(q.shipping_lines[0].options.len(), 2);
    i.chosen_rate_ids = vec![id(2)];
    let q = price_cart(&i);
    assert_eq!(
        q.shipping_lines[0].amount_minor, 900,
        "the customer's choice is kept"
    );
}

#[test]
fn stock_limits_and_unavailable_products_are_issues() {
    let mut a = line(1, 1000, 3);
    a.available = Some(2);
    let mut b = line(2, 1000, 1);
    b.available = Some(0);
    let mut c = line(3, 1000, 1);
    c.purchasable = false;
    let mut d = line(4, 1000, 30);
    d.available = None;
    let q = price_cart(&input(vec![a, b, c, d]));
    let codes: Vec<IssueCode> = q.issues.iter().map(|x| x.code).collect();
    assert_eq!(
        codes,
        vec![
            IssueCode::QuantityLimited,
            IssueCode::OutOfStock,
            IssueCode::ProductUnavailable,
            IssueCode::QuantityLimited
        ]
    );
    assert_eq!(q.issues[0].data["max"], 2);
    assert_eq!(q.issues[3].data["max"], 20);
}

#[test]
fn first_order_promotions_need_a_first_order() {
    let mut i = input(vec![line(1, 1000, 1)]);
    let mut p = promo(10, Some("BIENVENUE10"), DiscountType::Percent);
    p.percent = Some(Decimal::from(10));
    p.eligibility = CustomerEligibility::FirstOrder;
    i.promotions = vec![p];
    i.codes = vec!["bienvenue10".into()];
    let q = price_cart(&i);
    assert_eq!(q.totals.discount_minor, 0);
    assert_eq!(q.issues[0].data["reason"], "not_eligible");
    i.customer.first_order = true;
    assert_eq!(price_cart(&i).totals.discount_minor, 100);
}

#[test]
fn category_promotions_only_discount_their_products() {
    let dates = id(77);
    let mut a = line(1, 1000, 1);
    a.category_ids = vec![dates];
    let b = line(2, 1000, 1);
    let mut i = input(vec![a, b]);
    let mut p = promo(10, None, DiscountType::Percent);
    p.percent = Some(Decimal::from(10));
    p.applies_to_all = false;
    p.category_ids = vec![dates];
    i.promotions = vec![p];
    let q = price_cart(&i);
    assert_eq!(q.lines[0].discount_minor, 100);
    assert_eq!(q.lines[1].discount_minor, 0);
}

fn bucket(q: &Quote, rate: &str) -> VatBucket {
    q.totals
        .vat_breakdown
        .iter()
        .find(|b| b.rate == Decimal::from_str(rate).unwrap().normalize())
        .cloned()
        .unwrap_or_default()
}

#[test]
fn vat_breakdown_sums_per_rate() {
    // Dates at 5.5 %, a basket at 20 %: prices incl. VAT.
    let mut dates = line(1, 1290, 2);
    dates.vat = vec![VatPart::whole(tva("5.5"))];
    let basket = line(2, 2400, 1);
    let q = price_cart(&input(vec![dates, basket]));
    assert_consistent(&q);
    assert_eq!(
        bucket(&q, "5.5"),
        VatBucket {
            rate: Decimal::from_str("5.5").unwrap().normalize(),
            base_minor: 2445,
            vat_minor: 135,
            gross_minor: 2580
        }
    );
    assert_eq!(bucket(&q, "20").vat_minor, 400);
    assert_eq!(q.totals.tax_minor, 535);
}

#[test]
fn a_gift_box_splits_its_vat_by_share() {
    // 60 % food (5.5 %), 40 % basket (20 %) of 50.00 €.
    let mut gift = line(1, 5000, 1);
    gift.vat = vec![
        VatPart {
            tax: tva("5.5"),
            share_bp: 6000,
        },
        VatPart {
            tax: tva("20"),
            share_bp: 4000,
        },
    ];
    let q = price_cart(&input(vec![gift]));
    assert_consistent(&q);
    assert_eq!(bucket(&q, "5.5").gross_minor, 3000);
    assert_eq!(bucket(&q, "20").gross_minor, 2000);
    assert_eq!(q.lines[0].tax_lines.len(), 2);
    assert_eq!(q.lines[0].tax_minor, 156 + 333);
}

#[test]
fn shipping_vat_follows_the_goods_by_value() {
    // Goods: 30.00 at 5.5 %, 10.00 at 20 % → 6.00 shipping split 4.50 / 1.50.
    let mut food = line(1, 3000, 1);
    food.vat = vec![VatPart::whole(tva("5.5"))];
    let basket = line(2, 1000, 1);
    let mut i = shipping_to(input(vec![food, basket]), "FR");
    i.rates = vec![rate(1, EUROPE, STANDARD, ShippingRateKind::Flat, 600)];
    let q = price_cart(&i);
    assert_consistent(&q);
    let s = &q.shipping_lines[0];
    assert_eq!(s.tax_lines.len(), 2);
    assert_eq!(s.tax_minor, 23 + 25);
    assert_eq!(bucket(&q, "5.5").gross_minor, 3450);
    assert_eq!(bucket(&q, "20").gross_minor, 1150);
}

#[test]
fn a_product_without_rate_blocks_the_sale() {
    let mut l = line(1, 1000, 1);
    l.vat = vec![];
    let codes: Vec<IssueCode> = price_cart(&input(vec![l]))
        .issues
        .iter()
        .map(|x| x.code)
        .collect();
    assert_eq!(codes, vec![IssueCode::TaxUnavailable]);
    let mut bad = line(1, 1000, 1);
    bad.vat = vec![VatPart {
        tax: tva("5.5"),
        share_bp: 5000,
    }];
    assert!(
        price_cart(&input(vec![bad]))
            .issues
            .iter()
            .any(|x| x.code == IssueCode::TaxUnavailable)
    );
}

#[test]
fn relay_point_delivery_uses_relay_rates_only() {
    let mut i = shipping_to(input(vec![line(1, 1200, 1)]), "FR");
    let mut relay = rate(2, EUROPE, STANDARD, ShippingRateKind::Flat, 450);
    relay.delivery_type = DeliveryType::RelayPoint;
    i.rates = vec![
        rate(1, EUROPE, STANDARD, ShippingRateKind::Flat, 790),
        relay,
    ];
    assert_eq!(price_cart(&i).shipping_lines[0].amount_minor, 790);
    i.method = Some(FulfillmentMethod::RelayPoint);
    assert_eq!(price_cart(&i).shipping_lines[0].amount_minor, 450);
}

#[test]
fn below_minimum_and_missing_address_block_the_checkout() {
    let mut i = shipping_to(input(vec![line(1, 500, 1)]), "MA");
    i.destination.country_code = None;
    i.settings.min_order_minor = 1000;
    let codes: Vec<IssueCode> = price_cart(&i).issues.iter().map(|x| x.code).collect();
    assert_eq!(
        codes,
        vec![IssueCode::AddressRequired, IssueCode::BelowMinOrder]
    );
}

#[test]
fn postal_patterns_select_the_most_specific_zone() {
    let mut z = zones();
    z.push(ZoneInput {
        id: id(602),
        country_codes: vec!["MA".into()],
        region_codes: None,
        postal_code_patterns: Some(vec!["52*".into()]),
        is_export: false,
        sort_order: 5,
    });
    let dest = Destination {
        country_code: Some("MA".into()),
        region: None,
        postal_code: Some("52 000".into()),
    };
    assert_eq!(zone_for(&z, &dest).unwrap().id, id(602));
    let elsewhere = Destination {
        postal_code: Some("40000".into()),
        ..dest
    };
    assert_eq!(zone_for(&z, &elsewhere).unwrap().id, MOROCCO);
}

#[test]
fn allocation_respects_caps() {
    assert_eq!(allocate(10, &[1, 1, 1], &[2, 100, 100]), vec![2, 4, 4]);
    assert_eq!(allocate(0, &[5, 5], &[5, 5]), vec![0, 0]);
    assert_eq!(allocate(7, &[0, 0], &[3, 10]), vec![3, 4]);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Rounding never creates or loses a cent: allocations sum to the discount, line totals and
    /// order totals agree, taxes never exceed the amount they are taken from.
    #[test]
    fn rounding_never_creates_or_loses_a_cent(
        prices in proptest::collection::vec((1i64..50_000, 1i32..6, 0u32..3), 1..8),
        discount in 0i64..80_000,
        percent in 0u32..10_000,
        inclusive in any::<bool>(),
        shipping in 0i64..3_000,
    ) {
        let rates = ["0", "7", "10", "14", "20"];
        let lines: Vec<LineInput> = prices.iter().enumerate().map(|(n, (unit, qty, r))| {
            let mut l = line(n as u128 + 1, *unit, *qty);
            l.vat = vec![VatPart { tax: tva(rates[*r as usize]), share_bp: 6000 }, VatPart { tax: tva("5.5"), share_bp: 4000 }];
            l
        }).collect();
        let mut i = shipping_to(input(lines), "MA");
        i.settings.prices_include_tax = inclusive;
        i.rates = vec![rate(1, MOROCCO, STANDARD, ShippingRateKind::Flat, shipping)];
        let mut fixed = promo(10, None, DiscountType::FixedAmount);
        fixed.amount_minor = Some(discount.max(1));
        fixed.is_stackable = true;
        let mut pct = promo(11, None, DiscountType::Percent);
        pct.percent = Some(Decimal::new(i64::from(percent.max(1)), 2));
        pct.applies_to_shipping = true;
        i.promotions = vec![fixed, pct];
        let q = price_cart(&i);
        assert_consistent(&q);
        let applied: i64 = q.promotions.iter().map(|p| p.amount_minor).sum();
        prop_assert_eq!(applied, q.totals.discount_minor);
        prop_assert!(q.totals.discount_minor <= q.totals.subtotal_minor);
        prop_assert!(q.totals.shipping_discount_minor <= q.totals.shipping_minor);
        for l in &q.lines {
            let net = l.subtotal_minor - l.discount_minor;
            prop_assert!(l.tax_minor >= 0);
            if inclusive { prop_assert!(l.tax_minor <= net); }
        }
        prop_assert!(q.totals.total_minor >= 0);
    }
}
