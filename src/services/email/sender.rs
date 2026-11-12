//! Email transport. Business code depends on [`EmailSender`] only, so an HTTP provider
//! (Brevo, Postmark, SES...) can be added later without touching callers.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::{
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
};
use uuid::Uuid;

use crate::config::{EmailConfig, EmailProvider, SmtpTls};

/// A fully rendered email, ready for the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingEmail {
    pub to_email: String,
    pub to_name: Option<String>,
    pub subject: String,
    pub html: String,
    pub text: String,
    pub reply_to: Option<String>,
}

/// The provider's id for the message (SMTP: the `Message-ID` we set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderMessageId(pub String);

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// Worth retrying (network, 4xx SMTP reply, provider outage).
    #[error("temporary email failure: {0}")]
    Transient(String),
    /// Retrying cannot help (invalid address, 5xx SMTP reply, rejected content).
    #[error("permanent email failure: {0}")]
    Permanent(String),
}

#[async_trait]
pub trait EmailSender: Send + Sync + 'static {
    async fn send(&self, msg: OutgoingEmail) -> Result<ProviderMessageId, SendError>;
}

/// Builds the sender selected by `EMAIL_PROVIDER`.
pub fn from_config(config: &EmailConfig) -> anyhow::Result<Arc<dyn EmailSender>> {
    Ok(match config.provider {
        EmailProvider::Log => Arc::new(LogSender),
        EmailProvider::Smtp => Arc::new(SmtpSender::new(config)?),
    })
}

/// SMTP with TLS (STARTTLS on 587, implicit TLS on 465) or plain for a local Mailpit.
pub struct SmtpSender {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    reply_to: Option<Mailbox>,
    message_id_domain: String,
}

impl SmtpSender {
    pub fn new(config: &EmailConfig) -> anyhow::Result<Self> {
        let smtp = config
            .smtp
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("EMAIL_PROVIDER=smtp needs SMTP_HOST"))?;
        let tls = match smtp.tls {
            SmtpTls::None => Tls::None,
            SmtpTls::StartTls => Tls::Required(TlsParameters::new(smtp.host.clone())?),
            SmtpTls::Implicit => Tls::Wrapper(TlsParameters::new(smtp.host.clone())?),
        };
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&smtp.host)
            .port(smtp.port)
            .tls(tls)
            .timeout(Some(Duration::from_secs(20)));
        if let (Some(user), Some(password)) = (&smtp.user, &smtp.password) {
            builder = builder.credentials(Credentials::new(user.clone(), password.clone()));
        }
        let from: Mailbox = format!("{} <{}>", config.from_name, config.from_email).parse()?;
        let reply_to = config.reply_to.as_deref().map(str::parse).transpose()?;
        let message_id_domain = config
            .from_email
            .rsplit_once('@')
            .map(|(_, d)| d.to_string())
            .unwrap_or_else(|| "localhost".into());
        Ok(Self {
            transport: builder.build(),
            from,
            reply_to,
            message_id_domain,
        })
    }
}

#[async_trait]
impl EmailSender for SmtpSender {
    async fn send(&self, msg: OutgoingEmail) -> Result<ProviderMessageId, SendError> {
        let to = match &msg.to_name {
            Some(name) => Mailbox::new(Some(name.clone()), msg.to_email.parse().map_err(invalid)?),
            None => Mailbox::new(None, msg.to_email.parse().map_err(invalid)?),
        };
        let message_id = format!("<{}@{}>", Uuid::now_v7(), self.message_id_domain);
        let mut builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(&msg.subject)
            .message_id(Some(message_id.clone()));
        let reply_to = msg
            .reply_to
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(invalid)?;
        if let Some(reply_to) = reply_to.or_else(|| self.reply_to.clone()) {
            builder = builder.reply_to(reply_to);
        }
        let email = builder
            .multipart(MultiPart::alternative_plain_html(msg.text, msg.html))
            .map_err(|e| SendError::Permanent(e.to_string()))?;
        match self.transport.send(email).await {
            Ok(_) => Ok(ProviderMessageId(message_id)),
            Err(e) if e.is_permanent() => Err(SendError::Permanent(e.to_string())),
            Err(e) => Err(SendError::Transient(e.to_string())),
        }
    }
}

fn invalid(e: impl std::fmt::Display) -> SendError {
    SendError::Permanent(format!("invalid address: {e}"))
}

/// Development: prints the email instead of sending it.
pub struct LogSender;

#[async_trait]
impl EmailSender for LogSender {
    async fn send(&self, msg: OutgoingEmail) -> Result<ProviderMessageId, SendError> {
        let id = format!("log-{}", Uuid::now_v7());
        tracing::info!(
            message_id = %id,
            to = %msg.to_email,
            subject = %msg.subject,
            html_bytes = msg.html.len(),
            "email (EMAIL_PROVIDER=log, not sent):\n{}",
            msg.text
        );
        Ok(ProviderMessageId(id))
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub use recording::RecordingSender;

#[cfg(any(test, feature = "test-utils"))]
mod recording {
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::sync::Notify;

    use super::*;

    /// Test double: records what is sent; the first `fail_first` sends fail transiently.
    #[derive(Default)]
    pub struct RecordingSender {
        pub sent: Mutex<Vec<OutgoingEmail>>,
        fail_first: AtomicUsize,
        attempts: AtomicUsize,
        delivered: Notify,
    }

    impl RecordingSender {
        pub fn failing(times: usize) -> Self {
            Self {
                fail_first: AtomicUsize::new(times),
                ..Self::default()
            }
        }
        pub fn sent(&self) -> Vec<OutgoingEmail> {
            self.sent.lock().unwrap().clone()
        }
        pub fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }
        /// Resolves as soon as `count` emails are sent, or `false` after `limit`.
        pub async fn wait_for(&self, count: usize, limit: Duration) -> bool {
            tokio::time::timeout(limit, async {
                loop {
                    let notified = self.delivered.notified();
                    if self.sent.lock().unwrap().len() >= count {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .is_ok()
        }
    }

    #[async_trait]
    impl EmailSender for RecordingSender {
        async fn send(&self, msg: OutgoingEmail) -> Result<ProviderMessageId, SendError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let remaining = self.fail_first.load(Ordering::SeqCst);
            if remaining > 0 {
                self.fail_first.store(remaining - 1, Ordering::SeqCst);
                return Err(SendError::Transient("simulated outage".into()));
            }
            let id = {
                let mut sent = self.sent.lock().unwrap();
                sent.push(msg);
                format!("test-{}", sent.len())
            };
            self.delivered.notify_waiters();
            Ok(ProviderMessageId(id))
        }
    }
}
