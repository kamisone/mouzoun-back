//! The camp's local time: Africa/Casablanca.
//!
//! Morocco changes its clock by decree (UTC+1 with a Ramadan switch to UTC+0 from 2018, then
//! permanent UTC+0 from 20 September 2026, 02:00 — tzdata 2026c). Every local ↔ UTC conversion
//! in the application goes through this module, which uses the IANA database bundled in the
//! binary (`jiff` / `jiff-tzdb`): results never depend on the OS or Postgres tz data, and a
//! future change only needs a dependency update (`cargo update -p jiff-tzdb`).
//!
//! SQL must not convert time zones (`AT TIME ZONE`): a database image with older tz data would
//! silently disagree with the application.

use std::sync::LazyLock;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use jiff::{Timestamp, civil, tz::TimeZone};

pub const ZONE: &str = "Africa/Casablanca";

static TZ: LazyLock<TimeZone> =
    LazyLock::new(|| TimeZone::get(ZONE).expect("the bundled tz database has Africa/Casablanca"));

/// Wall-clock time at the camp.
pub fn local(at: DateTime<Utc>) -> NaiveDateTime {
    let ts = Timestamp::new(at.timestamp(), at.timestamp_subsec_nanos() as i32)
        .expect("chrono timestamps are within jiff's range");
    let dt = ts.to_zoned(TZ.clone()).datetime();
    NaiveDate::from_ymd_opt(i32::from(dt.year()), dt.month() as u32, dt.day() as u32)
        .and_then(|d| {
            d.and_hms_nano_opt(
                dt.hour() as u32,
                dt.minute() as u32,
                dt.second() as u32,
                dt.subsec_nanosecond() as u32,
            )
        })
        .expect("jiff civil datetimes are valid chrono datetimes")
}

/// The camp's calendar date at `at`.
pub fn today(at: DateTime<Utc>) -> NaiveDate {
    local(at).date()
}

/// Camp wall-clock time → instant. Around a clock change, an ambiguous time takes the earlier
/// instant and a skipped time is moved forward by the gap.
pub fn to_utc(local: NaiveDateTime) -> DateTime<Utc> {
    use chrono::{Datelike, Timelike};
    let civil = civil::DateTime::new(
        local.year() as i16,
        local.month() as i8,
        local.day() as i8,
        local.hour() as i8,
        local.minute() as i8,
        local.second() as i8,
        local.nanosecond().min(999_999_999) as i32,
    )
    .expect("chrono datetimes are valid civil datetimes");
    let ts = TZ
        .to_ambiguous_timestamp(civil)
        .compatible()
        .expect("every Casablanca wall-clock time resolves");
    DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
        .expect("jiff timestamps are within chrono's range")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn morocco_is_utc_plus_0_since_20_september_2026() {
        // Before the decree: UTC+1, and UTC+0 during Ramadan 2026.
        assert_eq!(
            local(at("2026-06-01T08:00:00Z"))
                .format("%H:%M")
                .to_string(),
            "09:00"
        );
        assert_eq!(
            local(at("2026-03-01T08:00:00Z"))
                .format("%H:%M")
                .to_string(),
            "08:00"
        );
        // 20 Sept 2026 02:00 local: clocks back to 01:00, then UTC+0 all year, Ramadan included.
        assert_eq!(
            local(at("2026-09-19T12:00:00Z"))
                .format("%H:%M")
                .to_string(),
            "13:00"
        );
        assert_eq!(
            local(at("2026-09-21T12:00:00Z"))
                .format("%H:%M")
                .to_string(),
            "12:00"
        );
        for winter_or_ramadan in [
            "2027-01-07T09:00:00Z",
            "2027-02-20T09:00:00Z",
            "2027-07-01T09:00:00Z",
        ] {
            assert_eq!(
                local(at(winter_or_ramadan)).format("%H:%M").to_string(),
                "09:00"
            );
        }
    }

    #[test]
    fn local_times_round_trip_and_the_year_boundary_is_moroccan() {
        let nine = NaiveDate::from_ymd_opt(2027, 1, 7)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        assert_eq!(to_utc(nine), at("2027-01-07T09:00:00Z"));
        let summer_2026 = NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap();
        assert_eq!(to_utc(summer_2026), at("2026-06-01T08:00:00Z"));
        // 01:30 on 20 Sept 2026 happened twice: the earlier instant (still UTC+1) is used.
        let repeated = NaiveDate::from_ymd_opt(2026, 9, 20)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        assert_eq!(to_utc(repeated), at("2026-09-20T00:30:00Z"));
        assert_eq!(
            today(at("2025-12-31T23:30:00Z")),
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
        );
        assert_eq!(
            today(at("2027-12-31T23:30:00Z")),
            NaiveDate::from_ymd_opt(2027, 12, 31).unwrap()
        );
    }
}
