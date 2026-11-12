//! Guest accounts API (`/v1/account/...`). Sign-in endpoints are public; everything else needs a
//! guest session (`GuestUser`, 401 otherwise). Booking pages keep their own endpoints
//! (`/v1/bookings/{reference}`), authorised by ownership for signed-in guests.

pub mod auth;
pub mod cards;
pub mod extras;
pub mod oauth;
pub mod passkeys;
pub mod privacy;
pub mod profile;

use axum::{
    Router,
    routing::{delete, get, post, put},
};

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/identify", post(auth::identify))
        .route("/auth/code", post(auth::verify_code))
        .route("/auth/link", post(auth::verify_link))
        .route("/auth/password", post(auth::password_login))
        .route("/auth/refresh", post(auth::refresh))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/logout-all", post(auth::logout_all))
        .route("/auth/reauth/code", post(auth::reauth_send))
        .route("/auth/reauth", post(auth::reauth))
        .route("/auth/passkey/options", post(passkeys::sign_in_options))
        .route("/auth/passkey", post(passkeys::sign_in))
        .route(
            "/auth/reauth/passkey/options",
            post(passkeys::reauth_options),
        )
        .route("/auth/reauth/passkey", post(passkeys::reauth))
        .route("/passkeys", get(passkeys::list).post(passkeys::register))
        .route("/passkeys/options", post(passkeys::registration_options))
        .route("/passkeys/{id}", delete(passkeys::remove))
        .route("/me", get(profile::me).patch(profile::update_me))
        .route("/trips", get(profile::trips))
        .route(
            "/notifications",
            get(profile::notifications).put(profile::set_notification),
        )
        .route("/sessions", get(profile::list_sessions))
        .route("/sessions/{id}", delete(profile::revoke_session))
        .route(
            "/password",
            post(profile::set_password).delete(profile::remove_password),
        )
        .route("/email", post(profile::request_email_change))
        .route("/email/confirm", post(profile::confirm_email_change))
        .route(
            "/wishlists",
            get(extras::wishlists).post(extras::create_wishlist),
        )
        .route(
            "/wishlists/{id}",
            axum::routing::patch(extras::rename_wishlist).delete(extras::delete_wishlist),
        )
        .route(
            "/wishlists/{id}/items/{product}",
            put(extras::put_item).delete(extras::delete_item),
        )
        .route(
            "/wishlists/{id}/share",
            post(extras::share).delete(extras::unshare),
        )
        .route("/wishlists/shared/{token}", get(extras::shared))
        .route("/saved", get(extras::saved_ids).post(extras::merge_saved))
        .route("/saved/{product}", put(extras::save).delete(extras::unsave))
        .route(
            "/travellers",
            get(extras::travellers).post(extras::create_traveller),
        )
        .route(
            "/travellers/{id}",
            put(extras::update_traveller).delete(extras::delete_traveller),
        )
        .route("/travellers/{id}/document", post(extras::reveal_document))
        .route("/oauth/providers", get(oauth::providers))
        .route("/oauth/{provider}/start", get(oauth::start))
        .route(
            "/oauth/{provider}/callback",
            get(oauth::callback_get).post(oauth::callback_post),
        )
        .route("/identities/{provider}", delete(oauth::unlink))
        .route("/payment-methods", get(cards::list))
        .route("/payment-methods/setup", post(cards::setup))
        .route("/payment-methods/{id}", delete(cards::remove))
        .route(
            "/privacy/export",
            get(privacy::download_export).post(privacy::request_export),
        )
        .route(
            "/privacy/deletion",
            get(privacy::deletion_status)
                .post(privacy::request_deletion)
                .delete(privacy::cancel_deletion),
        )
        .route(
            "/privacy/deletion/cancel",
            post(privacy::cancel_deletion_by_link),
        )
        .route("/trips/{reference}/ticket", get(extras::ticket))
        .route("/trips/claim", post(extras::claim))
        .route("/trips/claim/confirm", post(extras::confirm_claim))
}
