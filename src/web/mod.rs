//! HTTP layer: router composition and cross-cutting middleware.

pub mod extract;
pub mod health;
pub mod pagination;

use std::time::Duration;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderName, HeaderValue, Method, StatusCode, header},
};
use tower::ServiceBuilder;
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{auth, modules, state::AppState};

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn router(state: AppState) -> Router {
    let api_v1 = Router::new()
        .nest("/auth", auth::routes())
        .nest("/activities", modules::activities::routes())
        .nest("/media", modules::media::routes());

    Router::new()
        .merge(health::routes())
        .nest("/v1", api_v1)
        // Files never transit through the API (direct-to-GCS uploads), so JSON bodies stay small.
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(REQUEST_ID, MakeRequestUuid))
                .layer(TraceLayer::new_for_http())
                .layer(PropagateRequestIdLayer::new(REQUEST_ID))
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(15),
                ))
                .layer(CompressionLayer::new())
                .layer(cors(&state)),
        )
        .with_state(state)
}

fn cors(state: &AppState) -> CorsLayer {
    let origins: Vec<HeaderValue> = state
        .config
        .cors_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        .expose_headers([REQUEST_ID])
        .max_age(Duration::from_secs(3600))
}
