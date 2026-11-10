//! Google Cloud Storage implementation, built on Google's official Rust SDK.
//!
//! - Signed URLs are produced locally with the service-account key (no network call).
//! - Metadata operations use `StorageControl`, data reads use `Storage`.
//! - Both clients pool connections internally and are shared across buckets.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use async_trait::async_trait;
use axum::http::Method;
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use google_cloud_auth::{
    credentials::{self, Credentials, service_account},
    signer::Signer,
};
use google_cloud_gax::error::rpc::Code;
use google_cloud_storage::{
    builder::storage::SignedUrlBuilder,
    client::{Storage, StorageControl},
    model_ext::ReadRange,
};
use serde_json::json;

use super::{
    Buckets, ObjectMeta, ObjectStorage, SignedRequest, StorageError, UploadConstraints,
    upload_headers,
};
use crate::config::{GcsCredentials, StorageConfig};

struct GcsClients {
    storage: Storage,
    control: StorageControl,
    signer: Signer,
}

pub struct GcsBucket {
    clients: Arc<GcsClients>,
    /// Bucket resource name: `projects/_/buckets/<name>`.
    resource: String,
    public_base_url: String,
}

/// Builds the GCS clients and the configured buckets. Fails fast on malformed credentials.
pub async fn connect(config: &StorageConfig) -> anyhow::Result<Buckets> {
    let (credentials, signer) = build_credentials(&config.credentials)?;
    let storage = Storage::builder()
        .with_credentials(credentials.clone())
        .build()
        .await
        .context("building GCS storage client")?;
    let control = StorageControl::builder()
        .with_credentials(credentials)
        .build()
        .await
        .context("building GCS control client")?;
    let clients = Arc::new(GcsClients {
        storage,
        control,
        signer,
    });

    let public = Arc::new(GcsBucket::new(
        clients.clone(),
        &config.public_bucket,
        config.public_base_url.clone(),
    ));
    let private = config.private_bucket.as_deref().map(|name| {
        Arc::new(GcsBucket::new(
            clients.clone(),
            name,
            format!("https://storage.googleapis.com/{name}"),
        )) as Arc<dyn ObjectStorage>
    });

    // Signing is local: this proves the private key is usable before we accept traffic.
    public
        .signed_download_url("__startup_check__", Duration::from_secs(60), None)
        .await
        .context("GCS credentials cannot sign URLs (check GCS_PRIVATE_KEY)")?;

    Ok(Buckets { public, private })
}

fn build_credentials(creds: &GcsCredentials) -> anyhow::Result<(Credentials, Signer)> {
    match creds {
        GcsCredentials::ServiceAccount {
            project_id,
            client_email,
            private_key_id,
            private_key,
        } => {
            let key = json!({
                "type": "service_account",
                "project_id": project_id,
                "private_key_id": private_key_id,
                "private_key": private_key,
                "client_email": client_email,
                "token_uri": "https://oauth2.googleapis.com/token",
            });
            let credentials = service_account::Builder::new(key.clone())
                .build()
                .context("invalid GCS service-account credentials")?;
            let signer = service_account::Builder::new(key)
                .build_signer()
                .context("invalid GCS service-account signer")?;
            Ok((credentials, signer))
        }
        GcsCredentials::ApplicationDefault => {
            let credentials = credentials::Builder::default()
                .build()
                .context("no Application Default Credentials found")?;
            let signer = credentials::Builder::default()
                .build_signer()
                .context("Application Default Credentials cannot sign URLs")?;
            Ok((credentials, signer))
        }
    }
}

impl GcsBucket {
    fn new(clients: Arc<GcsClients>, bucket: &str, public_base_url: String) -> Self {
        Self {
            clients,
            resource: format!("projects/_/buckets/{bucket}"),
            public_base_url,
        }
    }
}

#[async_trait]
impl ObjectStorage for GcsBucket {
    async fn signed_upload(
        &self,
        key: &str,
        constraints: &UploadConstraints,
    ) -> Result<SignedRequest, StorageError> {
        let headers = upload_headers(constraints);
        let mut builder = SignedUrlBuilder::for_object(&self.resource, key)
            .with_method(Method::PUT)
            .with_expiration(constraints.ttl);
        for (name, value) in &headers {
            builder = builder.with_header(name, value);
        }
        let url = builder
            .sign_with(&self.clients.signer)
            .await
            .map_err(|e| StorageError::Signing(e.to_string()))?;
        Ok(SignedRequest {
            url,
            method: "PUT",
            headers,
            expires_at: Utc::now() + constraints.ttl,
        })
    }

    async fn signed_download_url(
        &self,
        key: &str,
        ttl: Duration,
        download_filename: Option<&str>,
    ) -> Result<String, StorageError> {
        let mut builder = SignedUrlBuilder::for_object(&self.resource, key)
            .with_method(Method::GET)
            .with_expiration(ttl);
        if let Some(name) = download_filename {
            builder = builder.with_query_param(
                "response-content-disposition",
                format!("attachment; filename=\"{name}\""),
            );
        }
        builder
            .sign_with(&self.clients.signer)
            .await
            .map_err(|e| StorageError::Signing(e.to_string()))
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, StorageError> {
        match self
            .clients
            .control
            .get_object()
            .set_bucket(&self.resource)
            .set_object(key)
            .send()
            .await
        {
            Ok(obj) => Ok(Some(ObjectMeta {
                size: u64::try_from(obj.size).unwrap_or_default(),
                content_type: obj.content_type,
            })),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(backend(e)),
        }
    }

    async fn read_prefix(&self, key: &str, len: u64) -> Result<Bytes, StorageError> {
        let mut response = self
            .clients
            .storage
            .read_object(&self.resource, key)
            .set_read_range(ReadRange::head(len))
            .send()
            .await
            .map_err(backend)?;
        let mut buf = BytesMut::new();
        while let Some(chunk) = response.next().await {
            buf.extend_from_slice(&chunk.map_err(backend)?);
            if buf.len() as u64 >= len {
                break;
            }
        }
        buf.truncate(len as usize);
        Ok(buf.freeze())
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self
            .clients
            .control
            .delete_object()
            .set_bucket(&self.resource)
            .set_object(key)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(backend(e)),
        }
    }

    fn public_url(&self, key: &str) -> String {
        format!("{}/{key}", self.public_base_url)
    }

    async fn check(&self) -> Result<(), StorageError> {
        // A 404 on a probe key proves the credentials and bucket access work.
        self.head("__healthcheck__").await.map(|_| ())
    }
}

fn is_not_found(e: &google_cloud_storage::Error) -> bool {
    e.http_status_code() == Some(404) || e.status().is_some_and(|s| s.code == Code::NotFound)
}

fn backend(e: google_cloud_storage::Error) -> StorageError {
    StorageError::Backend(e.to_string())
}
