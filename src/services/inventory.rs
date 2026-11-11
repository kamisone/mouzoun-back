//! Inventory generation (invariants 5 and 6).
//!
//! - `generate_inventory_calendar`: creates the missing `inventory_calendar` rows up to 18 months
//!   ahead, with capacity = active units. Pool-mode types count their units too: the schema has
//!   no pool-size column, so a pool of 20 pitches is 20 `accommodation_units` that are simply
//!   never allocated individually. Existing rows are never touched (`ON CONFLICT DO NOTHING`),
//!   so manual overrides (capacity, stop-sell, restrictions) and reservations are preserved.
//! - `generate_activity_slots`: builds `activity_slots` from active `activity_schedules`, in
//!   Africa/Casablanca local time. Idempotent through `UNIQUE (product_id, starts_at)`.

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::{Africa::Casablanca, Tz};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::types::{DateRange, DaysOfWeek};

pub const CALENDAR_HORIZON_MONTHS: i32 = 18;

/// Returns the number of rows created.
pub async fn generate_inventory_calendar(
    pool: &PgPool,
    product_id: Option<Uuid>,
    from: NaiveDate,
    months: i32,
) -> sqlx::Result<u64> {
    let result = sqlx::query!(
        "INSERT INTO inventory_calendar (product_id, day, capacity)
         SELECT a.product_id, day::date,
                (SELECT count(*) FROM accommodation_units u
                 WHERE u.product_id = a.product_id AND u.status = 'active')::int
         FROM accommodation_types a
         JOIN products p ON p.id = a.product_id AND p.deleted_at IS NULL
         CROSS JOIN generate_series($2::date, ($2::date + make_interval(months => $3))::date - 1, interval '1 day') AS day
         WHERE $1::uuid IS NULL OR a.product_id = $1
         ON CONFLICT (product_id, day) DO NOTHING",
        product_id,
        from,
        months
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Creates the slots of `[from, from + days)`. Returns the number of slots created.
pub async fn generate_activity_slots(
    pool: &PgPool,
    from: NaiveDate,
    days: i64,
) -> sqlx::Result<u64> {
    let schedules = sqlx::query!(
        r#"SELECT s.id, s.product_id, s.days_of_week, s.start_time, s.capacity,
                  s.valid_period AS "valid_period: DateRange", a.duration_minutes
           FROM activity_schedules s
           JOIN activities a ON a.product_id = s.product_id
           JOIN products p ON p.id = s.product_id AND p.deleted_at IS NULL
           WHERE s.is_active"#
    )
    .fetch_all(pool)
    .await?;

    let (mut ids, mut products, mut schedule_ids, mut starts, mut ends, mut capacities) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    for schedule in &schedules {
        let weekdays = DaysOfWeek(schedule.days_of_week);
        let duration = Duration::minutes(i64::from(schedule.duration_minutes.unwrap_or(60)));
        for date in from
            .iter_days()
            .take(usize::try_from(days).unwrap_or_default())
        {
            if !weekdays.contains(date) || !schedule.valid_period.contains(date) {
                continue;
            }
            let Some(starts_at) = local_to_utc(&Casablanca, date.and_time(schedule.start_time))
            else {
                continue;
            };
            ids.push(Uuid::now_v7());
            products.push(schedule.product_id);
            schedule_ids.push(schedule.id);
            starts.push(starts_at);
            ends.push(starts_at + duration);
            capacities.push(schedule.capacity);
        }
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query!(
        "INSERT INTO activity_slots (id, product_id, schedule_id, starts_at, ends_at, capacity)
         SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::timestamptz[], $5::timestamptz[], $6::int4[])
         ON CONFLICT (product_id, starts_at) DO NOTHING",
        &ids,
        &products,
        &schedule_ids,
        &starts,
        &ends,
        &capacities
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Local wall-clock time → instant. On a clock change, an ambiguous time takes the earlier
/// instant and a skipped time (gap) is moved forward by one hour.
pub fn local_to_utc(tz: &Tz, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) => Some(t.with_timezone(&Utc)),
        LocalResult::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        LocalResult::None => tz
            .from_local_datetime(&(local + Duration::hours(1)))
            .earliest()
            .map(|t| t.with_timezone(&Utc)),
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveTime;

    use super::*;

    #[test]
    fn casablanca_local_times_follow_the_ramadan_clock_change() {
        // Morocco is UTC+1 most of the year and UTC+0 during Ramadan (2027: ~7 Feb – 14 Mar).
        let nine = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let summer = local_to_utc(
            &Casablanca,
            NaiveDate::from_ymd_opt(2027, 6, 1).unwrap().and_time(nine),
        )
        .unwrap();
        assert_eq!(summer.format("%H:%M").to_string(), "08:00");
        let ramadan = local_to_utc(
            &Casablanca,
            NaiveDate::from_ymd_opt(2027, 2, 20).unwrap().and_time(nine),
        )
        .unwrap();
        assert_eq!(ramadan.format("%H:%M").to_string(), "09:00");
    }
}
