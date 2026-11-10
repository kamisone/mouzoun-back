use sqlx::PgPool;

use crate::{auth::password, config::Config};

/// Creates the first admin from ADMIN_EMAIL / ADMIN_PASSWORD if no admin exists yet.
pub async fn ensure_admin(db: &PgPool, config: &Config) -> anyhow::Result<()> {
    let (Some(email), Some(pass)) = (&config.admin_email, &config.admin_password) else {
        return Ok(());
    };

    if pass.chars().count() < 12 {
        tracing::warn!("ADMIN_PASSWORD is shorter than 12 characters: use a stronger password");
    }

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE role = 'admin')")
            .fetch_one(db)
            .await?;
    if exists {
        return Ok(());
    }

    let hash = password::hash(pass.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    sqlx::query(
        "INSERT INTO users (email, password_hash, full_name, role) VALUES ($1, $2, 'Administrator', 'admin')",
    )
    .bind(email.trim().to_lowercase())
    .bind(hash)
    .execute(db)
    .await?;
    tracing::info!(%email, "initial admin user created");
    Ok(())
}
