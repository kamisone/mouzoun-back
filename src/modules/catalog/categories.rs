//! Stay and activity categories (`/v1/admin/catalog/categories`, `categories.scope = 'product'`):
//! Admin › Catalogue › Catégories. The same screens and rules as the shop categories, with
//! `products.read` / `products.write`, one flat ordered list (no subcategories) and no public
//! page (so no redirects or reserved slugs). The page builder's carousel and tiles rules pick
//! them; the catalog editor links products to them. Implementation:
//! `modules::shop::admin::categories` (`*_in` with `Scope::Product`).

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    auth::Staff,
    error::AppResult,
    modules::shop::admin::categories::{
        self as c, AddProducts, CategoryInput, ProductOrder, TreeBody,
    },
    services::shop::categories::Scope,
    state::AppState,
};

const SCOPE: Scope = Scope::Product;

pub async fn list(State(state): State<AppState>, staff: Staff) -> AppResult<Json<Value>> {
    c::list_in(&state, &staff, SCOPE).await
}

pub async fn detail(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    c::detail_in(&state, &staff, id, SCOPE).await
}

pub async fn create(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<CategoryInput>,
) -> AppResult<(StatusCode, Json<Value>)> {
    c::create_in(&state, &staff, body, SCOPE).await
}

pub async fn update(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<CategoryInput>,
) -> AppResult<Json<Value>> {
    c::update_in(&state, &staff, id, body, SCOPE).await
}

pub async fn save_tree(
    State(state): State<AppState>,
    staff: Staff,
    Json(body): Json<TreeBody>,
) -> AppResult<Json<Value>> {
    c::save_tree_in(&state, &staff, body, SCOPE).await
}

pub async fn impact(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    c::impact_in(&state, &staff, id, SCOPE).await
}

pub async fn delete(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> AppResult<StatusCode> {
    c::delete_in(&state, &staff, id, &body, SCOPE).await
}

pub async fn products(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Value>> {
    c::products_in(&state, &staff, id, SCOPE).await
}

pub async fn order_products(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(items): Json<Vec<ProductOrder>>,
) -> AppResult<StatusCode> {
    c::order_products_in(&state, &staff, id, items, SCOPE).await
}

pub async fn add_products(
    State(state): State<AppState>,
    staff: Staff,
    Path(id): Path<Uuid>,
    Json(body): Json<AddProducts>,
) -> AppResult<Json<Value>> {
    c::add_products_in(&state, &staff, id, body, SCOPE).await
}

pub async fn remove_product(
    State(state): State<AppState>,
    staff: Staff,
    Path((id, product)): Path<(Uuid, Uuid)>,
) -> AppResult<StatusCode> {
    c::remove_product_in(&state, &staff, id, product, SCOPE).await
}

pub async fn create_suggested(
    State(state): State<AppState>,
    staff: Staff,
) -> AppResult<Json<Value>> {
    c::create_suggested_in(&state, &staff, SCOPE).await
}
