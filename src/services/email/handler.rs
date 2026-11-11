//! Outbox events → emails.
//!
//! | event                               | guest email          | staff email              |
//! |-------------------------------------|----------------------|--------------------------|
//! | booking.created (pending/awaiting)  | booking_received     | staff_new_booking        |
//! | booking.confirmed                   | booking_confirmation |                          |
//! | booking.modified                    | booking_modified     |                          |
//! | booking.cancelled                   | booking_cancelled    | staff_booking_cancelled  |
//! | booking.expired                     | booking_cancelled    |                          |
//! | booking.pre_arrival_due (scheduled) | pre_arrival          |                          |
//! | booking.review_due (scheduled)      | review_request       |                          |
//! | payment.succeeded                   | payment_receipt      |                          |
//! | inquiry.created                     | inquiry_auto_reply   | staff_new_inquiry        |
//! | newsletter.subscribed               | newsletter_confirm   |                          |
//! | auth.password_reset_requested       | password_reset       |                          |
//! | auth.email_verification_requested   | email_verification   |                          |
//! | auth.staff_invited                  | staff_invitation     |                          |
//!
//! Each handler re-reads the current state: an email that no longer makes sense (booking
//! cancelled before its pre-arrival email, token already used...) is skipped, not sent.

use async_trait::async_trait;
use chrono::Utc;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use uuid::Uuid;

use super::{EmailError, EmailJob, Mailer, Source, context};
use crate::{
    db::{identity::TokenPurpose, operations::OutboxEvent},
    services::{
        outbox::{
            AuthTokenPayload, BookingPayload, InquiryPayload, NewsletterPayload, PaymentPayload,
            types,
        },
        settings,
    },
    worker::{HandlerError, OutboxHandler, scheduler},
};

pub struct EmailHandler {
    pool: PgPool,
    mailer: Mailer,
}

impl EmailHandler {
    pub fn new(pool: PgPool, mailer: Mailer) -> Self {
        Self { pool, mailer }
    }
}

const EVENT_TYPES: &[&str] = &[
    types::BOOKING_CREATED,
    types::BOOKING_CONFIRMED,
    types::BOOKING_MODIFIED,
    types::BOOKING_CANCELLED,
    types::BOOKING_EXPIRED,
    types::BOOKING_PRE_ARRIVAL_DUE,
    types::BOOKING_REVIEW_DUE,
    types::PAYMENT_SUCCEEDED,
    types::INQUIRY_CREATED,
    types::NEWSLETTER_SUBSCRIBED,
    types::PASSWORD_RESET_REQUESTED,
    types::EMAIL_VERIFICATION_REQUESTED,
    types::STAFF_INVITED,
];

#[async_trait]
impl OutboxHandler for EmailHandler {
    fn name(&self) -> &'static str {
        "email"
    }

    fn event_types(&self) -> &'static [&'static str] {
        EVENT_TYPES
    }

    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError> {
        let jobs = self.jobs(event).await?;
        let mut failure: Option<EmailError> = None;
        for job in jobs {
            // One failing recipient must not block the others; already-sent ones are skipped on
            // retry thanks to email_log.
            if let Err(e) = self.mailer.deliver(Some(event.id), &job).await {
                tracing::warn!(event_id = event.id, template = %job.template, error = %e, "email not sent");
                failure = Some(match failure {
                    Some(prev) if !prev.is_transient() => prev,
                    _ => e,
                });
            }
        }
        match failure {
            None => Ok(()),
            Some(e) if e.is_transient() => Err(HandlerError::Retry(e.into())),
            Some(e) => Err(HandlerError::Permanent(e.into())),
        }
    }
}

