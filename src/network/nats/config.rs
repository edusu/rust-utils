//! Configuration types for [`NatsClient`](super::NatsClient).

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use crate::secret::Secret;

/// User/password credentials for NATS authentication.
///
/// The password is wrapped in [`Secret`] so it does not leak through
/// `Debug`/`Display` on enclosing error reports or traces.
#[derive(Debug, Clone)]
pub struct NatsCredentials {
    /// NATS user.
    pub user: String,
    /// NATS password, redacted in `Debug`/`Display`.
    pub password: Secret<String>,
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
/// use rust_utils::secret::Secret;
///
/// let opts = NatsConnectOptions::new("nats://localhost:4222")
///     .credentials(NatsCredentials {
///         user: "svc".into(),
///         password: Secret::new("secret".into()),
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
/// Constructed via [`SubscriptionConfig::new`] (or [`Default`]) and
/// chained setters. Fields are private so new options can be added
/// without breaking callers.
///
/// Defaults: 16 concurrent handlers, 1 MiB per-message payload cap,
/// no queue group (plain fan-out subscribe).
///
/// # Queue groups (load-balancing)
/// When [`queue_group`](Self::queue_group) is `Some(name)`, the
/// subscription joins the named **queue group** on the NATS server:
/// every message published to the subject is delivered to **exactly
/// one** member of the group, chosen by the server. Multiple
/// subscribers sharing the same group therefore load-balance work;
/// subscribers **outside** the group (including other groups on the
/// same subject) still receive their own copy independently.
///
/// Queue groups are a subscriber-side concern only — publishers are
/// unaware of whether their messages are fanned out or balanced.
#[derive(Debug, Clone)]
pub struct SubscriptionConfig {
    pub(crate) max_concurrency: Option<NonZeroUsize>,
    pub(crate) max_payload_bytes: usize,
    pub(crate) queue_group: Option<String>,
}

impl SubscriptionConfig {
    /// Build a config with explicit concurrency / payload values and
    /// no queue group.
    ///
    /// # Arguments
    /// * `max_concurrency` — `Some(n)` caps in-flight handlers at `n`;
    ///   `None` leaves fan-out unbounded.
    /// * `max_payload_bytes` — reject messages larger than this.
    pub const fn new(max_concurrency: Option<NonZeroUsize>, max_payload_bytes: usize) -> Self {
        Self {
            max_concurrency,
            max_payload_bytes,
            queue_group: None,
        }
    }

    /// Cap the maximum number of messages processed concurrently.
    /// Pass `None` for unbounded fan-out.
    pub fn max_concurrency(mut self, cap: Option<NonZeroUsize>) -> Self {
        self.max_concurrency = cap;
        self
    }

    /// Replace the per-message payload cap.
    ///
    /// Messages above this size are dropped (logged at `warn`) so a
    /// single oversized message cannot OOM the worker.
    pub fn max_payload_bytes(mut self, bytes: usize) -> Self {
        self.max_payload_bytes = bytes;
        self
    }

    /// Join the named queue group so messages delivered to the
    /// subject are load-balanced across all subscribers that share
    /// this group name (see the type-level docs for the exact
    /// semantics).
    pub fn queue_group(mut self, name: impl Into<String>) -> Self {
        self.queue_group = Some(name.into());
        self
    }
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            max_concurrency: NonZeroUsize::new(16),
            max_payload_bytes: 1024 * 1024,
            queue_group: None,
        }
    }
}
