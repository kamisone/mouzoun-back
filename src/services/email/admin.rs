//! Back-office operations on emails (exposed by the admin API and the `email` CLI).

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgPool;

use super::{
    EmailError, Mailer, OutgoingEmail, ProviderMessageId, Rendered, Source, samples, templates,
};

/// Renders `code` in `locale` with `sample_data`, or the built-in sample of that template.
pub async fn render_preview(
    mailer: &Mailer,
    code: &str,
    locale: &str,
    sample_data: Option<Value>,
) -> Result<Rendered, EmailError> {
    let locales = templates::active_locales(&mailer.pool).await?;
    let default_locale = templates::resolve_locale(&[], &locales);
    let template = templates::load(&mailer.pool, code, locale)
        .await?
        .ok_or_else(|| EmailError::UnknownTemplate(code.into()))?;
    let data = match sample_data {
        Some(data) => data,
        None => samples::for_template(code, &template.locale, &mailer.settings.public_site_url)
            .ok_or_else(|| EmailError::UnknownTemplate(code.into()))?,
    };
    mailer
        .render(&template, &Source::Data(data), &default_locale)
        .await
}

/// Sends the sample rendering of `code` to `to_email` (subject prefixed with `[TEST]`); logged
/// in `email_log` without an event.
pub async fn send_test(
    mailer: &Mailer,
    code: &str,
    locale: &str,
    to_email: &str,
) -> Result<ProviderMessageId, EmailError> {
    let rendered = render_preview(mailer, code, locale, None).await?;
    let subject = format!("[TEST] {}", rendered.subject);
    let log_id = sqlx::query_scalar!(
        "INSERT INTO email_log (template_code, to_email, locale, subject, status, attempts)
         VALUES ($1, $2, $3, $4, 'sending', 1) RETURNING id",
        code,
        to_email,
        locale,
        subject
    )
    .fetch_one(&mailer.pool)
    .await?;
    let result = mailer
        .sender
        .send(OutgoingEmail {
            to_email: to_email.into(),
            to_name: None,
            subject,
            html: rendered.html,
            text: rendered.text,
            reply_to: mailer.settings.reply_to.clone(),
        })
        .await;
    match &result {
        Ok(id) => sqlx::query!(
            "UPDATE email_log SET status = 'sent', provider_message_id = $2, sent_at = now() WHERE id = $1",
            log_id,
            id.0
        )
        .execute(&mailer.pool)
        .await?,
        Err(e) => sqlx::query!("UPDATE email_log SET status = 'failed', error = $2 WHERE id = $1", log_id, e.to_string())
            .execute(&mailer.pool)
            .await?,
    };
    Ok(result?)
}

#[derive(Debug, Clone, Serialize)]
pub struct DeadEvent {
    pub id: i64,
    pub event_type: String,
    pub aggregate_id: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub failed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub async fn list_dead(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<DeadEvent>> {
    sqlx::query_as!(
        DeadEvent,
        "SELECT id, event_type, aggregate_id, attempts, last_error, failed_at, created_at
         FROM outbox_events WHERE failed_at IS NOT NULL
         ORDER BY failed_at DESC LIMIT $1",
        limit
    )
    .fetch_all(pool)
    .await
}

/// Puts a dead event back in the queue (attempts reset). Emails it already sent are not sent
/// again (email_log). Returns false if the event is not dead.
pub async fn retry_event(pool: &PgPool, event_id: i64) -> sqlx::Result<bool> {
    let updated = sqlx::query!(
        "UPDATE outbox_events
         SET failed_at = NULL, attempts = 0, last_error = NULL, available_at = now()
         WHERE id = $1 AND failed_at IS NOT NULL",
        event_id
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}
