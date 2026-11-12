//! Account housekeeping (every 15 minutes): data exports, due deletions, expired tokens and
//! sessions.

use chrono::Utc;
use sqlx::PgPool;

use crate::services::accounts::privacy;

pub async fn run_once(db: PgPool) -> anyhow::Result<()> {
    let now = Utc::now();
    privacy::build_pending_exports(&db, now).await?;
    privacy::run_due_deletions(&db, now).await?;
    privacy::cleanup(&db, now).await?;
    Ok(())
}
