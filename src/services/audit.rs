//! Audit trail: who changed what, written in the same transaction as the change.

use ipnetwork::IpNetwork;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::operations::ActorType;

/// Who acts: a staff user, the guest (customer), the system (jobs) or a provider webhook.
#[derive(Debug, Clone, Default)]
pub struct Actor {
    pub kind: Option<ActorType>,
    pub user_id: Option<Uuid>,
    pub ip_address: Option<IpNetwork>,
    pub user_agent: Option<String>,
    pub request_id: Option<String>,
}

impl Actor {
    pub fn system() -> Self {
        Self {
            kind: Some(ActorType::System),
            ..Self::default()
        }
    }

    pub fn webhook() -> Self {
        Self {
            kind: Some(ActorType::Webhook),
            ..Self::default()
        }
    }

    pub fn customer() -> Self {
        Self {
            kind: Some(ActorType::Customer),
            ..Self::default()
        }
    }

    pub fn staff(user_id: Uuid) -> Self {
        Self {
            kind: Some(ActorType::User),
            user_id: Some(user_id),
            ..Self::default()
        }
    }

    pub fn with_request(
        mut self,
        ip: Option<IpNetwork>,
        user_agent: Option<String>,
        request_id: Option<String>,
    ) -> Self {
        self.ip_address = ip;
        self.user_agent = user_agent;
        self.request_id = request_id;
        self
    }

    fn actor_type(&self) -> ActorType {
        self.kind.unwrap_or(if self.user_id.is_some() {
            ActorType::User
        } else {
            ActorType::System
        })
    }
}

/// `action` is `<entity>.<verb>` (`booking.cancelled`, `payment.recorded`...).
pub async fn record(
    conn: &mut PgConnection,
    actor: &Actor,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    changes: Value,
) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO audit_logs (actor_type, actor_user_id, action, entity_type, entity_id, changes,
                                 ip_address, user_agent, request_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        actor.actor_type().as_str(),
        actor.user_id,
        action,
        entity_type,
        entity_id,
        changes,
        actor.ip_address,
        actor.user_agent.as_deref(),
        actor.request_id.as_deref()
    )
    .execute(conn)
    .await?;
    Ok(())
}
