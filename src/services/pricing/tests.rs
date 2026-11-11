//! Unit tests of the pure pricing engine.

use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use super::engine::*;
use crate::db::{
    pricing::{Discount, PricingUnit, TaxCalcType},
    types::{CurrencyCode, DateRange, DaysOfWeek, Money},
};

const PRODUCT: Uuid = Uuid::from_u128(1);
const PLAN_FLEX: Uuid = Uuid::from_u128(10);
const ADULT: Uuid = Uuid::from_u128(100);
const CHILD: Uuid = Uuid::from_u128(101);
const INFANT: Uuid = Uuid::from_u128(102);
const LOW: Uuid = Uuid::from_u128(200);
const HIGH: Uuid = Uuid::from_u128(201);

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn rule(id: u128, unit: PricingUnit, amount: i64) -> PriceRule {
    PriceRule {
        id: Uuid::from_u128(id),
        rate_plan_id: None,
        season_id: None,
        guest_category_id: None,
        unit,
        amount_minor: amount,
        extra_guest_amount_minor: None,
        min_quantity: 1,
        max_quantity: None,
        min_nights: None,
        days_of_week: DaysOfWeek::ALL,
        valid_period: None,
        priority: 0,
    }
}

fn ctx(prices: Vec<PriceRule>) -> PricingContext {
    PricingContext {
        currency: CurrencyCode::eur(),
        prices,
        seasons: vec![],
        taxes: vec![],
        guest_categories: vec![
            GuestCategoryInfo {
                id: ADULT,
                counts_toward_capacity: true,
            },
            GuestCategoryInfo {
                id: CHILD,
                counts_toward_capacity: true,
            },
            GuestCategoryInfo {
                id: INFANT,
                counts_toward_capacity: false,
            },
        ],
        promotion: None,
    }
}

fn adults(n: i32) -> Vec<GuestCount> {
    vec![GuestCount {
        guest_category_id: ADULT,
        count: n,
    }]
}

fn stay<'a>(
    guests: &'a [GuestCount],
    arrival: NaiveDate,
    departure: NaiveDate,
) -> PricingRequest<'a> {
    PricingRequest {
        product_id: PRODUCT,
        rate_plan_id: None,
        service: ServiceTime::Stay { arrival, departure },
        guests,
        quantity: 1,
        standard_occupancy: Some(2),
        booked_at: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
        booked_on: d(2026, 11, 1),
    }
}

#[test]
fn nightly_unit_price_times_nights_and_units() {
    let guests = adults(2);
    let mut req = stay(&guests, d(2027, 1, 4), d(2027, 1, 6));
    req.quantity = 2;
    let q = quote(
        &req,
        &ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000)]),
    )
    .unwrap();
    assert_eq!(q.lines.len(), 2);
    assert_eq!(q.subtotal_minor, 3_000 * 2 * 2);
    assert_eq!(q.total_minor, 12_000);
    assert_eq!(q.currency, CurrencyCode::eur());
}

#[test]
fn a_season_price_overrides_the_base_price_on_its_nights() {
    let guests = adults(2);
    let mut c = ctx(vec![
        rule(1, PricingUnit::PerUnitPerNight, 3_000),
        PriceRule {
            season_id: Some(HIGH),
            ..rule(2, PricingUnit::PerUnitPerNight, 5_000)
        },
    ]);
    c.seasons = vec![SeasonRule {
        id: HIGH,
        priority: 1,
        periods: vec![DateRange::new(d(2027, 1, 5), d(2027, 2, 1))],
    }];
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c).unwrap();
    assert_eq!(
        q.lines[0].amount_minor, 3_000,
        "night of the 4th: no season"
    );
    assert_eq!(
        q.lines[1].amount_minor, 5_000,
        "night of the 5th: high season"
    );
    assert_eq!(q.lines[1].season_id, Some(HIGH));
}

#[test]
fn overlapping_seasons_resolve_to_the_highest_priority() {
    let night = d(2027, 3, 10);
    let seasons = vec![
        SeasonRule {
            id: LOW,
            priority: 1,
            periods: vec![DateRange::new(d(2027, 1, 1), d(2027, 12, 31))],
        },
        SeasonRule {
            id: HIGH,
            priority: 5,
            periods: vec![DateRange::new(d(2027, 3, 1), d(2027, 4, 1))],
        },
    ];
    assert_eq!(season_of(night, &seasons), Some(HIGH));
    assert_eq!(season_of(d(2027, 6, 1), &seasons), Some(LOW));
    assert_eq!(season_of(d(2028, 6, 1), &seasons), None);
}

