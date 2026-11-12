//! Renders and sends one email, at most once per (event, recipient, template).
//!
//! `email_log` is the idempotency record, written with its own commits (never inside the
//! dispatcher's transaction, which a crash would roll back):
//!
//! ```text
//! queued ──► sending ──► sent
//!   ▲           └──────► failed ──(retry)──► sending ...
//! ```
//!
//! - `sent` (or later): never sent again;
//! - `sending` found on retry: the previous attempt died while talking to the provider, so the
//!   guest may have the email already. It is NOT resent (stays `sending`, alert logged).

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::{
    EmailError, Mailer, OutgoingEmail, ProviderMessageId, context, render,
    templates::{self, LoadedTemplate},
};
use crate::db::identity::TokenPurpose;

/// What an email is about: its variables are loaded from this when it is rendered.
#[derive(Debug, Clone)]
pub enum Source {
    Booking {
        booking_id: Uuid,
        payment_id: Option<Uuid>,
    },
    /// A booking email with extra variables (`access_url`, `refund`...).
    BookingWith {
        booking_id: Uuid,
        extra: Value,
    },
    Inquiry {
        inquiry_id: Uuid,
    },
    Newsletter {
        token: String,
    },
    Account {
        user_id: Uuid,
        purpose: TokenPurpose,
        token: String,
        expires_at: DateTime<Utc>,
        invited_by: Option<Uuid>,
        staff: bool,
    },
    /// Explicit variables (previews, tests).
    Data(Value),
}

#[derive(Debug, Clone)]
pub struct EmailJob {
    pub template: String,
    pub to_email: String,
    pub to_name: Option<String>,
    /// Preferred locales, best first; the first active one wins, else the default locale.
    pub locales: Vec<Option<String>>,
    pub booking_id: Option<Uuid>,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Sent(ProviderMessageId),
    /// Already sent by an earlier attempt.
    AlreadySent,
    /// An earlier attempt was interrupted mid-send: not resent, to avoid a duplicate.
    Uncertain,
    /// Nothing to send (template deactivated...).
    Skipped(String),
}

impl Mailer {
    /// Sends `job` once for `event_id`. Without an event id (test sends) every call sends.
    pub async fn deliver(
        &self,
        event_id: Option<i64>,
        job: &EmailJob,
    ) -> Result<Delivery, EmailError> {
        let locales = templates::active_locales(&self.pool).await?;
        let locale = templates::resolve_locale(&job.locales, &locales);
        let default_locale = templates::resolve_locale(&[], &locales);
        let Some(template) = templates::load(&self.pool, &job.template, &locale).await? else {
            tracing::warn!(template = %job.template, "email template unknown or inactive, nothing sent");
            return Ok(Delivery::Skipped(format!(
                "template {} unknown or inactive",
                job.template
            )));
        };

        let log_id = match self.claim_log(event_id, job, &template).await? {
            Claim::Proceed(id) => id,
            Claim::Done => return Ok(Delivery::AlreadySent),
            Claim::Interrupted(id) => {
                // Stays `sending` (never resent automatically, even if the event is retried for
                // another recipient). To force a resend: set it to 'failed', then replay the event.
                sqlx::query!(
                    "UPDATE email_log SET error = 'interrupted while sending: delivery unknown, not resent' WHERE id = $1",
                    id
                )
                .execute(&self.pool)
                .await?;
                tracing::error!(alert = true, email_log_id = id, template = %job.template, to = %job.to_email, "email delivery uncertain (crash during send): check with the guest");
                return Ok(Delivery::Uncertain);
            }
        };

        let rendered = match self.render(&template, &job.source, &default_locale).await {
            Ok(r) => r,
            Err(e) => {
                self.finish_log(log_id, Err(&e.to_string())).await?;
                return Err(e);
            }
        };
        sqlx::query!(
            "UPDATE email_log SET status = 'sending', attempts = attempts + 1, subject = $2, error = NULL WHERE id = $1",
            log_id,
            rendered.subject
        )
        .execute(&self.pool)
        .await?;

        let message = OutgoingEmail {
            to_email: job.to_email.clone(),
            to_name: job.to_name.clone(),
            subject: rendered.subject,
            html: rendered.html,
            text: rendered.text,
            reply_to: self.settings.reply_to.clone(),
        };
        match self.sender.send(message).await {
            Ok(id) => {
                sqlx::query!(
                    "UPDATE email_log SET status = 'sent', provider_message_id = $2, sent_at = now() WHERE id = $1",
                    log_id,
                    id.0
                )
                .execute(&self.pool)
                .await?;
                tracing::info!(email_log_id = log_id, template = %job.template, locale = %template.locale, "email sent");
                Ok(Delivery::Sent(id))
            }
            Err(e) => {
                self.finish_log(log_id, Err(&e.to_string())).await?;
                Err(e.into())
            }
        }
    }

    pub(crate) async fn render(
        &self,
        template: &LoadedTemplate,
        source: &Source,
        default_locale: &str,
    ) -> Result<render::Rendered, EmailError> {
        let data = context::build(
            &self.pool,
            &self.settings,
            source,
            &template.locale,
            default_locale,
        )
        .await?;
        Ok(render::render(
            &template.source(),
            &data,
            &template.locale,
            template.rtl,
        )?)
    }

    async fn claim_log(
        &self,
        event_id: Option<i64>,
        job: &EmailJob,
        template: &LoadedTemplate,
    ) -> sqlx::Result<Claim> {
        let inserted = sqlx::query_scalar!(
            "INSERT INTO email_log (template_code, to_email, booking_id, outbox_event_id, locale, status)
             VALUES ($1, $2, $3, $4, $5, 'queued')
             ON CONFLICT (outbox_event_id, to_email, template_code) DO NOTHING
             RETURNING id",
            template.code,
            job.to_email,
            job.booking_id,
            event_id,
            template.locale
        )
        .fetch_optional(&self.pool)
        .await?;
        if let Some(id) = inserted {
            return Ok(Claim::Proceed(id));
        }
        let existing = sqlx::query!(
            "SELECT id, status FROM email_log WHERE outbox_event_id = $1 AND to_email = $2 AND template_code = $3",
            event_id,
            job.to_email,
            template.code
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(match existing.status.as_str() {
            "queued" | "failed" => Claim::Proceed(existing.id),
            "sending" => Claim::Interrupted(existing.id),
            _ => Claim::Done,
        })
    }

    async fn finish_log(&self, id: i64, result: Result<(), &str>) -> sqlx::Result<()> {
        if let Err(error) = result {
            sqlx::query!(
                "UPDATE email_log SET status = 'failed', error = $2 WHERE id = $1",
                id,
                error
            )
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }
}

enum Claim {
    Proceed(i64),
    Done,
    Interrupted(i64),
}
