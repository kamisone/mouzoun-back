//! Locale-aware formatting used inside email templates (`money`, `date`, `datetime`, `time`).
//!
//! Dates and times are shown in the camp's time zone (`crate::camp_time`). Arabic follows Moroccan usage (ar-MA): Western digits, Moroccan month names.
//! Unknown locales fall back to English conventions.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday};

/// No-break space and narrow no-break space (French thousands separator).
const NBSP: char = '\u{a0}';
const NNBSP: char = '\u{202f}';

/// `12000, "EUR"` → fr `120,00 €` · en `€120.00` · ar `120,00 €`.
pub fn money(amount_minor: i64, currency: &str, locale: &str) -> String {
    let negative = amount_minor < 0;
    let abs = amount_minor.unsigned_abs();
    let (units, cents) = (abs / 100, abs % 100);
    let (group, decimal) = match lang(locale) {
        "fr" => (NNBSP, ','),
        "ar" => ('.', ','),
        _ => (',', '.'),
    };
    let number = format!("{}{decimal}{cents:02}", group_digits(units, group));
    let sign = if negative { "-" } else { "" };
    let currency = currency.trim().to_ascii_uppercase();
    match lang(locale) {
        "fr" | "ar" => format!("{sign}{number}{NBSP}{}", symbol(&currency, locale)),
        _ => match currency.as_str() {
            "EUR" | "USD" | "GBP" => format!("{sign}{}{number}", symbol(&currency, locale)),
            _ => format!("{sign}{currency}{NBSP}{number}"),
        },
    }
}

fn symbol(currency: &str, locale: &str) -> String {
    match (currency, lang(locale)) {
        ("EUR", _) => "€".into(),
        ("USD", "fr") => "$US".into(),
        ("USD", _) => "$".into(),
        ("GBP", _) => "£".into(),
        ("MAD", "ar") => "د.م.".into(),
        ("MAD", _) => "MAD".into(),
        (other, _) => other.into(),
    }
}

fn group_digits(mut n: u64, sep: char) -> String {
    let mut parts = Vec::new();
    loop {
        if n < 1000 {
            parts.push(n.to_string());
            break;
        }
        parts.push(format!("{:03}", n % 1000));
        n /= 1000;
    }
    parts.reverse();
    parts.join(&sep.to_string())
}

/// `vendredi 15 janvier 2027` · `Friday 15 January 2027` · `الجمعة 15 يناير 2027`.
pub fn date(day: NaiveDate, locale: &str) -> String {
    let weekday = weekday_name(day.weekday(), locale);
    let month = month_name(day.month(), locale);
    format!("{weekday} {} {month} {}", day.day(), day.year())
}

/// `08:00` (24 h, Casablanca time).
pub fn time(at: DateTime<Utc>) -> String {
    let local = crate::camp_time::local(at);
    format!("{:02}:{:02}", local.hour(), local.minute())
}

/// `vendredi 15 janvier 2027 à 08:00` · `Friday 15 January 2027 at 08:00` · `... على الساعة 08:00`.
pub fn datetime(at: DateTime<Utc>, locale: &str) -> String {
    let day = crate::camp_time::today(at);
    let joiner = match lang(locale) {
        "fr" => "à",
        "ar" => "على الساعة",
        _ => "at",
    };
    format!("{} {joiner} {}", date(day, locale), time(at))
}

/// Parses what templates receive: `2027-01-15`, an RFC 3339 timestamp, or a naive
/// `2027-01-15T08:00:00` (taken as UTC).
pub enum Moment {
    Day(NaiveDate),
    Instant(DateTime<Utc>),
}

pub fn parse_moment(raw: &str) -> Option<Moment> {
    let raw = raw.trim();
    if let Ok(day) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        return Some(Moment::Day(day));
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Some(Moment::Instant(at.with_timezone(&Utc)));
    }
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|naive| Moment::Instant(naive.and_utc()))
}

