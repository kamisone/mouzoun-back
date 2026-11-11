//! Email tools:
//!
//! ```text
//! cargo run --bin email -- preview <template> <locale> [out.html]   render with sample data
//! cargo run --bin email -- send-test <template> <locale> <to>        send the sample rendering
//! cargo run --bin email -- retry <event_id>                          replay a dead outbox event
//! cargo run --bin email -- list-dead                                 dead outbox events
//! cargo run --bin email -- validate                                  check every stored template
//! ```

use anyhow::{Context, bail};
use mouzoun_api::{
    app,
    config::{Config, EmailConfig},
    infra::db,
    services::email::{EmailSettings, Mailer, admin, render, samples, sender},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::install_crypto_provider();
    dotenvy::dotenv().ok();
    app::init_tracing(false);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, name: &str| {
        args.get(i)
            .cloned()
            .with_context(|| format!("missing <{name}>\n{USAGE}"))
    };

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let pool = db::connect(&database_url, 2).await?;
    let mailer = || -> anyhow::Result<Mailer> {
        // Full config when available (API env), else just what emails need.
        let settings_and_email = match Config::from_env() {
            Ok(config) => (EmailSettings::from_config(&config), config.email),
            Err(_) => {
                let email = EmailConfig::from_env()?;
                let settings = EmailSettings {
                    public_site_url: email.public_site_url.clone(),
                    media_base_url: std::env::var("GCS_PUBLIC_BASE_URL").unwrap_or_default(),
                    link_secret: std::env::var("JWT_SECRET").unwrap_or_default(),
                    reply_to: email.reply_to.clone(),
                };
                (settings, email)
            }
        };
        let (settings, email) = settings_and_email;
        Ok(Mailer::new(
            pool.clone(),
            sender::from_config(&email)?,
            settings,
        ))
    };

    match args.first().map(String::as_str) {
        Some("preview") => {
            let (code, locale) = (arg(1, "template")?, arg(2, "locale")?);
            let out = admin::render_preview(&mailer()?, &code, &locale, None).await?;
            println!("Subject: {}\n\n{}", out.subject, out.text);
            if let Some(path) = args.get(3) {
                std::fs::write(path, &out.html)?;
                println!("\nHTML written to {path}");
            }
        }
        Some("send-test") => {
            let (code, locale, to) = (arg(1, "template")?, arg(2, "locale")?, arg(3, "to")?);
            let id = admin::send_test(&mailer()?, &code, &locale, &to).await?;
            println!("sent ({})", id.0);
        }
        Some("retry") => {
            let id: i64 = arg(1, "event_id")?
                .parse()
                .context("event_id must be a number")?;
            if admin::retry_event(&pool, id).await? {
                println!("event {id} queued again");
            } else {
                bail!("event {id} is not dead (or does not exist)");
            }
        }
        Some("list-dead") => {
            let dead = admin::list_dead(&pool, 100).await?;
            if dead.is_empty() {
                println!("no dead events");
            }
            for e in dead {
                println!(
                    "{:>8}  {:<36}  {:<38}  attempts={}  failed_at={}\n          {}",
                    e.id,
                    e.event_type,
                    e.aggregate_id,
                    e.attempts,
                    e.failed_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
                    e.last_error.unwrap_or_default()
                );
            }
        }
        Some("validate") => {
            let rows = sqlx::query!(
                r#"SELECT t.code::text AS "code!", t.variables, tr.locale, tr.subject, tr.body_html, tr.body_text
                   FROM email_templates t JOIN email_template_translations tr ON tr.email_template_id = t.id
                   ORDER BY t.code, tr.locale"#
            )
            .fetch_all(&pool)
            .await?;
            let mut errors = 0;
            for r in &rows {
                let source = render::TemplateSource {
                    subject: &r.subject,
                    body_html: &r.body_html,
                    body_text: &r.body_text,
                    variables: &r.variables,
                };
                if let Err(e) = render::validate(&source) {
                    errors += 1;
                    println!("✗ {} [{}]: {e}", r.code, r.locale);
                }
            }
            let missing: Vec<&str> = samples::TEMPLATE_CODES
                .iter()
                .copied()
                .filter(|c| !rows.iter().any(|r| r.code == *c))
                .collect();
            println!(
                "{} translations checked, {errors} invalid, missing templates: {missing:?}",
                rows.len()
            );
            if errors > 0 || !missing.is_empty() {
                std::process::exit(1);
            }
        }
        _ => println!("{USAGE}"),
    }
    Ok(())
}

const USAGE: &str = "usage: email preview <template> <locale> [out.html] | send-test <template> <locale> <to> | retry <event_id> | list-dead | validate";
