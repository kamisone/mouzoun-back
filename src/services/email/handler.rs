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
//! | booking.declined                    | booking_declined     |                          |
//! | booking.request_accepted            | booking_payment_link |                          |
//! | booking.balance_due (scheduled)     | balance_reminder     |                          |
//! | booking.balance_failed              | balance_failed       | staff_balance_failed     |
//! | booking.precheckin_due (scheduled)  | precheckin_reminder  |                          |
//! | booking.access_requested            | booking_magic_link   |                          |
//! | refund.succeeded                    | refund_receipt       |                          |
//! | cart.abandoned (consent only, once) | cart_abandoned       |                          |
//!
//! `booking.created` emails the guest for requests and staff-made bookings only: a website
//! guest paying online is on the payment page, and staff hear about the booking when the
//! payment succeeds (`payment.succeeded` of a deposit / full payment).
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
            AuthTokenPayload, BookingAccessPayload, BookingPayload, InquiryPayload,
            NewsletterPayload, PaymentPayload, RefundPayload, types,
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
    types::BOOKING_DECLINED,
    types::BOOKING_REQUEST_ACCEPTED,
    types::BOOKING_BALANCE_DUE,
    types::BOOKING_BALANCE_FAILED,
    types::BOOKING_PRECHECKIN_DUE,
    types::BOOKING_ACCESS_REQUESTED,
    types::REFUND_SUCCEEDED,
    types::CART_ABANDONED,
    types::AUTH_LOGIN_CODE,
    types::ACCOUNT_CREATED,
    types::AUTH_NEW_DEVICE,
    types::AUTH_PASSWORD_CHANGED,
    types::AUTH_EMAIL_CHANGE_REQUESTED,
    types::AUTH_EMAIL_CHANGED,
    types::AUTH_PASSKEY_ADDED,
    types::AUTH_IDENTITY_LINKED,
    types::BOOKING_CLAIM_REQUESTED,
    types::BOOKING_CLAIMED,
    types::ACCOUNT_EXPORT_READY,
    types::ACCOUNT_DELETION_SCHEDULED,
    types::ACCOUNT_DELETED,
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
                let Some(r) = context::booking_recipient(&self.pool, p.booking_id)
                    .await
                    .map_err(retry)?
                else {
                    return Ok(vec![]);
                };
                // A website guest paying online is on the payment page right now.
                let paying_online = r.status == "awaiting_payment" && r.source == "website";
                let mut jobs = Vec::new();
                if !paying_online
                    && let Some(guest) = self
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
                if !paying_online {
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
                }
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
                let payment = sqlx::query!(
                    r#"SELECT p.status = 'succeeded' AS "ok!", p.purpose, b.source
                       FROM payments p JOIN bookings b ON b.id = p.booking_id WHERE p.id = $1"#,
                    p.payment_id
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?;
                let Some(payment) = payment.filter(|p| p.ok) else {
                    return Ok(vec![]);
                };
                let mut jobs: Vec<EmailJob> = self
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
                    .collect();
                // An online checkout is a new booking for the team once its payment arrived.
                if payment.source == "website"
                    && matches!(payment.purpose.as_str(), "deposit" | "full")
                {
                    jobs.extend(
                        self.staff_job(
                            "staff_new_booking",
                            Some(p.booking_id),
                            Source::Booking {
                                booking_id: p.booking_id,
                                payment_id: Some(p.payment_id),
                            },
                        )
                        .await?,
                    );
                }
                Ok(jobs)
            }
            types::BOOKING_DECLINED => {
                self.guest_only(event, "booking_declined", &["cancelled"])
                    .await
            }
            types::BOOKING_REQUEST_ACCEPTED => {
                self.guest_only(event, "booking_payment_link", &["awaiting_payment"])
                    .await
            }
            types::BOOKING_BALANCE_DUE => {
                let p: BookingPayload = payload(event)?;
                let due = sqlx::query_scalar!(
                    "SELECT total_minor - amount_paid_minor FROM bookings WHERE id = $1",
                    p.booking_id
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?
                .flatten()
                .unwrap_or(0);
                if due <= 0 {
                    return Ok(vec![]);
                }
                Ok(self
                    .booking_job(p.booking_id, "balance_reminder", None, &["confirmed"])
                    .await?
                    .into_iter()
                    .collect())
            }
            types::BOOKING_BALANCE_FAILED => {
                let p: BookingPayload = payload(event)?;
                let mut jobs: Vec<EmailJob> = self
                    .booking_job(p.booking_id, "balance_failed", None, &["confirmed"])
                    .await?
                    .into_iter()
                    .collect();
                jobs.extend(
                    self.staff_job(
                        "staff_balance_failed",
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
            types::BOOKING_PRECHECKIN_DUE => {
                let p: BookingPayload = payload(event)?;
                let done = sqlx::query_scalar!(
                    "SELECT precheckin_completed_at IS NOT NULL AS \"done!\" FROM bookings WHERE id = $1",
                    p.booking_id
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?
                .unwrap_or(true);
                if done {
                    return Ok(vec![]);
                }
                Ok(self
                    .booking_job(p.booking_id, "precheckin_reminder", None, &["confirmed"])
                    .await?
                    .into_iter()
                    .collect())
            }
            types::BOOKING_ACCESS_REQUESTED => {
                let p: BookingAccessPayload = payload(event)?;
                let token = p.secret.map(|s| s.token).ok_or_else(|| {
                    HandlerError::Permanent(anyhow::anyhow!(
                        "magic link token already scrubbed: ask for a new link"
                    ))
                })?;
                let Some(mut job) = self
                    .booking_job(
                        p.booking_id,
                        "booking_magic_link",
                        None,
                        &[
                            "pending",
                            "awaiting_payment",
                            "confirmed",
                            "checked_in",
                            "completed",
                            "cancelled",
                            "expired",
                            "no_show",
                        ],
                    )
                    .await?
                else {
                    return Ok(vec![]);
                };
                let reference = sqlx::query_scalar!(
                    "SELECT reference FROM bookings WHERE id = $1",
                    p.booking_id
                )
                .fetch_one(&self.pool)
                .await
                .map_err(retry)?;
                let locale = job
                    .locales
                    .iter()
                    .flatten()
                    .next()
                    .cloned()
                    .unwrap_or_default();
                let url = super::links::magic_url(
                    &self.mailer.settings().public_site_url,
                    &locale,
                    &reference,
                    &token,
                );
                job.source = Source::BookingWith {
                    booking_id: p.booking_id,
                    extra: serde_json::json!({ "access_url": url }),
                };
                Ok(vec![job])
            }
            types::REFUND_SUCCEEDED => {
                let p: RefundPayload = payload(event)?;
                let refund = sqlx::query!(
                    r#"SELECT r.amount_minor, p.currency::text AS "currency!" FROM refunds r
                       JOIN payments p ON p.id = r.payment_id WHERE r.id = $1 AND r.status = 'succeeded'"#,
                    p.refund_id
                )
                .fetch_optional(&self.pool)
                .await
                .map_err(retry)?;
                let Some(refund) = refund else {
                    return Ok(vec![]);
                };
                let Some(mut job) = self
                    .booking_job(
                        p.booking_id,
                        "refund_receipt",
                        None,
                        &[
                            "pending",
                            "awaiting_payment",
                            "confirmed",
                            "checked_in",
                            "completed",
                            "cancelled",
                            "expired",
                            "no_show",
                        ],
                    )
                    .await?
                else {
                    return Ok(vec![]);
                };
                job.source = Source::BookingWith {
                    booking_id: p.booking_id,
                    extra: serde_json::json!({ "refund": { "amount_minor": refund.amount_minor, "currency": refund.currency } }),
                };
                Ok(vec![job])
            }
            types::CART_ABANDONED => {
                let p: BookingPayload = payload(event)?;
                let consent = context::booking_recipient(&self.pool, p.booking_id)
                    .await
                    .map_err(retry)?
                    .is_some_and(|r| r.marketing_consent);
                if !consent {
                    return Ok(vec![]);
                }
                self.guest_only(event, "cart_abandoned", &["expired"]).await
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
                if let Some(jobs) = super::account_jobs::jobs(
                    &self.pool,
                    &self.mailer.settings().public_site_url,
                    event,
                )
                .await?
                {
                    return Ok(jobs);
                }
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
