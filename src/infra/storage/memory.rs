//! In-memory [`ObjectStorage`] for the test-suite. Compiled only with `cfg(test)` or the
//! `test-utils` feature: it is not part of the production binary.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;

use super::{ObjectAcl, ObjectMeta, ObjectStorage, PutOptions, StorageError};

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
    acl: Option<ObjectAcl>,
    cache_control: Option<String>,
}

/// What the API wrote for one object (assertions in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMeta {
    pub size: usize,
    pub content_type: String,
    pub acl: Option<ObjectAcl>,
    pub cache_control: Option<String>,
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

    pub fn object(&self, key: &str) -> Option<StoredMeta> {
        self.objects.lock().unwrap().get(key).map(|o| StoredMeta {
            size: o.bytes.len(),
            content_type: o.content_type.clone(),
            acl: o.acl,
            cache_control: o.cache_control.clone(),
        })
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
    async fn put(&self, key: &str, bytes: Bytes, options: &PutOptions) -> Result<(), StorageError> {
        self.objects.lock().unwrap().insert(
            key.to_string(),
            StoredObject {
                bytes,
                content_type: options.content_type.clone(),
                acl: Some(options.acl),
                cache_control: options.cache_control.clone(),
            },
        );
        Ok(())
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
