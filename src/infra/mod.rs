//! Infrastructure adapters: database, cache and object storage.
//! Business modules depend on these abstractions, never on vendor SDKs directly.
pub mod cache;
pub mod db;
pub mod storage;
