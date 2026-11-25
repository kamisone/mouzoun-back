//! Operations before the shop opens. Idempotent: run them again safely.
//!
//! ```text
//! cargo run --bin mouzoun-cli -- legal-pages install [--force] [--as <staff email>]
//!     creates and publishes Mouzoun's six legal pages from legal/pages/*.html (left alone when
//!     already set up, unless --force) and points the legal.* settings at them.
//! cargo run --bin mouzoun-cli -- legal-pages remove-notes [--as <staff email>]
//!     removes the reviewer note paragraphs of the earlier templates from the legal pages and
//!     republishes them (author: system). Idempotent.
//! cargo run --bin mouzoun-cli -- shipping seed-placeholders
//!     placeholder rates for mainland France (never offered until an admin confirms them).
//! ```

use anyhow::{Context, bail};
use mouzoun_api::{app, modules::pages::registry::SectionRegistry, services::shop::launch};
use sqlx::PgPool;

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

const USAGE: &str = "usage: mouzoun-cli legal-pages install [--force] [--as <staff email>]
       mouzoun-cli legal-pages remove-notes [--as <staff email>]
       mouzoun-cli shipping seed-placeholders";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::install_crypto_provider();
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = (
        args.first().map(String::as_str).unwrap_or_default(),
        args.get(1).map(String::as_str).unwrap_or_default(),
    );
    let url = std::env::var("DATABASE_URL").context("missing DATABASE_URL")?;
    let pool = PgPool::connect(&url).await?;
    match command {
        ("legal-pages", "install") => {
            let reg = SectionRegistry::load();
            reg.sync(&pool).await?;
            let staff = launch::staff_for(&pool, option(&args, "--as")).await?;
            for (key, outcome) in
                launch::install_legal_pages(&pool, &reg, &staff, flag(&args, "--force")).await?
            {
                let label = match outcome {
                    launch::PageOutcome::Created => "created and published",
                    launch::PageOutcome::Replaced => "replaced by the template and republished",
                    launch::PageOutcome::AlreadyInstalled => {
                        "already installed (use --force to replace)"
                    }
                };
                println!("{key}: {label}");
            }
            Ok(())
        }
        ("legal-pages", "remove-notes") => {
            let reg = SectionRegistry::load();
            reg.sync(&pool).await?;
            let staff = launch::staff_for(&pool, option(&args, "--as")).await?;
            for (slug, outcome) in launch::remove_legal_page_notes(&pool, &reg, &staff).await? {
                let label = match outcome {
                    launch::CleanOutcome::Republished => "notes removed, republished",
                    launch::CleanOutcome::DraftOnly => {
                        "draft clean, but it has other unpublished changes: publish it in the builder to remove the notes from the live page"
                    }
                    launch::CleanOutcome::Clean => "nothing to remove",
                };
                println!("{slug}: {label}");
            }
            Ok(())
        }
        ("shipping", "seed-placeholders") => {
            let n = launch::seed_placeholder_shipping(&pool).await?;
            println!("placeholder rates created: {n}");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}
