//! Guest accounts: sign-in by email code / magic link / password / social identity, rotating
//! sessions, profile, trips, wishlists, saved travellers, preferences, privacy. See
//! docs/accounts/README.md.

pub mod oidc;
pub mod passkeys;
pub mod passwords;
pub mod privacy;
pub mod sessions;
pub mod signin;
pub mod tickets;
pub mod tokens;
pub mod totp;
