//! In-memory [`ObjectStorage`] for the test-suite. Compiled only with `cfg(test)` or the
//! `test-utils` feature: it is not part of the production binary.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;

use super::{
    ObjectMeta, ObjectStorage, SignedRequest, StorageError, UploadConstraints, upload_headers,
};

pub struct InMemoryStorage {
    name: String,
    objects: Mutex<HashMap<String, StoredObject>>,
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::named("memory-bucket")
    }
}

#[derive(Clone)]
struct StoredObject {
    bytes: Bytes,
    content_type: String,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            objects: Mutex::default(),
        }
    }

    /// Simulates the client-side upload to the signed URL.
    pub fn put(&self, key: &str, bytes: impl Into<Bytes>, content_type: &str) {
        self.objects.lock().unwrap().insert(
            key.to_string(),
            StoredObject {
                bytes: bytes.into(),
                content_type: content_type.to_string(),
            },
        );
    }

    pub fn contains(&self, key: &str) -> bool {
        self.objects.lock().unwrap().contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ObjectStorage for InMemoryStorage {
    async fn signed_upload(
        &self,
        key: &str,
        constraints: &UploadConstraints,
    ) -> Result<SignedRequest, StorageError> {
        Ok(SignedRequest {
            url: format!("memory://upload/{key}"),
            method: "PUT",
            headers: upload_headers(constraints),
            expires_at: Utc::now() + constraints.ttl,
        })
    }

    async fn signed_download_url(
        &self,
        key: &str,
        _ttl: Duration,
        _download_filename: Option<&str>,
    ) -> Result<String, StorageError> {
        Ok(format!("memory://download/{key}?signed=1"))
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        Ok(self.objects.lock().unwrap().get(key).map(|o| ObjectMeta {
            size: o.bytes.len() as u64,
            content_type: o.content_type.clone(),
        }))
    }

    async fn read_prefix(&self, key: &str, len: u64) -> Result<Bytes, StorageError> {
        let objects = self.objects.lock().unwrap();
        let obj = objects
            .get(key)
            .ok_or_else(|| StorageError::Backend(format!("{key} not found")))?;
        let end = (len as usize).min(obj.bytes.len());
        Ok(obj.bytes.slice(..end))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }

    fn public_url(&self, key: &str) -> String {
        format!("https://cdn.test/{key}")
    }

    fn bucket_name(&self) -> &str {
        &self.name
    }

    async fn check(&self) -> Result<(), StorageError> {
        Ok(())
    }
}
