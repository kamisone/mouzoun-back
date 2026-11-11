//! Data layer: one module per migration section (models, enums mirroring CHECK constraints,
//! and the shared value types). Business rules live in `crate::services`.

pub mod bookings;
pub mod catalog;
pub mod content;
pub mod foundation;
pub mod identity;
pub mod inventory;
pub mod media;
pub mod operations;
pub mod pricing;
pub mod taxonomy;
pub mod types;
