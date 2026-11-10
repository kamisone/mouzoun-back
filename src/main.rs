use mouzoun_api::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app::install_crypto_provider();
    dotenvy::dotenv().ok();
    let config = Config::from_env()?;
    app::init_tracing(config.log_json);
    if let Err(e) = app::run(config).await {
        tracing::error!(error = ?e, "fatal error");
        return Err(e);
    }
    Ok(())
}
