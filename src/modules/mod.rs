//! HTTP-facing modules. Each one is layered the same way:
//!
//! - `mod.rs`        routes (HTTP surface of the module)
//! - `handlers.rs`   HTTP adapters: extract, call the service, shape the response
//! - `service.rs`    business rules and orchestration
//! - `model.rs`      request/response types
//!
//! The data layer is `crate::db`; booking, pricing and inventory rules are in `crate::services`.
pub mod booking;
pub mod media;
