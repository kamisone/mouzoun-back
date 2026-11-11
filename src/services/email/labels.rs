//! Fixed wording of the code-defined parts of an email (layout footer, booking summary,
//! cancellation policy, guest counts). Template bodies themselves are edited in the admin.
//!
//! Languages without an entry here use English wording (add one when adding a locale).

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Labels {
    pub reference: &'static str,
    pub arrival: &'static str,
    pub departure: &'static str,
    pub guests: &'static str,
    pub details: &'static str,
    pub discount: &'static str,
    pub taxes: &'static str,
    pub total: &'static str,
    pub paid: &'static str,
    pub balance: &'static str,
    pub due_on: &'static str,
    pub cancellation_policy: &'static str,
    pub phone: &'static str,
}

pub fn labels(locale: &str) -> Labels {
    match base(locale) {
        "fr" => Labels {
            reference: "Référence",
            arrival: "Arrivée",
            departure: "Départ",
            guests: "Voyageurs",
            details: "Votre réservation",
            discount: "Remise",
            taxes: "Taxes",
            total: "Total",
            paid: "Déjà réglé",
            balance: "Reste à payer",
            due_on: "avant le",
            cancellation_policy: "Conditions d'annulation",
            phone: "Téléphone",
        },
        "ar" => Labels {
            reference: "المرجع",
            arrival: "الوصول",
            departure: "المغادرة",
            guests: "الضيوف",
            details: "حجزك",
            discount: "الخصم",
            taxes: "الضرائب",
            total: "المجموع",
            paid: "المبلغ المدفوع",
            balance: "المبلغ المتبقي",
            due_on: "قبل",
            cancellation_policy: "شروط الإلغاء",
            phone: "الهاتف",
        },
        _ => Labels {
            reference: "Reference",
            arrival: "Arrival",
            departure: "Departure",
            guests: "Guests",
            details: "Your booking",
            discount: "Discount",
            taxes: "Taxes",
            total: "Total",
            paid: "Already paid",
            balance: "Balance due",
            due_on: "by",
            cancellation_policy: "Cancellation policy",
            phone: "Phone",
        },
    }
}

/// `2 adultes, 1 enfant` · `2 adults, 1 child` · `البالغون: 2، الأطفال: 1`.
pub fn guests_label(adults: i64, children: i64, infants: i64, locale: &str) -> String {
    let parts: Vec<String> = [(adults, 0), (children, 1), (infants, 2)]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, kind)| match base(locale) {
            "fr" => {
                let word = ["adulte", "enfant", "bébé"][kind];
                format!("{n} {word}{}", if n > 1 { "s" } else { "" })
            }
            "ar" => format!("{}: {n}", ["البالغون", "الأطفال", "الرضع"][kind]),
            _ => {
                let (one, many) = [
                    ("adult", "adults"),
                    ("child", "children"),
                    ("infant", "infants"),
                ][kind];
                format!("{n} {}", if n == 1 { one } else { many })
            }
        })
        .collect();
    parts.join(if base(locale) == "ar" { "، " } else { ", " })
}

/// `2 nuits` · `2 nights` · `ليلتان`.
pub fn nights_label(nights: i64, locale: &str) -> String {
    match base(locale) {
        "fr" => format!("{nights} nuit{}", if nights > 1 { "s" } else { "" }),
        "ar" => match nights {
            1 => "ليلة واحدة".into(),
            2 => "ليلتان".into(),
            3..=10 => format!("{nights} ليالٍ"),
            _ => format!("{nights} ليلة"),
        },
        _ => format!("{nights} night{}", if nights == 1 { "" } else { "s" }),
    }
}

/// `3 participants` · `3 participants` · `3 مشاركين`.
pub fn participants_label(count: i64, locale: &str) -> String {
    match base(locale) {
        "fr" => format!("{count} participant{}", if count > 1 { "s" } else { "" }),
        "ar" => match count {
            1 => "مشارك واحد".into(),
            2 => "مشاركان".into(),
            3..=10 => format!("{count} مشاركين"),
            _ => format!("{count} مشاركًا"),
        },
        _ => format!("{count} participant{}", if count == 1 { "" } else { "s" }),
    }
}

