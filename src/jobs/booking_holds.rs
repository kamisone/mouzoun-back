//! Expires pending bookings whose hold ran out, releasing their inventory (invariant 3), and
//! cancels their unpaid payments at the provider.

use chrono::Utc;
use sqlx::PgPool;

use crate::services::{
    booking,
    payments::{self, Payments},
};

pub async fn run_once(db: PgPool, payments: Payments) -> anyhow::Result<()> {
    let expired = booking::expire_holds(&db, Utc::now(), 200).await?;
    if expired > 0 {
        tracing::info!(expired, "booking holds expired");
    }
    // Their unpaid payment intents are closed at the provider.
    let cancelled = payments::cancel_abandoned(&db, &payments, 100).await?;
    if cancelled > 0 {
        tracing::info!(cancelled, "abandoned payments cancelled");
    }
    Ok(())
}
