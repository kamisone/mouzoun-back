//! Online shop HTTP layer: public API (`/v1/shop`), back office (`/v1/admin/shop`), documents.
//! Business rules live in `crate::services::shop`. See docs/shop/README.md.

pub mod account;
pub mod admin;
pub mod documents;
pub mod public;

use axum::{
    Router,
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};

use crate::{error::AppError, state::AppState};

/// Cart and checkout exist only while `shop.status` is `open` (404 before anything else,
/// even before the body is read).
async fn require_open(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match public::shop_settings(&state).await {
        Ok(s) if s.is_open() => next.run(req).await,
        Ok(_) => AppError::NotFound.into_response(),
        Err(e) => e.into_response(),
    }
}

pub fn public_routes(state: &AppState) -> Router<AppState> {
    let buying = Router::new()
        .route(
            "/shop/cart",
            get(public::get_cart).post(public::create_cart),
        )
        .route("/shop/cart/items", post(public::add_item))
        .route(
            "/shop/cart/items/{id}",
            patch(public::update_item).delete(public::remove_item),
        )
        .route("/shop/cart/fulfillment", put(public::set_fulfillment))
        .route("/shop/cart/promotions", post(public::add_promotion))
        .route(
            "/shop/cart/promotions/{code}",
            delete(public::remove_promotion),
        )
        .route("/shop/cart/shipping-options", get(public::shipping_options))
        .route("/shop/checkout", post(public::checkout_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_open));
    Router::new()
        .merge(buying)
        .route("/shop/config", get(public::config))
        .route("/shop/relay-points", get(public::relay_points))
        .route("/shop/products", get(public::products))
        .route("/shop/products/by-slug/{slug}", get(public::product))
        .route("/shop/categories", get(public::categories))
        .route("/shop/categories/{slug}", get(public::category))
        .route("/shop/orders/{number}", get(public::get_order))
        .route("/shop/orders/{number}/status", get(public::order_status))
        .route(
            "/shop/orders/{number}/returns",
            post(public::request_return),
        )
        .route("/shop/orders/{number}/invoice", get(public::invoice))
        .route(
            "/shop/stock-notifications",
            post(public::stock_notification),
        )
        .route("/shop/notify-me", post(public::notify_me))
        .route(
            "/shop/payments/fake/{session}/{outcome}",
            post(public::fake_outcome),
        )
        .route("/shop/account/orders", get(account::orders))
        .route(
            "/shop/account/addresses",
            get(account::addresses).post(account::create_address),
        )
        .route(
            "/shop/account/addresses/{id}",
            put(account::update_address).delete(account::delete_address),
        )
}

pub fn admin_routes() -> Router<AppState> {
    Router::new().nest("/shop", admin::routes())
}
