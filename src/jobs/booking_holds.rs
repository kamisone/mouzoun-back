//! Expires pending bookings whose hold ran out, releasing their inventory (invariant 3).

use chrono::Utc;
use sqlx::PgPool;

use crate::services::booking;

pub async fn run_once(db: PgPool) -> anyhow::Result<()> {
    let expired = booking::expire_holds(&db, Utc::now(), 200).await?;
    if expired > 0 {
        tracing::info!(expired, "booking holds expired");
    }
    Ok(())
}
