#![allow(dead_code)] // shared by several test binaries, each using a subset

//! Test harness: real router + real Postgres (one fresh database per test via `sqlx::test`)
//! + real Redis (namespaced per test) + in-memory object storage.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use mouzoun_api::{
    auth::{Role, jwt},
    config::{Config, GcsCredentials, MediaConfig, StorageConfig},
    infra::{
        cache::Cache,
        storage::{Buckets, memory::InMemoryStorage},
    },
    state::AppState,
    web,
};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const JWT_SECRET: &str = "test-secret-test-secret-test-secret-0000";

pub struct TestApp {
    pub router: Router,
    pub db: PgPool,
    pub storage: Arc<InMemoryStorage>,
    pub admin_token: String,
    pub state: AppState,
}

pub fn test_config() -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        database_url: String::new(),
        database_max_connections: 5,
        redis_url: redis_url(),
        jwt_secret: JWT_SECRET.into(),
        jwt_ttl_minutes: 60,
        cors_origins: vec![],
        log_json: false,
        admin_email: None,
        admin_password: None,
        storage: StorageConfig {
            public_bucket: "test-bucket".into(),
            private_bucket: None,
            public_base_url: "https://cdn.test".into(),
            credentials: GcsCredentials::ApplicationDefault,
        },
        media: MediaConfig {
            max_upload_bytes: 1024 * 1024,
            upload_url_ttl: Duration::from_secs(900),
            download_url_ttl: Duration::from_secs(900),
            pending_ttl: Duration::from_secs(3600),
            cleanup_interval: Duration::from_secs(3600),
        },
    }
}

fn redis_url() -> String {
    dotenvy::dotenv().ok();
    std::env::var("REDIS_URL").expect("REDIS_URL must be set (docker compose up -d)")
}

impl TestApp {
    pub async fn new(db: PgPool) -> Self {
        Self::with_private_bucket(db, false).await
    }

    pub async fn with_private_bucket(db: PgPool, private: bool) -> Self {
        let config = test_config();
        let cache = Cache::connect(&config.redis_url, format!("test:{}", Uuid::new_v4()))
            .await
            .expect("redis");
        let storage = Arc::new(InMemoryStorage::new());
        let buckets = Buckets {
            public: storage.clone(),
            private: private.then(|| Arc::new(InMemoryStorage::new()) as _),
        };

        let admin_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, password_hash, full_name, role)
             VALUES ('admin@test.ma', 'x', 'Admin', 'admin') RETURNING id",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        let admin_token = jwt::issue(JWT_SECRET, admin_id, Role::Admin, 60).unwrap();

        let state = AppState::new(config, db.clone(), cache, buckets);
        Self {
            router: web::router(state.clone()),
            db,
            storage,
            admin_token,
            state,
        }
    }

    pub async fn request(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    pub async fn admin(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let token = self.admin_token.clone();
        self.request(method, uri, body, Some(&token)).await
    }
}

/// Smallest valid PNG header bytes (enough for magic-byte sniffing).
pub fn png_bytes(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.resize(len, 0);
    bytes
}
