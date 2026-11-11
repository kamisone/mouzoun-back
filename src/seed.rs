use sqlx::PgPool;

use crate::{config::Config, services::identity};

/// Creates the first super admin from `ADMIN_EMAIL` / `ADMIN_PASSWORD` when none exists.
pub async fn ensure_admin(db: &PgPool, config: &Config) -> anyhow::Result<()> {
    let (Some(email), Some(pass)) = (&config.admin_email, &config.admin_password) else {
        return Ok(());
    };
    if identity::ensure_super_admin(db, email, pass, "Administrator").await? {
        tracing::info!(%email, "super admin created");
    }
    Ok(())
}