/// Cancellation rules (`[{min_hours_before, refund_percent}]`, the booking's snapshot) as
/// sentences, most generous first.
pub fn policy_lines(rules: &Value, locale: &str) -> Vec<String> {
    let mut rules: Vec<(i64, i64)> = rules
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    Some((
                        r.get("min_hours_before")?.as_i64()?,
                        r.get("refund_percent")?.as_i64()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    rules.sort_by_key(|r| std::cmp::Reverse(r.0));

    let mut lines = Vec::with_capacity(rules.len());
    for (i, &(hours, percent)) in rules.iter().enumerate() {
        let refund = refund(percent, locale);
        let line = if hours > 0 {
            match base(locale) {
                "fr" => format!(
                    "Annulation au moins {} avant l'arrivée : {refund}",
                    duration(hours, locale)
                ),
                "ar" => format!(
                    "الإلغاء قبل {} على الأقل من الوصول: {refund}",
                    duration(hours, locale)
                ),
                _ => format!(
                    "Cancellation at least {} before arrival: {refund}",
                    duration(hours, locale)
                ),
            }
        } else {
            let previous = i.checked_sub(1).map(|p| rules[p].0).filter(|h| *h > 0);
            match (base(locale), previous) {
                ("fr", Some(h)) => format!(
                    "Annulation moins de {} avant l'arrivée : {refund}",
                    duration(h, locale)
                ),
                ("fr", None) => format!("Annulation : {refund}"),
                ("ar", Some(h)) => format!(
                    "الإلغاء قبل أقل من {} من الوصول: {refund}",
                    duration(h, locale)
                ),
                ("ar", None) => format!("الإلغاء: {refund}"),
                (_, Some(h)) => format!(
                    "Cancellation less than {} before arrival: {refund}",
                    duration(h, locale)
                ),
                (_, None) => format!("Cancellation: {refund}"),
            }
        };
        lines.push(line);
    }
    lines
}

fn refund(percent: i64, locale: &str) -> String {
    match (base(locale), percent) {
        ("fr", 0) => "aucun remboursement".into(),
        ("fr", 100) => "remboursement intégral".into(),
        ("fr", p) => format!("remboursement de {p}\u{a0}%"),
        ("ar", 0) => "لا يوجد استرداد".into(),
        ("ar", 100) => "استرداد كامل المبلغ".into(),
        ("ar", p) => format!("استرداد {p}٪"),
        (_, 0) => "no refund".into(),
        (_, 100) => "full refund".into(),
        (_, p) => format!("{p}% refund"),
    }
}

fn duration(hours: i64, locale: &str) -> String {
    let (n, day) = if hours % 24 == 0 {
        (hours / 24, true)
    } else {
        (hours, false)
    };
    match (base(locale), day) {
        ("fr", true) => format!("{n} jour{}", if n > 1 { "s" } else { "" }),
        ("fr", false) => format!("{n}\u{a0}h"),
        ("ar", true) => match n {
            1 => "يوم واحد".into(),
            2 => "يومين".into(),
            3..=10 => format!("{n} أيام"),
            _ => format!("{n} يومًا"),
        },
        ("ar", false) => match n {
            3..=10 => format!("{n} ساعات"),
            _ => format!("{n} ساعة"),
        },
        (_, true) => format!("{n} day{}", if n == 1 { "" } else { "s" }),
        (_, false) => format!("{n} hours"),
    }
}

fn base(locale: &str) -> &str {
    match locale.split(['-', '_']).next().unwrap_or("") {
        "fr" => "fr",
        "ar" => "ar",
        _ => "en",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn guests_and_nights_read_naturally() {
        assert_eq!(guests_label(2, 1, 0, "fr"), "2 adultes, 1 enfant");
        assert_eq!(guests_label(1, 2, 1, "en"), "1 adult, 2 children, 1 infant");
        assert_eq!(guests_label(2, 1, 0, "ar"), "البالغون: 2، الأطفال: 1");
        assert_eq!(nights_label(2, "ar"), "ليلتان");
        assert_eq!(nights_label(1, "fr"), "1 nuit");
    }

    #[test]
    fn cancellation_rules_become_sentences() {
        let rules = json!([
            {"min_hours_before": 0, "refund_percent": 0},
            {"min_hours_before": 168, "refund_percent": 100},
            {"min_hours_before": 48, "refund_percent": 50}
        ]);
        assert_eq!(
            policy_lines(&rules, "en"),
            [
                "Cancellation at least 7 days before arrival: full refund",
                "Cancellation at least 2 days before arrival: 50% refund",
                "Cancellation less than 2 days before arrival: no refund",
            ]
        );
        assert_eq!(
            policy_lines(&rules, "fr")[0],
            "Annulation au moins 7 jours avant l'arrivée : remboursement intégral"
        );
        assert!(policy_lines(&Value::Null, "fr").is_empty());
    }
}
