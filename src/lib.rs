//! Collection of reusable structures and utilities shared across Rust
//! projects.
//!
//! Each sub-module is self-contained and can be used in isolation.

pub(crate) mod backoff;
pub mod concurrency;
pub mod error;
pub mod network;
pub mod secret;

pub use error::{UtilsError, UtilsReport, UtilsResult};
