//! Outbox handlers and their registry.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::db::operations::OutboxEvent;

/// Why an event could not be handled (yet).
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// Temporary: retried with backoff, up to `OUTBOX_MAX_ATTEMPTS`.
    #[error("{0:#}")]
    Retry(anyhow::Error),
    /// Retrying cannot help (bad template, invalid data): the event is marked dead at once.
    #[error("{0:#}")]
    Permanent(anyhow::Error),
    /// Not an error: the event arrived early; handle it again at this time (no attempt counted).
    #[error("deferred until {0}")]
    Defer(DateTime<Utc>),
}

/// Reacts to outbox events. Must be idempotent: an event can be delivered more than once
/// (crash after the side effect, before the commit).
#[async_trait]
pub trait OutboxHandler: Send + Sync + 'static {
    /// For logs.
    fn name(&self) -> &'static str;
    /// Event types this handler subscribes to.
    fn event_types(&self) -> &'static [&'static str];
    async fn handle(&self, event: &OutboxEvent) -> Result<(), HandlerError>;
}

/// Routes an event type to every handler subscribed to it.
#[derive(Default, Clone)]
pub struct Registry {
    routes: HashMap<&'static str, Vec<Arc<dyn OutboxHandler>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, handler: Arc<dyn OutboxHandler>) -> Self {
        for event_type in handler.event_types() {
            self.routes
                .entry(event_type)
                .or_default()
                .push(handler.clone());
        }
        self
    }

    pub fn handlers(&self, event_type: &str) -> &[Arc<dyn OutboxHandler>] {
        self.routes
            .get(event_type)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}
