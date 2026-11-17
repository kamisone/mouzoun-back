//! Object storage abstraction.
//!
//! Business code talks to [`ObjectStorage`]; [`gcs::GcsBucket`] is the production
//! implementation. `memory::InMemoryStorage` backs the test-suite only (`test-utils` feature).
//!
//! One bucket holds public and private files. Its access control is fine-grained: the API writes
//! every object itself (server to server, so the bucket needs no CORS) with its ACL
//! ([`ObjectAcl`]); browsers never upload to the bucket directly.

pub mod gcs;
#[cfg(any(test, feature = "test-utils"))]
pub mod memory;

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;

/// One bucket. Implementations must be cheap to share (`Arc<dyn ObjectStorage>`).
#[async_trait]
pub trait ObjectStorage: Send + Sync + 'static {
    /// Writes an object with its content type, cache policy and ACL (replaces any same key).
    async fn put(&self, key: &str, bytes: Bytes, options: &PutOptions) -> Result<(), StorageError>;

    /// Time-limited download URL (private objects).
    async fn signed_download_url(
        &self,
        key: &str,
        ttl: Duration,
        download_filename: Option<&str>,
    ) -> Result<String, StorageError>;

    /// Object metadata, `None` if the object does not exist.
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError>;

    /// Idempotent: deleting a missing object succeeds.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Stable public URL (only meaningful for `public-read` objects).
    fn public_url(&self, key: &str) -> String;

    /// Bucket name, as stored in `media_assets.bucket`.
    fn bucket_name(&self) -> &str;

    /// Cheap authenticated round-trip, used by startup diagnostics.
    async fn check(&self) -> Result<(), StorageError>;
}

/// The application's bucket.
pub type SharedStorage = Arc<dyn ObjectStorage>;

/// Access of one object, set when it is written (GCS predefined ACLs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectAcl {
    /// Anyone can read it at its public URL (site photos). The project keeps full control.
    PublicRead,
    /// Only the project (owner, IAM grants) can read it; others get a signed URL.
    Private,
}

impl ObjectAcl {
    /// Name of the predefined ACL in the GCS API.
    pub fn predefined(self) -> &'static str {
        match self {
            Self::PublicRead => "publicRead",
            Self::Private => "private",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PutOptions {
    pub content_type: String,
    pub acl: ObjectAcl,
    pub cache_control: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub size: u64,
    pub content_type: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage signing failed: {0}")]
    Signing(String),
    #[error("storage request failed: {0}")]
    Backend(String),
}
