//! Object storage abstraction.
//!
//! Business code talks to [`ObjectStorage`]; [`gcs::GcsBucket`] is the production
//! implementation. `memory::InMemoryStorage` backs the test-suite only (`test-utils` feature).

pub mod gcs;
#[cfg(any(test, feature = "test-utils"))]
pub mod memory;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::Serialize;

/// One bucket. Implementations must be cheap to share (`Arc<dyn ObjectStorage>`).
#[async_trait]
pub trait ObjectStorage: Send + Sync + 'static {
    /// Pre-signed request the client uses to upload bytes directly to the bucket.
    /// The signature pins content type, exact size and cache headers.
    async fn signed_upload(
        &self,
        key: &str,
        constraints: &UploadConstraints,
    ) -> Result<SignedRequest, StorageError>;

    /// Time-limited download URL (private objects).
    async fn signed_download_url(
        &self,
        key: &str,
        ttl: Duration,
        download_filename: Option<&str>,
    ) -> Result<String, StorageError>;

    /// Object metadata, `None` if the object does not exist.
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError>;

    /// First `len` bytes of the object (content sniffing).
    async fn read_prefix(&self, key: &str, len: u64) -> Result<Bytes, StorageError>;

    /// Idempotent: deleting a missing object succeeds.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Stable public URL (only meaningful for publicly readable buckets).
    fn public_url(&self, key: &str) -> String;

    /// Bucket name, as stored in `media_assets.bucket`.
    fn bucket_name(&self) -> &str;

    /// Cheap authenticated round-trip, used by startup diagnostics.
    async fn check(&self) -> Result<(), StorageError>;
}

/// The buckets available to the application.
#[derive(Clone)]
pub struct Buckets {
    pub public: Arc<dyn ObjectStorage>,
    pub private: Option<Arc<dyn ObjectStorage>>,
}

#[derive(Debug, Clone)]
pub struct UploadConstraints {
    pub content_type: String,
    pub size_bytes: u64,
    pub cache_control: Option<String>,
    pub ttl: Duration,
}

/// Everything a client needs to perform the upload request.
#[derive(Debug, Clone, Serialize)]
pub struct SignedRequest {
    pub url: String,
    pub method: &'static str,
    /// Headers the client MUST send verbatim (they are part of the signature).
    pub headers: BTreeMap<String, String>,
    pub expires_at: DateTime<Utc>,
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

/// Headers pinned into every signed upload.
pub(crate) fn upload_headers(c: &UploadConstraints) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert("content-type".to_string(), c.content_type.clone());
    // GCS rejects the upload unless its size is exactly the declared one.
    headers.insert(
        "x-goog-content-length-range".to_string(),
        format!("{0},{0}", c.size_bytes),
    );
    if let Some(cc) = &c.cache_control {
        headers.insert("cache-control".to_string(), cc.clone());
    }
    headers
}