#[test]
fn the_most_specific_rule_wins_then_the_highest_priority() {
    let guests = adults(2);
    let prices = vec![
        PriceRule {
            priority: 10,
            ..rule(1, PricingUnit::PerUnitPerNight, 3_000)
        },
        PriceRule {
            rate_plan_id: Some(PLAN_FLEX),
            ..rule(2, PricingUnit::PerUnitPerNight, 2_500)
        },
    ];
    let mut req = stay(&guests, d(2027, 1, 4), d(2027, 1, 5));
    assert_eq!(
        quote(&req, &ctx(prices.clone())).unwrap().subtotal_minor,
        3_000,
        "no plan: generic row"
    );
    req.rate_plan_id = Some(PLAN_FLEX);
    assert_eq!(
        quote(&req, &ctx(prices)).unwrap().subtotal_minor,
        2_500,
        "plan-specific beats a higher-priority generic row"
    );

    let same_specificity = vec![
        PriceRule {
            priority: 1,
            ..rule(3, PricingUnit::PerUnitPerNight, 1_000)
        },
        PriceRule {
            priority: 5,
            ..rule(4, PricingUnit::PerUnitPerNight, 2_000)
        },
    ];
    req.rate_plan_id = None;
    assert_eq!(
        quote(&req, &ctx(same_specificity)).unwrap().subtotal_minor,
        2_000
    );
}

#[test]
fn weekday_bitmask_prices_weekend_nights() {
    let guests = adults(2);
    let weekend = PriceRule {
        days_of_week: DaysOfWeek(32 | 64),
        ..rule(2, PricingUnit::PerUnitPerNight, 4_000)
    };
    let c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000), weekend]);
    // Friday 8th → Monday 11th January 2027: nights of Fri, Sat, Sun.
    let q = quote(&stay(&guests, d(2027, 1, 8), d(2027, 1, 11)), &c).unwrap();
    let amounts: Vec<i64> = q.lines.iter().map(|l| l.amount_minor).collect();
    assert_eq!(amounts, vec![3_000, 4_000, 4_000]);
}

#[test]
fn per_person_prices_use_each_category_s_own_rule() {
    let guests = vec![
        GuestCount {
            guest_category_id: ADULT,
            count: 2,
        },
        GuestCount {
            guest_category_id: CHILD,
            count: 1,
        },
        GuestCount {
            guest_category_id: INFANT,
            count: 1,
        },
    ];
    let c = ctx(vec![
        rule(1, PricingUnit::PerPersonPerNight, 2_000),
        PriceRule {
            guest_category_id: Some(CHILD),
            ..rule(2, PricingUnit::PerPersonPerNight, 1_000)
        },
        PriceRule {
            guest_category_id: Some(INFANT),
            ..rule(3, PricingUnit::PerPersonPerNight, 0)
        },
    ]);
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 5)), &c).unwrap();
    assert_eq!(q.subtotal_minor, 2 * 2_000 + 1_000);
    assert_eq!(q.lines.len(), 3);
}

#[test]
fn a_category_without_any_price_is_an_error() {
    let guests = vec![
        GuestCount {
            guest_category_id: ADULT,
            count: 1,
        },
        GuestCount {
            guest_category_id: CHILD,
            count: 1,
        },
    ];
    let c = ctx(vec![PriceRule {
        guest_category_id: Some(ADULT),
        ..rule(1, PricingUnit::PerPersonPerNight, 2_000)
    }]);
    let err = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 5)), &c).unwrap_err();
    assert_eq!(
        err,
        PricingError::NoPrice {
            date: d(2027, 1, 4),
            category: Some(CHILD)
        }
    );
}

#[test]
fn guests_above_standard_occupancy_pay_the_extra_guest_amount() {
    let guests = vec![
        GuestCount {
            guest_category_id: ADULT,
            count: 3,
        },
        GuestCount {
            guest_category_id: INFANT,
            count: 1,
        }, // does not count toward capacity
    ];
    let unit = PriceRule {
        extra_guest_amount_minor: Some(800),
        ..rule(1, PricingUnit::PerUnitPerNight, 3_000)
    };
    let q = quote(
        &stay(&guests, d(2027, 1, 4), d(2027, 1, 5)),
        &ctx(vec![unit]),
    )
    .unwrap();
    assert_eq!(q.subtotal_minor, 3_000 + 800);
    assert_eq!(q.lines[1].kind, LineKind::ExtraGuest);
}

