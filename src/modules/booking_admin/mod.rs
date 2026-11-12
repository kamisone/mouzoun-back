//! Back-office booking API (`/v1/admin/...`, staff only, then one permission per endpoint):
//!
//! | endpoint | permission |
//! |---|---|
//! | GET dashboard/today | dashboard.read |
//! | GET booking/go-live (checklist), PUT booking/enabled (refused while a required check fails) | dashboard.read / settings.write |
//! | GET bookings, GET bookings/{id}, GET bookings/export.csv | bookings.read |
//! | POST quote, POST bookings (manual), PATCH bookings/{id}, POST …/accept, …/decline, …/check-in, …/check-out, …/no-show, …/notes, …/resend, POST …/payment-link | bookings.write |
//! | POST …/cancel, POST …/items/cancel | bookings.cancel |
//! | price override / manual discount on a manual booking | bookings.override_price |
//! | POST …/payments (cash, transfer, card terminal) | payments.record_offline |
//! | POST …/refunds | payments.refund |
//! | GET …/guests (numbers revealed with guests.read_identity), PUT …/guests | bookings.read / bookings.write |
//! | GET customers | customers.read |
//! | GET/PATCH tasks | bookings.read / bookings.write |
//! | crm/... | see `customers.rs` |

mod bookings;
mod customers;
mod dashboard;
mod go_live;

use axum::{
    Router,
    routing::{get, patch, post, put},
};

use crate::state::AppState;

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/dashboard/today", get(dashboard::today))
        .route("/booking/go-live", get(go_live::checklist))
        .route("/booking/enabled", put(go_live::switch))
        .route("/quote", post(bookings::quote))
        .route("/customers", get(bookings::customers))
        .route("/bookings", get(bookings::list).post(bookings::create))
        .route("/bookings/export.csv", get(bookings::export))
        .route(
            "/bookings/{id}",
            get(bookings::detail).patch(bookings::update),
        )
        .route("/bookings/{id}/accept", post(bookings::accept))
        .route("/bookings/{id}/decline", post(bookings::decline))
        .route("/bookings/{id}/status", post(bookings::set_status))
        .route("/bookings/{id}/cancel", post(bookings::cancel))
        .route("/bookings/{id}/items/cancel", post(bookings::cancel_items))
        .route("/bookings/{id}/notes", post(bookings::add_note))
        .route("/bookings/{id}/payments", post(bookings::record_payment))
        .route("/bookings/{id}/refunds", post(bookings::refund))
        .route("/bookings/{id}/resend", post(bookings::resend))
        .route(
            "/bookings/{id}/guests",
            get(bookings::guests).put(bookings::save_guests),
        )
        .route("/crm/customers", get(customers::list))
        .route("/crm/customers/{id}", get(customers::detail))
        .route(
            "/crm/customers/{id}/sign-in-link",
            post(customers::send_sign_in_link),
        )
        .route("/crm/customers/{id}/suspend", post(customers::suspend))
        .route(
            "/crm/customers/{id}/reactivate",
            post(customers::reactivate),
        )
        .route(
            "/crm/customers/{id}/sign-out-all",
            post(customers::sign_out_all),
        )
        .route("/crm/customers/{id}/merge", post(customers::merge))
        .route("/crm/customers/{id}/view-as", get(customers::view_as))
        .route("/crm/customers/{id}/export", get(customers::export))
        .route(
            "/crm/customers/{id}/deletion",
            post(customers::schedule_deletion),
        )
        .route("/crm/bookings/{id}/attach", post(customers::attach_booking))
        .route("/crm/reports/accounts", get(customers::report))
        .route("/tickets/scan", post(customers::scan_ticket))
        .route("/tasks", get(bookings::tasks))
        .route("/tasks/{id}", patch(bookings::update_task))
}
