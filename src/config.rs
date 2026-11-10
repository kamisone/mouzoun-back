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
    pub redis_url: String,
    pub jwt_secret: String,
    pub jwt_ttl_minutes: i64,
    pub cors_origins: Vec<String>,
    pub log_json: bool,
    pub admin_email: Option<String>,
    pub admin_password: Option<String>,
    pub storage: StorageConfig,
    pub media: MediaConfig,
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
            redis_url: required("REDIS_URL")?,
            jwt_secret,
            jwt_ttl_minutes: parse_or("JWT_TTL_MINUTES", 60 * 12)?,
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
