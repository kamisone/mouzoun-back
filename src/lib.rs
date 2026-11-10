//! Mouzoun API — library crate (the binary in `main.rs` is a thin launcher,
//! integration tests in `tests/` drive the same router).

pub mod app;
pub mod auth;
pub mod config;
pub mod error;
pub mod infra;
pub mod jobs;
pub mod modules;
pub mod seed;
pub mod state;
pub mod web;