#[test]
fn min_nights_rates_apply_only_to_long_enough_stays() {
    let guests = adults(2);
    let c = ctx(vec![
        rule(1, PricingUnit::PerUnitPerNight, 3_000),
        PriceRule {
            min_nights: Some(3),
            ..rule(2, PricingUnit::PerUnitPerNight, 2_500)
        },
    ]);
    assert_eq!(
        quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c)
            .unwrap()
            .subtotal_minor,
        6_000
    );
    assert_eq!(
        quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 7)), &c)
            .unwrap()
            .subtotal_minor,
        7_500
    );
}

#[test]
fn tourist_tax_is_per_person_per_night_with_exempt_categories() {
    let guests = vec![
        GuestCount {
            guest_category_id: ADULT,
            count: 2,
        },
        GuestCount {
            guest_category_id: CHILD,
            count: 1,
        },
        GuestCount {
            guest_category_id: INFANT,
            count: 1,
        },
    ];
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000)]);
    c.taxes = vec![TaxRule {
        id: Uuid::from_u128(300),
        code: "tourist_tax".into(),
        calc_type: TaxCalcType::FixedPerPersonPerNight,
        rate_percent: None,
        amount_minor: Some(250),
        currency: Some(CurrencyCode::eur()),
        is_inclusive: false,
        exempt_guest_category_ids: vec![CHILD, INFANT],
        valid_period: None,
    }];
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 7)), &c).unwrap();
    assert_eq!(q.tax_minor, 250 * 2 * 3, "2 adults × 3 nights");
    assert_eq!(q.total_minor, q.subtotal_minor + 1_500);
}

#[test]
fn percent_taxes_exclusive_are_added_inclusive_are_only_reported() {
    let guests = adults(2);
    let tax = |id: u128, rate: i64, inclusive: bool| TaxRule {
        id: Uuid::from_u128(id),
        code: format!("vat{id}"),
        calc_type: TaxCalcType::Percent,
        rate_percent: Some(Decimal::new(rate, 0)),
        amount_minor: None,
        currency: None,
        is_inclusive: inclusive,
        exempt_guest_category_ids: vec![],
        valid_period: None,
    };
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 12_000)]);
    c.taxes = vec![tax(1, 10, false), tax(2, 20, true)];
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 5)), &c).unwrap();
    assert_eq!(q.taxes[0].amount_minor, 1_200);
    assert_eq!(q.taxes[1].amount_minor, 2_000);
    assert!(q.taxes[1].inclusive);
    assert_eq!(q.total_minor, 12_000 + 1_200);
}

fn promotion(discount: Discount) -> PromotionRule {
    PromotionRule {
        id: Uuid::from_u128(400),
        code: Some("DESERT10".into()),
        discount,
        min_subtotal: None,
        min_nights: None,
        min_days_before_arrival: None,
        booking_window: None,
        stay_window: None,
        applies_to_all: true,
        product_ids: vec![],
    }
}

#[test]
fn percent_promotion_is_applied_before_taxes() {
    let guests = adults(2);
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000)]);
    c.promotion = Some(promotion(Discount::Percent {
        percent: Decimal::new(10, 0),
    }));
    c.taxes = vec![TaxRule {
        id: Uuid::from_u128(1),
        code: "vat".into(),
        calc_type: TaxCalcType::Percent,
        rate_percent: Some(Decimal::new(10, 0)),
        amount_minor: None,
        currency: None,
        is_inclusive: false,
        exempt_guest_category_ids: vec![],
        valid_period: None,
    }];
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c).unwrap();
    assert_eq!(q.discount_minor, 600);
    assert_eq!(q.tax_minor, 540, "10 % of the discounted 5 400");
    assert_eq!(q.total_minor, 6_000 - 600 + 540);
}

