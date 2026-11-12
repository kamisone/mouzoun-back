//! Application configuration, loaded once from environment variables at startup.
//! Every value is validated here so misconfiguration fails fast with a clear message.

use std::{fmt, time::Duration};

use anyhow::{Context, Result, bail};

#[derive(Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub database_max_connections: u32,
    /// Apply pending migrations at startup (`DATABASE_RUN_MIGRATIONS`, default true).
    pub run_migrations: bool,
    pub redis_url: String,
    pub jwt_secret: String,
    pub jwt_ttl_minutes: i64,
    /// `Secure` session cookie (HTTPS only). Disable only for plain-HTTP local development.
    pub cookie_secure: bool,
    /// Browser origins allowed to call the API (CORS) and to send state-changing requests (CSRF).
    pub cors_origins: Vec<String>,
    pub log_json: bool,
    pub admin_email: Option<String>,
    pub admin_password: Option<String>,
    pub storage: StorageConfig,
    pub media: MediaConfig,
    pub email: EmailConfig,
    pub worker: WorkerConfig,
    pub booking: BookingConfig,
}

/// Payments, bot protection and guest-data encryption (docs/booking/README.md).
#[derive(Clone, Default)]
pub struct BookingConfig {
    pub stripe: Option<StripeConfig>,
    pub paypal: Option<PaypalConfig>,
    /// `PAYMENTS_FAKE=true`: a provider that "pays" without any card, for local development and
    /// end-to-end tests. Never enable it on a public server.
    pub fake_payments: bool,
    /// `TURNSTILE_SECRET_KEY`: Cloudflare Turnstile check on checkout and "find my booking".
    pub turnstile_secret: Option<String>,
    /// `GUEST_DATA_KEY` (base64, 32 bytes): AES-256-GCM key of guest ID document numbers.
    pub guest_data_key: Option<[u8; 32]>,
    /// Trust `X-Forwarded-For` (set by our nginx) for the client IP (`TRUST_PROXY`, default true).
    pub trust_proxy: bool,
    /// Check new passwords against Have I Been Pwned (`HIBP_ENABLED`, default true).
    pub hibp_enabled: bool,
    /// Social sign-in (OpenID Connect).
    pub google: Option<OidcClient>,
    pub apple: Option<AppleClient>,
    /// Public URL of the API as seen by browsers (OIDC redirect URIs), e.g. https://mouzoun.com/api.
    pub api_public_url: String,
    /// WebAuthn relying party: id (domain) and origin (`WEBAUTHN_RP_ID`, `WEBAUTHN_ORIGIN`).
    pub webauthn_rp_id: String,
    pub webauthn_origin: String,
}

#[derive(Clone)]
pub struct OidcClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone)]
pub struct AppleClient {
    /// Services ID (e.g. com.mouzoun.web).
    pub client_id: String,
    pub team_id: String,
    pub key_id: String,
    /// ES256 private key (.p8, PEM).
    pub private_key: String,
}

#[derive(Clone)]
pub struct StripeConfig {
    pub secret_key: String,
    pub publishable_key: String,
    pub webhook_secret: String,
    /// `STRIPE_API_BASE` (tests point it at a mock), default https://api.stripe.com.
    pub api_base: String,
}

#[derive(Clone)]
pub struct PaypalConfig {
    pub client_id: String,
    pub client_secret: String,
    pub webhook_id: String,
    /// https://api-m.sandbox.paypal.com or https://api-m.paypal.com (`PAYPAL_MODE=sandbox|live`).
    pub api_base: String,
}

impl fmt::Debug for BookingConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BookingConfig")
            .field("stripe", &self.stripe.as_ref().map(|s| &s.api_base))
            .field("paypal", &self.paypal.as_ref().map(|p| &p.api_base))
            .field("fake_payments", &self.fake_payments)
            .field("turnstile", &self.turnstile_secret.is_some())
            .field("guest_data_key", &self.guest_data_key.is_some())
            .field("google", &self.google.is_some())
            .field("apple", &self.apple.is_some())
            .field("webauthn_rp_id", &self.webauthn_rp_id)
            .finish()
    }
}

