//! Migration maintenance.
//!
//! ```text
//! cargo run --bin migrations -- status               applied vs. shipped migrations, checksum mismatches
//! cargo run --bin migrations -- accept-corrections   one-off repair, see below
//! ```
//!
//! `accept-corrections`: migrations 0006, 0008 and 0012 were once edited after being applied;
//! the edits now live in 0020 and the three files are back to what production applied. A
//! database built from the edited files has the corrected schema already but the old
//! checksums, so SQLx refuses to start. This command records the shipped checksums for those
//! three versions **only after checking the schema is the corrected one**, then the normal
//! migrations run. Refused when `APP_ENV=production` (production never had the edited files).

use anyhow::{Context, bail};
use mouzoun_api::app;
use sqlx::{PgPool, migrate::Migrator};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");
/// The migrations edited after being applied (see the module docs).
const CORRECTED_VERSIONS: [i64; 3] = [6, 8, 12];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::install_crypto_provider();
    dotenvy::dotenv().ok();
    let command = std::env::args().nth(1).unwrap_or_default();
    let url = std::env::var("DATABASE_URL").context("missing DATABASE_URL")?;
    let pool = PgPool::connect(&url).await?;
    match command.as_str() {
        "status" => status(&pool).await,
        "accept-corrections" => accept_corrections(&pool).await,
        _ => bail!("usage: migrations <status|accept-corrections>"),
    }
}

async fn applied(pool: &PgPool) -> anyhow::Result<Vec<(i64, Vec<u8>)>> {
    Ok(sqlx::query_as::<_, (i64, Vec<u8>)>(
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await?)
}

async fn status(pool: &PgPool) -> anyhow::Result<()> {
    let applied = applied(pool).await?;
    for m in MIGRATOR
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
    {
        let state = match applied.iter().find(|(v, _)| *v == m.version) {
            None => "pending",
            Some((_, sum)) if sum.as_slice() == m.checksum.as_ref() => "applied",
            Some(_) => "CHECKSUM MISMATCH",
        };
        println!("{:04} {:<40} {state}", m.version, m.description);
    }
    Ok(())
}

async fn accept_corrections(pool: &PgPool) -> anyhow::Result<()> {
    if std::env::var("APP_ENV").as_deref() == Ok("production") {
        bail!(
            "refused with APP_ENV=production: production applied the original files, nothing to repair"
        );
    }
    let corrected: bool = sqlx::query_scalar(
        "SELECT
           EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema()
                   AND table_name = 'rate_plans' AND column_name = 'deposit_amount_minor')
           AND NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema()
                   AND table_name = 'rate_plans' AND column_name = 'deposit_value')
           AND EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema()
                   AND table_name = 'promotions' AND column_name = 'discount_free_nights')
           AND NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_schema = current_schema()
                   AND table_name = 'promotions' AND column_name = 'discount_value')
           AND to_regclass('booking_reference_counters') IS NOT NULL
           AND to_regclass('booking_reference_seq') IS NULL",
    )
    .fetch_one(pool)
    .await?;
    if !corrected {
        bail!("this database does not have the corrected schema: nothing changed (run `status`)");
    }
    let applied = applied(pool).await?;
    let mut tx = pool.begin().await?;
    for version in CORRECTED_VERSIONS {
        let shipped = MIGRATOR
            .iter()
            .find(|m| m.version == version && m.migration_type.is_up_migration())
            .with_context(|| format!("migration {version} is not shipped"))?;
        match applied.iter().find(|(v, _)| *v == version) {
            Some((_, sum)) if sum.as_slice() != shipped.checksum.as_ref() => {
                sqlx::query("UPDATE _sqlx_migrations SET checksum = $2 WHERE version = $1")
                    .bind(version)
                    .bind(shipped.checksum.as_ref())
                    .execute(&mut *tx)
                    .await?;
                println!("{version:04}: checksum updated");
            }
            Some(_) => println!("{version:04}: already up to date"),
            None => println!("{version:04}: not applied yet, nothing to do"),
        }
    }
    tx.commit().await?;
    println!("done: start the API (or `sqlx migrate run`) to apply the pending migrations");
    Ok(())
}
