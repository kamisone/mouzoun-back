//! Booking flows. Every flow is one database transaction that locks the inventory rows it
//! changes (in a consistent order), so concurrent bookings serialise instead of overselling.

mod accommodation;
mod activity;
mod common;
mod error;
mod lifecycle;
pub mod settings;
pub mod state;
pub mod trip;

pub use accommodation::{AccommodationRequest, book_accommodation};
pub use activity::{ActivityRequest, book_activity};
pub use common::{BookingCreated, BookingRequestMeta, CustomerInput, CustomerRef};
pub use error::BookingError;
pub use lifecycle::{Cancellation, cancel_booking, confirm_booking, expire_holds};