impl BookingConfig {
    pub fn from_env() -> Result<Self> {
        let stripe = match optional("STRIPE_SECRET_KEY") {
            Some(secret_key) => Some(StripeConfig {
                secret_key,
                publishable_key: required("STRIPE_PUBLISHABLE_KEY")?,
                webhook_secret: required("STRIPE_WEBHOOK_SECRET")?,
                api_base: optional("STRIPE_API_BASE")
                    .unwrap_or_else(|| "https://api.stripe.com".into())
                    .trim_end_matches('/')
                    .to_string(),
            }),
            None => None,
        };
        let paypal = match optional("PAYPAL_CLIENT_ID") {
            Some(client_id) => Some(PaypalConfig {
                client_id,
                client_secret: required("PAYPAL_CLIENT_SECRET")?,
                webhook_id: required("PAYPAL_WEBHOOK_ID")?,
                api_base: match optional("PAYPAL_MODE").as_deref() {
                    None | Some("sandbox") => "https://api-m.sandbox.paypal.com".into(),
                    Some("live") => "https://api-m.paypal.com".into(),
                    Some(other) => bail!("PAYPAL_MODE must be sandbox or live, got {other:?}"),
                },
            }),
            None => None,
        };
        let guest_data_key = match optional("GUEST_DATA_KEY") {
            Some(raw) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(raw.trim())
                    .context("GUEST_DATA_KEY must be base64")?;
                Some(
                    <[u8; 32]>::try_from(bytes.as_slice())
                        .map_err(|_| anyhow::anyhow!("GUEST_DATA_KEY must decode to 32 bytes"))?,
                )
            }
            None => None,
        };
        Ok(Self {
            stripe,
            paypal,
            fake_payments: parse_or("PAYMENTS_FAKE", false)?,
            turnstile_secret: optional("TURNSTILE_SECRET_KEY"),
            guest_data_key,
            trust_proxy: parse_or("TRUST_PROXY", true)?,
            hibp_enabled: parse_or("HIBP_ENABLED", true)?,
            google: match optional("GOOGLE_CLIENT_ID") {
                Some(client_id) => Some(OidcClient {
                    client_id,
                    client_secret: required("GOOGLE_CLIENT_SECRET")?,
                }),
                None => None,
            },
            apple: match optional("APPLE_CLIENT_ID") {
                Some(client_id) => Some(AppleClient {
                    client_id,
                    team_id: required("APPLE_TEAM_ID")?,
                    key_id: required("APPLE_KEY_ID")?,
                    private_key: required("APPLE_PRIVATE_KEY")?.replace("\\n", "\n"),
                }),
                None => None,
            },
            api_public_url: optional("API_PUBLIC_URL")
                .unwrap_or_else(|| "http://localhost:8080".into())
                .trim_end_matches('/')
                .to_string(),
            webauthn_rp_id: optional("WEBAUTHN_RP_ID").unwrap_or_else(|| "localhost".into()),
            webauthn_origin: optional("WEBAUTHN_ORIGIN")
                .unwrap_or_else(|| "http://localhost:3001".into()),
        })
    }
}

/// Transactional emails.
#[derive(Clone)]
pub struct EmailConfig {
    /// `EMAIL_PROVIDER=smtp|log` (default `log`: emails are printed, not sent).
    pub provider: EmailProvider,
    /// `EMAIL_FROM` / `EMAIL_FROM_NAME` / `EMAIL_REPLY_TO`.
    pub from_email: String,
    pub from_name: String,
    pub reply_to: Option<String>,
    /// Set when `SMTP_HOST` is.
    pub smtp: Option<SmtpConfig>,
    /// Public website, for links in emails (`PUBLIC_SITE_URL`, e.g. https://mouzoun.com).
    pub public_site_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailProvider {
    Log,
    Smtp,
}

#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tls: SmtpTls,
}

/// `SMTP_TLS=starttls` (port 587, default), `tls` (implicit TLS, port 465) or `none`
/// (local Mailpit on port 1025 only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpTls {
    StartTls,
    Implicit,
    None,
}

impl fmt::Debug for EmailConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmailConfig")
            .field("provider", &self.provider)
            .field("from_email", &self.from_email)
            .field("smtp_host", &self.smtp.as_ref().map(|s| &s.host))
            .field("smtp_port", &self.smtp.as_ref().map(|s| s.port))
            .field("smtp_tls", &self.smtp.as_ref().map(|s| s.tls))
            .field("public_site_url", &self.public_site_url)
            .finish_non_exhaustive()
    }
}

/// Outbox dispatcher.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Run the dispatcher inside the API process (`OUTBOX_WORKER_ENABLED`, default true). Set
    /// false when running `cargo run --bin worker` separately.
    pub in_api: bool,
    /// Attempts before an event is marked dead (`OUTBOX_MAX_ATTEMPTS`, default 8).
    pub max_attempts: i32,
    /// Events claimed per transaction (`OUTBOX_BATCH_SIZE`, default 20).
    pub batch_size: i64,
    /// Fallback polling when no NOTIFY arrives (`OUTBOX_POLL_SECS`, default 5).
    pub poll_interval: Duration,
    /// How often scheduled emails (pre-arrival, review request) are planned (default 300 s).
    pub scheduler_interval: Duration,
}

