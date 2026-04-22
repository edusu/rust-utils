//! Networking utilities.
//!
//! Currently ships a rate-limited HTTP [`Client`] built on top of
//! `reqwest`, governed by the `governor` crate (GCRA algorithm).

mod client;
mod rate_limit;
mod retry;

pub use client::{Client, RateLimitedClient};
pub use rate_limit::RateLimitWindow;
pub use retry::{HttpExecutor, RetryPolicy, RetryingClient};