fn lang(locale: &str) -> &str {
    let base = locale.split(['-', '_']).next().unwrap_or("");
    match base {
        "fr" => "fr",
        "ar" => "ar",
        _ => "en",
    }
}

fn weekday_name(day: Weekday, locale: &str) -> &'static str {
    let i = day.num_days_from_monday() as usize;
    match lang(locale) {
        "fr" => [
            "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche",
        ][i],
        "ar" => [
            "الاثنين",
            "الثلاثاء",
            "الأربعاء",
            "الخميس",
            "الجمعة",
            "السبت",
            "الأحد",
        ][i],
        _ => [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ][i],
    }
}

fn month_name(month: u32, locale: &str) -> &'static str {
    let i = (month as usize).saturating_sub(1).min(11);
    match lang(locale) {
        "fr" => [
            "janvier",
            "février",
            "mars",
            "avril",
            "mai",
            "juin",
            "juillet",
            "août",
            "septembre",
            "octobre",
            "novembre",
            "décembre",
        ][i],
        // Moroccan month names.
        "ar" => [
            "يناير",
            "فبراير",
            "مارس",
            "أبريل",
            "ماي",
            "يونيو",
            "يوليوز",
            "غشت",
            "شتنبر",
            "أكتوبر",
            "نونبر",
            "دجنبر",
        ][i],
        _ => [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ][i],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_follows_each_locale() {
        assert_eq!(money(12_000, "EUR", "fr"), "120,00\u{a0}€");
        assert_eq!(money(12_345_678, "EUR", "fr"), "123\u{202f}456,78\u{a0}€");
        assert_eq!(money(12_000, "EUR", "en"), "€120.00");
        assert_eq!(money(12_345_678, "EUR", "en"), "€123,456.78");
        assert_eq!(money(123_456, "EUR", "ar"), "1.234,56\u{a0}€");
        assert_eq!(money(5_000, "MAD", "ar"), "50,00\u{a0}د.م.");
        assert_eq!(money(5_000, "MAD", "en"), "MAD\u{a0}50.00");
        assert_eq!(money(-250, "EUR", "en"), "-€2.50");
        assert_eq!(money(5, "eur", "fr"), "0,05\u{a0}€");
    }

    #[test]
    fn dates_are_localized_and_in_casablanca_time() {
        let day = NaiveDate::from_ymd_opt(2027, 1, 15).unwrap();
        assert_eq!(date(day, "fr"), "vendredi 15 janvier 2027");
        assert_eq!(date(day, "en"), "Friday 15 January 2027");
        assert_eq!(date(day, "ar"), "الجمعة 15 يناير 2027");
        assert_eq!(
            date(NaiveDate::from_ymd_opt(2027, 8, 1).unwrap(), "ar"),
            "الأحد 1 غشت 2027"
        );

        // Morocco is UTC+0 since 20 September 2026 ...
        let winter = "2027-01-15T07:00:00Z".parse().unwrap();
        assert_eq!(datetime(winter, "fr"), "vendredi 15 janvier 2027 à 07:00");
        assert_eq!(datetime(winter, "en"), "Friday 15 January 2027 at 07:00");
        // ... it was UTC+1 before: late UTC evening was already the next day.
        let before = "2025-12-31T23:30:00Z".parse().unwrap();
        assert_eq!(datetime(before, "en"), "Thursday 1 January 2026 at 00:30");
    }

    #[test]
    fn moments_parse_dates_and_timestamps() {
        assert!(matches!(parse_moment("2027-01-15"), Some(Moment::Day(_))));
        assert!(matches!(
            parse_moment("2027-01-15T07:00:00Z"),
            Some(Moment::Instant(_))
        ));
        assert!(matches!(
            parse_moment("2027-01-15T07:00:00.123+01:00"),
            Some(Moment::Instant(_))
        ));
        assert!(parse_moment("next friday").is_none());
    }
}
