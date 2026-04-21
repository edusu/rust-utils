//! Networking utilities.
//!
//! Currently ships a rate-limited HTTP [`Client`] built on top of
//! `reqwest`, governed by the `governor` crate (GCRA algorithm).

mod client;
mod rate_limit;

pub use client::{BuildError, Client, RateLimitedClient};
pub use rate_limit::RateLimitWindow;
