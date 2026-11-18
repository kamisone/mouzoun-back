//! Media library backed by object storage (back-office only).
//!
//! Upload: `POST /v1/admin/media` (multipart). The API checks the type, the size and the magic
//! bytes, writes the object to the bucket with the ACL of its visibility (`publicRead` for site
//! photos, `private` for personal files) and inserts the `media_assets` row. Browsers never
//! write to the bucket, so it needs no CORS rule and its ACLs stay under the API's control.

mod handlers;
pub mod model;
pub mod policy;
mod service;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, put},
};

pub use service::MediaService;

use crate::state::AppState;

/// Room for the multipart envelope around the largest allowed file.
const MULTIPART_OVERHEAD: usize = 64 * 1024;

/// `/v1/admin/media` (staff guard applied by the admin router).
pub fn admin_routes(max_upload_bytes: i64) -> Router<AppState> {
    let limit = usize::try_from(max_upload_bytes).unwrap_or(0) + MULTIPART_OVERHEAD;
    Router::new()
        .route(
            "/",
            get(handlers::list)
                // Only this route accepts large bodies; the API-wide limit stays at 1 MiB.
                .post(handlers::upload)
                .layer(DefaultBodyLimit::max(limit)),
        )
        .route("/{id}", get(handlers::get).delete(handlers::delete))
        // Alt texts, titles and captions per language (content.write).
        .route(
            "/{id}/texts",
            put(crate::modules::pages::gallery::admin_media_texts),
        )
}
