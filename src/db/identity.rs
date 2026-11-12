//! 0002_identity: users, roles, permissions, sessions, one-time tokens, settings.

use chrono::{DateTime, Utc};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::Locale;
use crate::text_enum;

text_enum!(UserStatus { Invited = "invited", Active = "active", Suspended = "suspended" });
text_enum!(TokenPurpose {
    PasswordReset = "password_reset",
    EmailVerification = "email_verification",
    Invitation = "invitation",
    EmailChange = "email_change",
    BookingClaim = "booking_claim",
});
text_enum!(SessionKind { Guest = "guest", Staff = "staff" });
text_enum!(IdentityProvider { Google = "google", Apple = "apple", Facebook = "facebook" });
text_enum!(NotificationCategory { Reminders = "reminders", Offers = "offers", Reviews = "reviews" });
text_enum!(NotificationChannel { Email = "email", Sms = "sms", Whatsapp = "whatsapp" });
text_enum!(DeletionRequestedBy { Customer = "customer", Staff = "staff" });
text_enum!(SettingValueType {
    String = "string",
    Number = "number",
    Boolean = "boolean",
    Json = "json",
    LocalizedText = "localized_text",
    MediaId = "media_id",
});

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    #[serde(skip)]
    pub password_hash: Option<String>,
    pub full_name: String,
    pub phone: Option<String>,
    pub preferred_locale: Option<Locale>,
    pub status: UserStatus,
    pub is_staff: bool,
    #[serde(skip)]
    pub totp_secret_enc: Option<Vec<u8>>,
    pub email_verified_at: Option<DateTime<Utc>>,
    pub last_login_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Role {
    pub id: Uuid,
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub is_system: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Permission {
    pub code: String,
    pub module: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserSession {
    pub id: Uuid,
    pub user_id: Uuid,
    #[serde(skip)]
    pub refresh_token_hash: Vec<u8>,
    pub user_agent: Option<String>,
    pub ip_address: Option<IpNetwork>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AuthToken {
    pub id: Uuid,
    pub user_id: Uuid,
    pub purpose: TokenPurpose,
    #[serde(skip)]
    pub token_hash: Vec<u8>,
    pub expires_at: DateTime<Utc>,
    pub used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Setting {
    pub key: String,
    pub group_name: String,
    pub value_type: SettingValueType,
    pub value: serde_json::Value,
    pub is_public: bool,
    pub description: Option<String>,
    pub updated_by: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}
