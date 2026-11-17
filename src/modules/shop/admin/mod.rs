//! Shop back office (`/v1/admin/shop/...`). Every route is behind `require_staff`; each handler
//! checks its permission and writes the audit log.
//!
//! | area | read | write |
//! |---|---|---|
//! | products, options, variants, variation library, media, categories, CSV | shop.read | shop.catalog |
//! | promotions, shipping, taxes | shop.read | shop.pricing |
//! | locations, inventory, lots, movements | shop.read | shop.stock |
//! | orders, drafts, fulfilments, payments recorded | shop.read + shop.orders | shop.orders |
//! | returns, refunds | shop.read | shop.refunds |
//! | settings, payment methods | shop.read | shop.settings |
//! | dashboard, reports | shop.read | — |

pub mod catalog;
pub mod categories;
pub mod france;
pub mod orders;
pub mod pricing;
pub mod reports;
pub mod settings;
pub mod stock;
pub mod variations;

use axum::{
    Router,
    routing::{get, patch, post, put},
};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(reports::dashboard))
        .route("/reports", get(reports::report))
        // Catalog
        .route("/products", get(catalog::list).post(catalog::create))
        .route("/products/bulk", post(catalog::bulk))
        .route("/products/export.csv", get(catalog::export_csv))
        .route("/products/import", post(catalog::import_csv))
        .route(
            "/products/{id}",
            get(catalog::detail)
                .put(catalog::update)
                .delete(catalog::delete),
        )
        .route("/products/{id}/duplicate", post(catalog::duplicate))
        .route("/products/{id}/options", put(catalog::set_options))
        .route(
            "/products/{id}/variants",
            post(catalog::create_variant).patch(catalog::bulk_variants),
        )
        .route(
            "/products/{id}/variants/generate",
            post(catalog::generate_variants),
        )
        .route("/variants/{id}/media", put(catalog::variant_media))
        .route(
            "/variations",
            get(variations::list).post(variations::create),
        )
        .route(
            "/variations/{id}",
            put(variations::update).delete(variations::delete),
        )
        .route(
            "/categories",
            get(categories::list).post(categories::create),
        )
        .route(
            "/categories/{id}",
            put(categories::update).delete(categories::delete),
        )
        // Pricing
        .route(
            "/promotions",
            get(pricing::promotions).post(pricing::create_promotion),
        )
        .route("/promotions/{id}", put(pricing::update_promotion))
        .route("/shipping", get(pricing::shipping))
        .route("/shipping/profiles", post(pricing::create_profile))
        .route("/shipping/profiles/{id}", put(pricing::update_profile))
        .route("/shipping/zones", post(pricing::create_zone))
        .route("/shipping/zones/{id}", put(pricing::update_zone))
        .route("/shipping/rates", post(pricing::create_rate))
        .route(
            "/shipping/rates/{id}",
            put(pricing::update_rate).delete(pricing::delete_rate),
        )
        .route("/taxes", get(pricing::taxes).post(pricing::create_tax))
        .route("/taxes/{id}", put(pricing::update_tax))
        // Stock
        .route(
            "/locations",
            get(stock::locations).post(stock::create_location),
        )
        .route("/locations/{id}", put(stock::update_location))
        .route("/inventory", get(stock::grid).post(stock::operate))
        .route("/inventory/export.csv", get(stock::export_csv))
        .route("/lots", get(stock::lots))
        .route("/movements", get(stock::movements))
        // Orders
        .route("/orders", get(orders::list))
        .route("/orders/drafts", post(orders::create_draft))
        .route(
            "/orders/{id}",
            get(orders::detail)
                .patch(orders::annotate)
                .delete(orders::delete_draft),
        )
        .route("/orders/{id}/confirm", post(orders::confirm))
        .route("/orders/{id}/edit", put(orders::edit))
        .route("/orders/{id}/finalize", post(orders::finalize))
        .route("/orders/{id}/payments", post(orders::record_payment))
        .route(
            "/orders/{id}/fulfillments",
            post(orders::create_fulfillment),
        )
        .route("/orders/{id}/cancel", post(orders::cancel))
        .route("/orders/{id}/refunds", post(orders::refund))
        .route("/orders/{id}/returns", post(orders::create_return))
        .route("/orders/{id}/resend", post(orders::resend))
        .route("/orders/{id}/invoice", get(orders::invoice))
        .route("/fulfillments/{id}", patch(orders::update_fulfillment))
        .route("/fulfillments/{id}/packing-slip", get(orders::packing_slip))
        .route("/returns", get(orders::returns))
        .route("/returns/{id}", patch(orders::update_return))
        .route("/customers/{id}/orders", get(categories::customer_orders))
        // Settings
        .route("/settings", get(settings::get).patch(settings::update))
        .route("/settings/stripe", get(settings::stripe))
        // French operation
        .route("/legal-entities", get(france::entities))
        .route("/legal-entities/{id}", put(france::update_entity))
        .route(
            "/legal-entities/{id}/invoice-preview",
            get(france::invoice_preview),
        )
        .route("/invoices/{id}/pdf", get(france::invoice_pdf))
        .route(
            "/tax-rates",
            get(france::tax_rates).post(|s, st, b| france::save_rate(s, st, None, b)),
        )
        .route(
            "/tax-rates/{id}",
            put(|s, st, p, b| france::save_rate(s, st, Some(p), b)),
        )
        .route("/compliance", get(france::compliance))
        .route("/withdrawals", get(france::withdrawals_due))
        .route("/exports/sales-journal.csv", get(france::sales_journal))
        .route("/exports/documents.csv", get(france::documents_export))
        .route("/exports/stripe.csv", get(france::stripe_export))
}
