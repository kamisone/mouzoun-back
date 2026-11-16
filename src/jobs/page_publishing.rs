//! Scheduled publishing: promotes `scheduled` page revisions whose `publish_at` has passed.

use sqlx::PgPool;

use crate::{infra::clock::Clock, modules::pages::service};

pub async fn run_once(db: PgPool, clock: Clock) -> anyhow::Result<()> {
    let promoted = service::promote_due(&db, clock.now()).await?;
    if promoted > 0 {
        tracing::info!(promoted, "scheduled page revisions published");
    }
    Ok(())
}
