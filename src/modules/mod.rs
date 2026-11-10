//! Business modules. Each one is layered the same way:
//!
//! - `mod.rs`        routes (HTTP surface of the module)
//! - `handlers.rs`   HTTP adapters: extract, authorize, call the service, shape the response
//! - `service.rs`    business rules, orchestration, caching
//! - `repository.rs` SQL only
//! - `model.rs`      database rows, domain types and API DTOs
//!
//! Add new modules (camps, bookings, products...) following `activities`.
pub mod activities;
pub mod media;
