//! Configuration types for [`NatsClient`](super::NatsClient).

use std::path::PathBuf;
use std::time::Duration;

/// User/password credentials for NATS authentication.
#[derive(Debug, Clone)]
pub struct NatsCredentials {
    /// NATS user.
    pub user: String,
    /// NATS password.
    pub password: String,
}

/// Connection-level options consumed by
/// [`NatsClient::connect`](super::NatsClient::connect).
///
/// Constructed via [`NatsConnectOptions::new`] and chained setters.
/// Fields are private so new options can be added without breaking
/// callers.
///
/// ```no_run
/// # use std::path::PathBuf;
/// # use std::time::Duration;
/// use rust_utils::network::nats::{NatsConnectOptions, NatsCredentials};
///
/// let opts = NatsConnectOptions::new("nats://localhost:4222")
///     .credentials(NatsCredentials {
///         user: "svc".into(),
///         password: "secret".into(),
///     })
///     .tls_root_certs(PathBuf::from("/etc/ssl/certs/ca.pem"))
///     .protocol_request_timeout(Duration::from_secs(10));
/// ```
#[derive(Debug, Clone)]
pub struct NatsConnectOptions {
    pub(crate) url: String,
    pub(crate) credentials: Option<NatsCredentials>,
    pub(crate) tls_root_certs: Option<PathBuf>,
    pub(crate) protocol_request_timeout: Duration,
}

impl NatsConnectOptions {
    /// Create a fresh set of options targeting `url`. The URL may
    /// include the `nats://` or `tls://` scheme; multiple hosts can
    /// be passed comma-separated per `async_nats`'s `ToServerAddrs`
    /// contract.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            credentials: None,
            tls_root_certs: None,
            protocol_request_timeout: Duration::from_secs(30),
        }
    }

    /// Attach user/password credentials.
    pub fn credentials(mut self, c: NatsCredentials) -> Self {
        self.credentials = Some(c);
        self
    }

    /// Enable TLS using a PEM file that contains root certificates
    /// for server verification. Sets `require_tls` on the underlying
    /// connection and ensures a rustls crypto provider is installed.
    pub fn tls_root_certs(mut self, path: PathBuf) -> Self {
        self.tls_root_certs = Some(path);
        self
    }

    /// Timeout applied to the *protocol-level* requests issued
    /// internally by `async_nats` while establishing and maintaining
    /// the connection.
    ///
    /// This is **not** the timeout for user-level [`NatsClient::request`](super::NatsClient::request)
    /// calls — those are bounded by the NATS server's own request
    /// semantics. Defaults to 30 seconds.
    pub fn protocol_request_timeout(mut self, d: Duration) -> Self {
        self.protocol_request_timeout = d;
        self
    }
}

/// Runtime configuration for a single subscription served by
/// [`NatsClient::serve_request_reply`](super::NatsClient::serve_request_reply).
///
/// Defaults: 16 concurrent handlers, 1 MiB per-message payload cap.
#[derive(Debug, Clone, Copy)]
pub struct SubscriptionConfig {
    /// Maximum number of messages processed concurrently. A value
    /// of `0` disables the cap (unbounded fan-out).
    pub max_concurrency: usize,
    /// Maximum accepted payload size in bytes. Larger messages are
    /// discarded (logged at `warn` level) so a single oversized
    /// message cannot OOM the worker.
    pub max_payload_bytes: usize,
}

impl SubscriptionConfig {
    /// Build a config with explicit values.
    pub const fn new(max_concurrency: usize, max_payload_bytes: usize) -> Self {
        Self {
            max_concurrency,
            max_payload_bytes,
        }
    }
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 16,
            max_payload_bytes: 1024 * 1024,
        }
    }
}
