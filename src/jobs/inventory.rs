//! Daily inventory generation: night rows 18 months ahead, activity slots as far ahead as
//! guests may book (`booking.max_days_ahead`). Both are idempotent.

use chrono::Utc;
use chrono_tz::Africa::Casablanca;
use sqlx::PgPool;

use crate::services::{inventory, settings};

pub async fn run_once(db: PgPool) -> anyhow::Result<()> {
    let today = Utc::now().with_timezone(&Casablanca).date_naive();
    let nights = inventory::generate_inventory_calendar(
        &db,
        None,
        today,
        inventory::CALENDAR_HORIZON_MONTHS,
    )
    .await?;
    let days_ahead = settings::number(&db, "booking.max_days_ahead", 365).await?;
    let slots = inventory::generate_activity_slots(&db, today, days_ahead).await?;
    tracing::info!(nights, slots, "inventory generated");
    Ok(())
}
