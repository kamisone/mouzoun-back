//! Integration tests: one test program (each file a module) instead of one per file, so the
//! whole API and its libraries are linked once, not ~20 times. Run one area with a filter:
//! `cargo test --test it accounts::` (module path) or any test name.

mod common;

mod accounts;
mod auth;
mod booking_accommodation;
mod booking_activity;
mod booking_admin;
mod booking_api;
mod booking_trip;
mod catalog;
mod crm;
mod email;
mod go_live;
mod health;
mod inventory;
mod media;
mod migrations;
mod numbering;
mod oidc;
mod outbox;
mod passkeys;
mod schema;