/// Google Cloud Storage settings.
#[derive(Clone)]
pub struct StorageConfig {
    /// Bucket for publicly readable files (activity photos, ...). Must grant `allUsers` read.
    pub public_bucket: String,
    /// Optional bucket for private files (invoices, IDs, ...), only reachable via signed URLs.
    /// Private uploads are rejected while this is not configured.
    pub private_bucket: Option<String>,
    /// Base URL used to build public file URLs (e.g. a CDN domain in front of the bucket).
    pub public_base_url: String,
    pub credentials: GcsCredentials,
}

#[derive(Clone)]
pub enum GcsCredentials {
    /// Service-account key passed through env vars (VPS deployment).
    ServiceAccount {
        project_id: String,
        client_email: String,
        private_key_id: String,
        private_key: String,
    },
    /// Application Default Credentials (GCE, Cloud Run, `gcloud auth application-default login`).
    ApplicationDefault,
}

// Never print secrets, even in debug logs.
impl fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let creds = match &self.credentials {
            GcsCredentials::ServiceAccount { client_email, .. } => {
                format!("ServiceAccount({client_email})")
            }
            GcsCredentials::ApplicationDefault => "ApplicationDefault".into(),
        };
        f.debug_struct("StorageConfig")
            .field("public_bucket", &self.public_bucket)
            .field("private_bucket", &self.private_bucket)
            .field("public_base_url", &self.public_base_url)
            .field("credentials", &creds)
            .finish()
    }
}

/// Upload rules and lifetimes for the media module.
#[derive(Clone, Debug)]
pub struct MediaConfig {
    pub max_upload_bytes: i64,
    pub upload_url_ttl: Duration,
    pub download_url_ttl: Duration,
    /// Pending uploads older than this are considered abandoned and purged.
    pub pending_ttl: Duration,
    pub cleanup_interval: Duration,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let jwt_secret = required("JWT_SECRET")?;
        if jwt_secret.len() < 32 {
            bail!("JWT_SECRET must be at least 32 characters");
        }
        Ok(Self {
            host: optional("APP_HOST").unwrap_or_else(|| "0.0.0.0".into()),
            port: parse_or("APP_PORT", 8080)?,
            database_url: required("DATABASE_URL")?,
            database_max_connections: parse_or("DATABASE_MAX_CONNECTIONS", 20)?,
            run_migrations: parse_or("DATABASE_RUN_MIGRATIONS", true)?,
            redis_url: required("REDIS_URL")?,
            jwt_secret,
            jwt_ttl_minutes: parse_or("JWT_TTL_MINUTES", 60 * 12)?,
            cookie_secure: parse_or("COOKIE_SECURE", true)?,
            cors_origins: optional("CORS_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            log_json: parse_or("LOG_JSON", false)?,
            admin_email: optional("ADMIN_EMAIL"),
            admin_password: optional("ADMIN_PASSWORD"),
            storage: StorageConfig::from_env()?,
            media: MediaConfig::from_env()?,
            email: EmailConfig::from_env()?,
            worker: WorkerConfig::from_env()?,
            booking: BookingConfig::from_env()?,
        })
    }
}

impl StorageConfig {
    fn from_env() -> Result<Self> {
        let public_bucket = required("GCS_BUCKET_NAME")?;
        let public_base_url = optional("GCS_PUBLIC_BASE_URL")
            .unwrap_or_else(|| format!("https://storage.googleapis.com/{public_bucket}"))
            .trim_end_matches('/')
            .to_string();

        let credentials = match (optional("GCS_CLIENT_EMAIL"), optional("GCS_PRIVATE_KEY")) {
            (Some(client_email), Some(raw_key)) => GcsCredentials::ServiceAccount {
                project_id: required("GCS_PROJECT_ID")?,
                client_email,
                private_key_id: required("GCS_PRIVATE_KEY_ID")?,
                private_key: normalize_private_key(&raw_key)?,
            },
            (None, None) => GcsCredentials::ApplicationDefault,
            _ => bail!("GCS_CLIENT_EMAIL and GCS_PRIVATE_KEY must be set together"),
        };

        Ok(Self {
            public_bucket,
            private_bucket: optional("GCS_PRIVATE_BUCKET_NAME"),
            public_base_url,
            credentials,
        })
    }
}

