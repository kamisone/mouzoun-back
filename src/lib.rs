//! Mouzoun API — library crate (the binary in `main.rs` is a thin launcher,
//! integration tests in `tests/` drive the same router).

pub mod app;
pub mod auth;
pub mod camp_time;
pub mod config;
pub mod db;
pub mod error;
pub mod infra;
pub mod jobs;
pub mod modules;
pub mod seed;
pub mod services;
pub mod state;
pub mod web;
pub mod worker;
