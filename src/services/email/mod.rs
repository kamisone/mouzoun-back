//! Transactional emails: templates in fr/en/ar edited in the admin, rendered with minijinja,
//! sent through an [`EmailSender`], logged in `email_log` and triggered by outbox events
//! (see `docs/email/README.md`).

pub mod admin;
mod context;
pub mod delivery;
pub mod format;
pub mod handler;
pub mod labels;
mod links;
pub mod render;
pub mod samples;
pub mod sender;
pub mod templates;
pub mod views;

use std::sync::Arc;

use sqlx::PgPool;

pub use delivery::{Delivery, EmailJob, Source};
pub use render::{RenderError, Rendered};
pub use sender::{EmailSender, OutgoingEmail, ProviderMessageId, SendError};

use crate::config::Config;

/// Settings the email pipeline needs besides the database.
#[derive(Clone, Debug)]
pub struct EmailSettings {
    /// Public website (links), e.g. `https://mouzoun.com`.
    pub public_site_url: String,
    /// Public bucket base URL (logo).
    pub media_base_url: String,
    /// Signs the booking links (derived from `JWT_SECRET`).
    pub link_secret: String,
    pub reply_to: Option<String>,
}

impl EmailSettings {
    pub fn from_config(config: &Config) -> Self {
        Self {
            public_site_url: config.email.public_site_url.clone(),
            media_base_url: config.storage.public_base_url.clone(),
            link_secret: config.jwt_secret.clone(),
            reply_to: config.email.reply_to.clone(),
        }
    }
}

/// Everything needed to render and send an email. Cheap to clone.
#[derive(Clone)]
pub struct Mailer {
    pub(crate) pool: PgPool,
    pub(crate) sender: Arc<dyn EmailSender>,
    pub(crate) settings: Arc<EmailSettings>,
}

impl Mailer {
    pub fn new(pool: PgPool, sender: Arc<dyn EmailSender>, settings: EmailSettings) -> Self {
        Self {
            pool,
            sender,
            settings: Arc::new(settings),
        }
    }

    pub fn settings(&self) -> &EmailSettings {
        &self.settings
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    #[error(transparent)]
    Render(#[from] RenderError),
    #[error(transparent)]
    Send(#[from] SendError),
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// The data the email is about is missing or inconsistent (retrying cannot help).
    #[error("{0}")]
    Data(String),
    #[error("unknown email template {0:?}")]
    UnknownTemplate(String),
}

impl EmailError {
    /// Worth retrying later.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Database(_) | Self::Send(SendError::Transient(_))
        )
    }
}
