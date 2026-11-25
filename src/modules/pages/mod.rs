//! Page builder: section types (`section_types/*.json`), sections and items of pages, shared
//! sections, draft → published revisions, public rendering. See docs/page-builder/README.md.
//!
//! Back office (`/v1/admin`, staff only, then one permission per endpoint):
//!
//! | endpoint | permission |
//! |---|---|
//! | GET section-types, GET pages, GET pages/{id}/builder, GET pages/{id}/revisions, GET pages/{id}/publish-check, GET revisions/{id}, GET shared-sections…, GET usages/{type}/{id}, GET lookup/{kind}, POST pages/{id}/preview-token, GET galleries/{id} | content.read |
//! | POST pages, PUT pages/{id}/builder, POST pages/{id}/blocks, POST blocks/{id}/duplicate, PATCH/DELETE blocks/{id}, POST/PUT/DELETE shared-sections…, POST galleries, PUT galleries/{id}, PUT media/{id}/texts | content.write (+ the type's `required_permission` for the sections touched) |
//! | POST pages/{id}/publish, POST revisions/{id}/restore, POST revisions/{id}/cancel | content.publish |
//! | PATCH section-types/{code} (activate, preview image) | content.html |
//!
//! Public: GET pages/home, GET pages/by-slug/{*slug} (`?locale=`, `?preview=`), POST
//! pages/{id}/preview/section, GET sections/{block_id}/gallery (photos of a gallery section,
//! on demand), GET banners, POST newsletter, POST contact.

pub mod cache;
pub mod gallery;
mod handlers;
pub mod model;
pub mod public;
pub mod registry;
mod repo;
pub mod service;
mod site;
pub mod validate;

use axum::{
    Router,
    routing::{get, patch, post},
};

pub use registry::SectionRegistry;

use crate::state::AppState;

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/section-types", get(handlers::section_types))
        .route("/section-types/{code}", patch(handlers::patch_section_type))
        .route("/pages", get(handlers::pages).post(handlers::create_page))
        .route("/pages/legal-tokens", get(handlers::legal_tokens))
        .route(
            "/pages/{id}/builder",
            get(handlers::builder).put(handlers::save_builder),
        )
        .route("/pages/{id}/blocks", post(handlers::add_block))
        .route("/pages/{id}/publish", post(handlers::publish))
        .route("/pages/{id}/publish-check", get(handlers::publish_check))
        .route("/pages/{id}/revisions", get(handlers::revisions))
        .route("/pages/{id}/preview-token", post(handlers::preview_token))
        .route(
            "/blocks/{id}",
            patch(handlers::patch_block).delete(handlers::delete_block),
        )
        .route("/blocks/{id}/duplicate", post(handlers::duplicate_block))
        .route("/revisions/{id}", get(handlers::revision))
        .route("/revisions/{id}/restore", post(handlers::restore_revision))
        .route("/revisions/{id}/cancel", post(handlers::cancel_revision))
        .route(
            "/shared-sections",
            get(handlers::shared_list).post(handlers::shared_create),
        )
        .route(
            "/shared-sections/{id}",
            get(handlers::shared_get)
                .put(handlers::shared_update)
                .delete(handlers::shared_delete),
        )
        .route("/shared-sections/{id}/usages", get(handlers::shared_usages))
        .route("/usages/{ref_type}/{id}", get(handlers::entity_usages))
        .route("/lookup/{kind}", get(handlers::lookup))
        .route("/galleries", post(gallery::admin_create))
        .route(
            "/galleries/{id}",
            get(gallery::admin_get).put(gallery::admin_update),
        )
}

pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/pages/home", get(handlers::home))
        .route("/pages/by-slug/{*slug}", get(handlers::by_slug))
        .route(
            "/pages/{id}/preview/section",
            post(handlers::preview_section),
        )
        .route("/sections/{block_id}/gallery", get(gallery::photos))
        .route("/banners", get(site::banners))
        .route("/newsletter", post(site::newsletter))
        .route("/contact", post(site::contact))
}