#[test]
fn free_nights_discount_the_cheapest_nights() {
    let guests = adults(2);
    let weekend = PriceRule {
        days_of_week: DaysOfWeek(32 | 64),
        ..rule(2, PricingUnit::PerUnitPerNight, 4_000)
    };
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000), weekend]);
    c.promotion = Some(promotion(Discount::FreeNights { nights: 1 }));
    let q = quote(&stay(&guests, d(2027, 1, 8), d(2027, 1, 11)), &c).unwrap();
    assert_eq!(q.discount_minor, 3_000, "the Friday night is the cheapest");
}

#[test]
fn fixed_amount_promotion_is_in_minor_units_and_capped_at_the_subtotal() {
    let guests = adults(2);
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 1_000)]);
    c.promotion = Some(promotion(Discount::FixedAmount {
        amount: Money::new(1_500, CurrencyCode::eur()), // 15.00 €
    }));
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 5)), &c).unwrap();
    assert_eq!(
        q.discount_minor, 1_000,
        "capped: a promotion never makes the total negative"
    );
    assert_eq!(q.total_minor, 0);
}

#[test]
fn promotion_conditions_are_enforced() {
    let guests = adults(2);
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000)]);
    c.promotion = Some(PromotionRule {
        min_nights: Some(3),
        ..promotion(Discount::Percent {
            percent: Decimal::TEN,
        })
    });
    let err = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c).unwrap_err();
    assert_eq!(err, PricingError::PromotionNotApplicable("stay too short"));

    c.promotion = Some(PromotionRule {
        applies_to_all: false,
        product_ids: vec![Uuid::from_u128(999)],
        ..promotion(Discount::Percent {
            percent: Decimal::TEN,
        })
    });
    assert!(matches!(
        quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c),
        Err(PricingError::PromotionNotApplicable(_))
    ));
}

#[test]
fn activities_are_priced_per_person_or_per_hour_on_their_date() {
    let guests = adults(2);
    let mut req = stay(&guests, d(2027, 1, 4), d(2027, 1, 5));
    req.service = ServiceTime::Date {
        date: d(2027, 1, 4),
        hours: Some(2),
        days: None,
    };
    let per_person = quote(&req, &ctx(vec![rule(1, PricingUnit::PerPerson, 4_500)])).unwrap();
    assert_eq!(per_person.subtotal_minor, 9_000);

    req.quantity = 2; // two quads
    let per_hour = quote(&req, &ctx(vec![rule(2, PricingUnit::PerHour, 3_000)])).unwrap();
    assert_eq!(per_hour.subtotal_minor, 3_000 * 2 * 2);

    let nightly_only = quote(
        &req,
        &ctx(vec![rule(3, PricingUnit::PerPersonPerNight, 1_000)]),
    );
    assert!(
        matches!(nightly_only, Err(PricingError::NoPrice { .. })),
        "nightly prices do not apply to dated services"
    );
}

#[test]
fn invalid_requests_are_rejected() {
    let guests = adults(2);
    let req = stay(&guests, d(2027, 1, 6), d(2027, 1, 6));
    assert!(matches!(
        quote(&req, &ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 1)])),
        Err(PricingError::InvalidRequest(_))
    ));
}

#[test]
fn amounts_in_another_currency_never_apply() {
    let guests = adults(2);
    let mut c = ctx(vec![rule(1, PricingUnit::PerUnitPerNight, 3_000)]);
    c.promotion = Some(promotion(Discount::FixedAmount {
        amount: Money::new(10_000, CurrencyCode::new("MAD")),
    }));
    assert!(matches!(
        quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c),
        Err(PricingError::PromotionNotApplicable(_))
    ));

    // A 50 MAD minimum is not "5 000 EUR cents".
    c.promotion = Some(PromotionRule {
        min_subtotal: Some(Money::new(5_000, CurrencyCode::new("MAD"))),
        ..promotion(Discount::Percent {
            percent: Decimal::TEN,
        })
    });
    assert!(matches!(
        quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c),
        Err(PricingError::PromotionNotApplicable(_))
    ));
    c.promotion = Some(PromotionRule {
        min_subtotal: Some(Money::new(5_000, CurrencyCode::eur())),
        ..promotion(Discount::Percent {
            percent: Decimal::TEN,
        })
    });
    let q = quote(&stay(&guests, d(2027, 1, 4), d(2027, 1, 6)), &c).unwrap();
    assert_eq!(q.discount_minor, 600);
}
