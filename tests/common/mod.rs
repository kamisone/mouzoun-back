//! Test harness: real router + real Postgres (one fresh database per test via `sqlx::test`)
//! + real Redis (namespaced per test) + in-memory object storage.
#![allow(dead_code)] // shared by several test binaries, each using a subset

pub mod fixtures;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use mouzoun_api::{
    auth::jwt,
    config::{
        AppEnv, BookingConfig, Config, EmailConfig, EmailProvider, GcsCredentials, MediaConfig,
        StorageConfig, WorkerConfig,
    },
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
pub const ALLOWED_ORIGIN: &str = "https://mouzoun.test";

pub struct TestApp {
    pub router: Router,
    pub db: PgPool,
    pub storage: Arc<InMemoryStorage>,
    pub admin_token: String,
    pub state: AppState,
}

pub fn test_config() -> Config {
    Config {
        app_env: AppEnv::Test,
        host: "127.0.0.1".into(),
        port: 0,
        database_url: String::new(),
        database_max_connections: 5,
        run_migrations: false,
        redis_url: redis_url(),
        jwt_secret: JWT_SECRET.into(),
        jwt_ttl_minutes: 60,
        cookie_secure: true,
        cors_origins: vec![ALLOWED_ORIGIN.into()],
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
        email: EmailConfig {
            provider: EmailProvider::Log,
            from_email: "no-reply@mouzoun.test".into(),
            from_name: "Mouzoun".into(),
            reply_to: None,
            smtp: None,
            public_site_url: ALLOWED_ORIGIN.into(),
        },
        worker: WorkerConfig {
            in_api: false,
            max_attempts: 8,
            batch_size: 20,
            poll_interval: Duration::from_secs(60),
            scheduler_interval: Duration::from_secs(3600),
        },
        booking: BookingConfig {
            fake_payments: true,
            guest_data_key: Some([7; 32]),
            trust_proxy: true,
            hibp_enabled: false,
            api_public_url: "https://mouzoun.test/api".into(),
            webauthn_rp_id: "mouzoun.test".into(),
            webauthn_origin: ALLOWED_ORIGIN.into(),
            ..BookingConfig::default()
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
            private: private.then(|| Arc::new(InMemoryStorage::named("memory-private")) as _),
        };

        let admin_id = create_user(&db, "admin@test.ma", "x", true, Some("super_admin")).await;
        let admin_token = jwt::issue(JWT_SECRET, admin_id, true, 60).unwrap();

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
        let headers: Vec<(&str, String)> = token
            .map(|t| vec![("authorization", format!("Bearer {t}"))])
            .unwrap_or_default();
        let (status, _, json) = self.send(method, uri, body, &headers).await;
        (status, json)
    }

    /// Full control over request headers; returns the response headers too.
    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, String)],
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            req = req.header(*name, value);
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
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, json)
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

/// Active user with an optional role. `password` is stored as given (hash it first for logins).
pub async fn create_user(
    db: &PgPool,
    email: &str,
    password_hash: &str,
    is_staff: bool,
    role: Option<&str>,
) -> Uuid {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (id, email, password_hash, full_name, status, is_staff)
         VALUES ($1, $2, $3, 'User', 'active', $4) RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(email)
    .bind(password_hash)
    .bind(is_staff)
    .fetch_one(db)
    .await
    .unwrap();
    if let Some(role) = role {
        sqlx::query(
            "INSERT INTO user_roles (user_id, role_id) SELECT $1, id FROM roles WHERE code = $2",
        )
        .bind(id)
        .bind(role)
        .execute(db)
        .await
        .unwrap();
    }
    id
}

/// Smallest valid PNG header bytes (enough for magic-byte sniffing).
pub fn png_bytes(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.resize(len, 0);
    bytes
}