impl EmailHandler {
    async fn jobs(&self, event: &OutboxEvent) -> Result<Vec<EmailJob>, HandlerError> {
        match event.event_type.as_str() {
            types::BOOKING_CREATED => {
                let p: BookingPayload = payload(event)?;
                let mut jobs = Vec::new();
                if let Some(guest) = self
                    .booking_job(
                        p.booking_id,
                        "booking_received",
                        None,
                        &["draft", "pending", "awaiting_payment"],
                    )
                    .await?
                {
                    jobs.push(guest);
                }
                jobs.extend(
                    self.staff_job(
                        "staff_new_booking",
                        Some(p.booking_id),
                        Source::Booking {
                            booking_id: p.booking_id,
                            payment_id: None,
                        },
                    )
                    .await?,
                );
                Ok(jobs)
            }
            types::BOOKING_CONFIRMED => {
                self.guest_only(
                    event,
                    "booking_confirmation",
                    &["confirmed", "checked_in", "completed"],
                )
                .await
            }
            types::BOOKING_MODIFIED => {
                self.guest_only(
                    event,
                    "booking_modified",
                    &["pending", "awaiting_payment", "confirmed"],
                )
                .await
            }
            types::BOOKING_EXPIRED => {
                self.guest_only(event, "booking_cancelled", &["expired"])
                    .await
            }
            types::BOOKING_CANCELLED => {
                let p: BookingPayload = payload(event)?;
                let mut jobs: Vec<EmailJob> = self
                    .booking_job(p.booking_id, "booking_cancelled", None, &["cancelled"])
                    .await?
                    .into_iter()
                    .collect();
                jobs.extend(
                    self.staff_job(
                        "staff_booking_cancelled",
                        Some(p.booking_id),
                        Source::Booking {
                            booking_id: p.booking_id,
                            payment_id: None,
                        },
                    )
                    .await?,
                );
                Ok(jobs)
            }
            types::BOOKING_PRE_ARRIVAL_DUE => {
                let p: BookingPayload = payload(event)?;
                let days = settings::number(&self.pool, scheduler::PRE_ARRIVAL_DAYS_KEY, 3)
                    .await
                    .map_err(retry)?;
                let recipient = context::booking_recipient(&self.pool, p.booking_id)
                    .await
                    .map_err(retry)?;
                if let Some(arrival) = recipient.as_ref().and_then(|r| r.arrival) {
                    // The event became due at `available_at`; a late worker is later still.
                    let reference = event.available_at.max(Utc::now());
                    let due = scheduler::pre_arrival_due_at(arrival, days);
                    // The stay moved later since this was planned: wait for the new date.
                    if due > event.available_at + chrono::Duration::minutes(1) {
                        return Err(HandlerError::Defer(due));
                    }
                    // Too late to be useful (the worker was down past the arrival).
                    if arrival < crate::camp_time::today(reference) {
                        return Ok(vec![]);
                    }
                }
                Ok(self
                    .booking_job(p.booking_id, "pre_arrival", None, &["confirmed"])
                    .await?
                    .into_iter()
                    .collect())
            }
            types::BOOKING_REVIEW_DUE => {
                self.guest_only(event, "review_request", &["completed"])
                    .await
            }
            types::PAYMENT_SUCCEEDED => {
                let p: PaymentPayload = payload(event)?;
                let succeeded = sqlx::query_scalar!(
                    "SELECT status = 'succeeded' AS \"ok!\" FROM payments WHERE id = $1",
                    p.payment_id
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?
                .unwrap_or(false);
                if !succeeded {
                    return Ok(vec![]);
                }
                Ok(self
                    .booking_job(
                        p.booking_id,
                        "payment_receipt",
                        Some(p.payment_id),
                        &[
                            "pending",
                            "awaiting_payment",
                            "confirmed",
                            "checked_in",
                            "completed",
                        ],
                    )
                    .await?
                    .into_iter()
                    .collect())
            }
            types::INQUIRY_CREATED => {
                let p: InquiryPayload = payload(event)?;
                let Some((inquiry, locale)) = context::inquiry_facts(&self.pool, p.inquiry_id)
                    .await
                    .map_err(retry)?
                else {
                    return Ok(vec![]);
                };
                let mut jobs = vec![EmailJob {
                    template: "inquiry_auto_reply".into(),
                    to_email: inquiry.email.clone(),
                    to_name: Some(inquiry.name.clone()),
                    locales: vec![locale],
                    booking_id: None,
                    source: Source::Inquiry {
                        inquiry_id: p.inquiry_id,
                    },
                }];
                jobs.extend(
                    self.staff_job(
                        "staff_new_inquiry",
                        None,
                        Source::Inquiry {
                            inquiry_id: p.inquiry_id,
                        },
                    )
                    .await?,
                );
                Ok(jobs)
            }
            types::NEWSLETTER_SUBSCRIBED => {
                let p: NewsletterPayload = payload(event)?;
                let token = p.secret.map(|s| s.token).ok_or_else(|| permanent("the confirmation token is gone (event replayed after completion): ask the subscriber to subscribe again"))?;
                let Some(sub) = sqlx::query!(
                    "SELECT email::text AS \"email!\", locale, status FROM newsletter_subscribers WHERE id = $1",
                    p.subscriber_id
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?
                else {
                    return Ok(vec![]);
                };
                if sub.status != "pending" {
                    return Ok(vec![]);
                }
                Ok(vec![EmailJob {
                    template: "newsletter_confirm".into(),
                    to_email: sub.email,
                    to_name: None,
                    locales: vec![sub.locale],
                    booking_id: None,
                    source: Source::Newsletter { token },
                }])
            }
            types::PASSWORD_RESET_REQUESTED => {
                self.account_job(event, "password_reset", TokenPurpose::PasswordReset)
                    .await
            }
            types::EMAIL_VERIFICATION_REQUESTED => {
                self.account_job(event, "email_verification", TokenPurpose::EmailVerification)
                    .await
            }
            types::STAFF_INVITED => {
                self.account_job(event, "staff_invitation", TokenPurpose::Invitation)
                    .await
            }
            other => {
                tracing::warn!(event_type = other, "email handler: unsupported event type");
                Ok(vec![])
            }
        }
    }

    async fn guest_only(
        &self,
        event: &OutboxEvent,
        template: &str,
        statuses: &[&str],
    ) -> Result<Vec<EmailJob>, HandlerError> {
        let p: BookingPayload = payload(event)?;
        Ok(self
            .booking_job(p.booking_id, template, None, statuses)
            .await?
            .into_iter()
            .collect())
    }

    /// The guest's email for a booking, if the booking is still in one of `statuses` and the
    /// guest has an email address.
    async fn booking_job(
        &self,
        booking_id: Uuid,
        template: &str,
        payment_id: Option<Uuid>,
        statuses: &[&str],
    ) -> Result<Option<EmailJob>, HandlerError> {
        let Some(r) = context::booking_recipient(&self.pool, booking_id)
            .await
            .map_err(retry)?
        else {
            tracing::warn!(%booking_id, template, "booking not found, email skipped");
            return Ok(None);
        };
        if !statuses.contains(&r.status.as_str()) {
            tracing::info!(%booking_id, template, status = %r.status, "booking status changed since the event, email skipped");
            return Ok(None);
        }
        let Some(to) = r.email else {
            tracing::info!(%booking_id, template, "guest has no email address, email skipped");
            return Ok(None);
        };
        Ok(Some(EmailJob {
            template: template.into(),
            to_email: to,
            to_name: Some(r.name),
            locales: r.locales,
            booking_id: Some(booking_id),
            source: Source::Booking {
                booking_id,
                payment_id,
            },
        }))
    }

    /// Staff notifications go to `notifications.staff_email`, always in the default locale.
    async fn staff_job(
        &self,
        template: &str,
        booking_id: Option<Uuid>,
        source: Source,
    ) -> Result<Option<EmailJob>, HandlerError> {
        let Some(to) = context::staff_email(&self.pool).await.map_err(retry)? else {
            tracing::debug!(
                template,
                "notifications.staff_email not set, staff email skipped"
            );
            return Ok(None);
        };
        Ok(Some(EmailJob {
            template: template.into(),
            to_email: to,
            to_name: None,
            locales: vec![],
            booking_id,
            source,
        }))
    }

    async fn account_job(
        &self,
        event: &OutboxEvent,
        template: &str,
        purpose: TokenPurpose,
    ) -> Result<Vec<EmailJob>, HandlerError> {
        let p: AuthTokenPayload = payload(event)?;
        let token = p.secret.map(|s| s.token).ok_or_else(|| {
            permanent(
                "the one-time token is gone (event replayed after completion): issue a new one",
            )
        })?;
        let Some(row) = sqlx::query!(
            r#"SELECT u.email::text AS "email!", u.full_name, u.preferred_locale, u.is_staff, t.expires_at, t.used_at, t.purpose
               FROM auth_tokens t JOIN users u ON u.id = t.user_id
               WHERE t.id = $1 AND t.user_id = $2"#,
            p.auth_token_id,
            p.user_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(retry)?
        else {
            return Ok(vec![]);
        };
        if row.used_at.is_some() || row.expires_at <= Utc::now() || row.purpose != purpose.as_str()
        {
            tracing::info!(
                event_id = event.id,
                template,
                "token used, expired or of another purpose: email skipped"
            );
            return Ok(vec![]);
        }
        Ok(vec![EmailJob {
            template: template.into(),
            to_email: row.email,
            to_name: Some(row.full_name),
            locales: vec![row.preferred_locale],
            booking_id: None,
            source: Source::Account {
                user_id: p.user_id,
                purpose,
                token,
                expires_at: row.expires_at,
                invited_by: p.invited_by,
                staff: row.is_staff,
            },
        }])
    }
}

fn payload<T: DeserializeOwned>(event: &OutboxEvent) -> Result<T, HandlerError> {
    serde_json::from_value(event.payload.clone())
        .map_err(|e| permanent(&format!("invalid {} payload: {e}", event.event_type)))
}

fn retry(e: sqlx::Error) -> HandlerError {
    HandlerError::Retry(e.into())
}

fn permanent(message: &str) -> HandlerError {
    HandlerError::Permanent(anyhow::anyhow!(message.to_string()))
}
