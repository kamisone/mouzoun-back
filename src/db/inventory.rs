//! 0007_inventory: units, per-night inventory, activity schedules and slots, resources, closures.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::types::{DateRange, Locale};
use crate::text_enum;

text_enum!(
    /// Status of a physical asset (accommodation unit or resource).
    AssetStatus { Active = "active", Maintenance = "maintenance", Retired = "retired" });
text_enum!(SlotStatus { Open = "open", Closed = "closed", Full = "full", Cancelled = "cancelled" });
text_enum!(ResourceKind {
    Vehicle = "vehicle",
    Equipment = "equipment",
    Animal = "animal",
    Guide = "guide",
    Room = "room",
    Other = "other",
});
text_enum!(ClosureScope { Site = "site", Product = "product", Unit = "unit", Resource = "resource" });

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AccommodationUnit {
    pub id: Uuid,
    pub product_id: Uuid,
    pub code: String,
    pub name: Option<String>,
    pub place_id: Option<Uuid>,
    pub status: AssetStatus,
    pub latitude: Option<Decimal>,
    pub longitude: Option<Decimal>,
    pub notes: Option<String>,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One night of availability of an accommodation type.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct InventoryDay {
    pub product_id: Uuid,
    pub day: NaiveDate,
    pub capacity: i32,
    pub reserved: i32,
    pub stop_sell: bool,
    pub min_nights_override: Option<i16>,
    pub closed_to_arrival: bool,
    pub closed_to_departure: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ActivitySchedule {
    pub id: Uuid,
    pub product_id: Uuid,
    pub days_of_week: i16,
    /// Local time (Africa/Casablanca).
    pub start_time: NaiveTime,
    pub capacity: i32,
    pub valid_period: DateRange,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ActivitySlot {
    pub id: Uuid,
    pub product_id: Uuid,
    pub schedule_id: Option<Uuid>,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub capacity: i32,
    pub reserved: i32,
    pub status: SlotStatus,
    pub guide_user_id: Option<Uuid>,
    pub guide_language: Option<Locale>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Resource {
    pub id: Uuid,
    pub kind: ResourceKind,
    pub code: String,
    pub name: String,
    pub seats: Option<i32>,
    pub user_id: Option<Uuid>,
    pub status: AssetStatus,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// e.g. a quad tour needs 1 `vehicle` per 2 participants.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ResourceRequirement {
    pub product_id: Uuid,
    pub resource_kind: ResourceKind,
    pub units_per_group: i32,
    pub participants_per_group: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ResourceRequirement {
    /// Resources needed for `participants`: one group per `participants_per_group` (rounded up).
    pub fn units_needed(&self, participants: i32) -> i32 {
        let groups = (participants + self.participants_per_group - 1) / self.participants_per_group;
        groups * self.units_per_group
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Closure {
    pub id: Uuid,
    pub scope: ClosureScope,
    pub product_id: Option<Uuid>,
    pub unit_id: Option<Uuid>,
    pub resource_id: Option<Uuid>,
    pub period: DateRange,
    pub reason: Option<String>,
    pub is_public: bool,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