impl MediaConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            max_upload_bytes: parse_or("MEDIA_MAX_UPLOAD_BYTES", 15 * 1024 * 1024)?,
            upload_url_ttl: Duration::from_secs(parse_or("MEDIA_UPLOAD_URL_TTL_SECS", 15 * 60)?),
            download_url_ttl: Duration::from_secs(parse_or(
                "MEDIA_DOWNLOAD_URL_TTL_SECS",
                15 * 60,
            )?),
            pending_ttl: Duration::from_secs(parse_or("MEDIA_PENDING_TTL_SECS", 24 * 3600)?),
            cleanup_interval: Duration::from_secs(parse_or("MEDIA_CLEANUP_INTERVAL_SECS", 3600)?),
        })
    }
}

impl EmailConfig {
    pub fn from_env() -> Result<Self> {
        let provider = match optional("EMAIL_PROVIDER").as_deref() {
            None | Some("log") => EmailProvider::Log,
            Some("smtp") => EmailProvider::Smtp,
            Some(other) => bail!("EMAIL_PROVIDER must be smtp or log, got {other:?}"),
        };
        let smtp = match optional("SMTP_HOST") {
            Some(host) => {
                let tls = match optional("SMTP_TLS").as_deref() {
                    None | Some("starttls") => SmtpTls::StartTls,
                    Some("tls") => SmtpTls::Implicit,
                    Some("none") => SmtpTls::None,
                    Some(other) => bail!("SMTP_TLS must be starttls, tls or none, got {other:?}"),
                };
                let default_port = match tls {
                    SmtpTls::StartTls => 587,
                    SmtpTls::Implicit => 465,
                    SmtpTls::None => 1025,
                };
                Some(SmtpConfig {
                    host,
                    port: parse_or("SMTP_PORT", default_port)?,
                    user: optional("SMTP_USER"),
                    password: optional("SMTP_PASSWORD"),
                    tls,
                })
            }
            None => None,
        };
        if provider == EmailProvider::Smtp && smtp.is_none() {
            bail!("EMAIL_PROVIDER=smtp requires SMTP_HOST");
        }
        let from_email = match (provider, optional("EMAIL_FROM")) {
            (_, Some(from)) => from,
            (EmailProvider::Log, None) => "no-reply@localhost".into(),
            (EmailProvider::Smtp, None) => bail!("EMAIL_PROVIDER=smtp requires EMAIL_FROM"),
        };
        if !from_email.contains('@') {
            bail!("EMAIL_FROM must be an email address");
        }
        Ok(Self {
            provider,
            from_email,
            from_name: optional("EMAIL_FROM_NAME").unwrap_or_else(|| "Mouzoun".into()),
            reply_to: optional("EMAIL_REPLY_TO"),
            smtp,
            public_site_url: optional("PUBLIC_SITE_URL")
                .unwrap_or_else(|| "http://localhost:3001".into())
                .trim_end_matches('/')
                .to_string(),
        })
    }
}

impl WorkerConfig {
    pub fn from_env() -> Result<Self> {
        let max_attempts: i32 = parse_or("OUTBOX_MAX_ATTEMPTS", 8)?;
        if max_attempts < 1 {
            bail!("OUTBOX_MAX_ATTEMPTS must be at least 1");
        }
        Ok(Self {
            in_api: parse_or("OUTBOX_WORKER_ENABLED", true)?,
            max_attempts,
            batch_size: parse_or("OUTBOX_BATCH_SIZE", 20)?,
            poll_interval: Duration::from_secs(parse_or("OUTBOX_POLL_SECS", 5)?),
            scheduler_interval: Duration::from_secs(parse_or("OUTBOX_SCHEDULER_SECS", 300)?),
        })
    }
}

/// Keys pasted into .env usually carry literal `\n` sequences; PEM needs real newlines.
pub(crate) fn normalize_private_key(raw: &str) -> Result<String> {
    let key = raw.trim().trim_matches('"').replace("\\n", "\n");
    if !key.starts_with("-----BEGIN") || !key.trim_end().ends_with("-----") {
        bail!("GCS_PRIVATE_KEY is not a PEM private key (expected -----BEGIN PRIVATE KEY-----...)");
    }
    Ok(key)
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn required(key: &str) -> Result<String> {
    optional(key).with_context(|| format!("missing required env var {key}"))
}

fn parse_or<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: fmt::Display,
{
    match optional(key) {
        Some(v) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid value for {key}: {e}")),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_private_key;

    #[test]
    fn private_key_escaped_newlines_are_restored() {
        let key = normalize_private_key(
            "\"-----BEGIN PRIVATE KEY-----\\nABC\\n-----END PRIVATE KEY-----\\n\"",
        )
        .unwrap();
        assert_eq!(
            key,
            "-----BEGIN PRIVATE KEY-----\nABC\n-----END PRIVATE KEY-----\n"
        );
    }

    #[test]
    fn non_pem_private_key_is_rejected() {
        assert!(normalize_private_key("XXXXX").is_err());
    }
}
