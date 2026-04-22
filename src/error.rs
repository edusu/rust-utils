//! Crate-wide error taxonomy built on `thiserror` + `error-stack`.
//!
//! Every fallible function in this crate returns [`UtilsResult<T>`],
//! which is `Result<T, Report<UtilsError>>`. [`UtilsError`] is a small,
//! coarse-grained classification of *what kind* of failure occurred;
//! the *why* (offending URL, underlying `reqwest::Error`, status code,
//! etc.) rides along as context attached to the [`Report`].
//!
//! # Why this shape
//! * **Coarse contexts, rich attachments.** Matching on a variant like
//!   [`UtilsError::Network`] is simple and stable; attachments carry the
//!   noisy details that callers usually only want when logging.
//! * **Stackable.** `error-stack` preserves the source chain, so
//!   `.change_context(UtilsError::Network)` keeps the original
//!   `reqwest::Error` reachable through the report.
//!
//! # Example
//! ```no_run
//! use error_stack::ResultExt;
//! use rust_utils::error::{UtilsError, UtilsResult};
//!
//! fn parse_port(raw: &str) -> UtilsResult<u16> {
//!     raw.parse::<u16>()
//!         .map_err(error_stack::Report::from)
//!         .change_context(UtilsError::Config)
//!         .attach_printable_lazy(|| format!("raw input: {raw:?}"))
//! }
//! ```

use error_stack::Report;
use thiserror::Error;

/// Coarse classification of every error surfaced by this crate.
///
/// Marked `#[non_exhaustive]` so new variants can be added without
/// breaking downstream `match` statements.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UtilsError {
    /// Transport-level failure reaching a remote endpoint (DNS, TLS,
    /// connect, read timeout, …).
    #[error("Network error")]
    Network,

    /// The remote endpoint completed the HTTP round-trip but returned
    /// a response that could not be turned into the expected domain
    /// type — non-2xx status, unexpected body, decode failure, etc.
    #[error("HTTP error")]
    Http,

    /// A constructor received an argument combination that is invalid
    /// by construction (e.g. a zero-length rate-limit period).
    #[error("Configuration error")]
    Config,

    /// A retry policy exhausted its budget without producing a success.
    #[error("Retry exhausted")]
    RetryExhausted,

    /// A concurrency primitive failed (e.g. a task panicked inside a
    /// pool, a semaphore closed unexpectedly).
    #[error("Concurrency error")]
    Concurrency,

    /// Catch-all for failures originating in a dependency that do not
    /// fit any of the categories above.
    #[error("Internal error")]
    Internal,
}

/// A [`Report`] whose root context is a [`UtilsError`].
pub type UtilsReport = Report<UtilsError>;

/// Shorthand for `Result<T, UtilsReport>`.
pub type UtilsResult<T> = Result<T, UtilsReport>;
