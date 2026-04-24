use std::num::NonZeroU32;
use std::sync::Arc;

use error_stack::Report;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};
use reqwest::{Client as ReqwestClient, Request, Response};

use super::rate_limit::RateLimitWindow;
use crate::error::{UtilsError, UtilsResult};

/// Concrete type of the in-memory, direct, non-keyed limiter used here.
///
/// Pulled out as an alias because the four type parameters of
/// `RateLimiter` turn every signature into noise.
type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

/// HTTP client that wraps [`reqwest::Client`] and optionally paces
/// outgoing requests through an internal rate limiter.
///
/// The variant is chosen at construction time. [`Client`] is `Clone`
/// and cheap to pass around: the rate-limited variant holds its limiter
/// and its inner `reqwest::Client` behind `Arc`s, so all clones share
/// state and the limit applies globally across them.
#[derive(Debug, Clone)]
pub enum Client {
    /// A client that waits on the rate limiter before dispatching.
    RateLimited(RateLimitedClient),
    /// A plain `reqwest::Client` with no pacing.
    Unrestricted(ReqwestClient),
}

impl Client {
    /// Execute a prepared [`reqwest::Request`].
    ///
    /// For the [`Self::RateLimited`] variant this first awaits the
    /// limiter and only then dispatches the request.
    ///
    /// # Errors
    /// Returns a [`UtilsError::Network`] report for connect/timeout
    /// failures and [`UtilsError::Http`] for any other transport-level
    /// failure. The original `reqwest::Error` is attached to the
    /// report for inspection/logging.
    pub async fn execute(&self, req: Request) -> UtilsResult<Response> {
        match self {
            Client::RateLimited(c) => c.execute(req).await,
            Client::Unrestricted(c) => c.execute(req).await.map_err(into_network_or_http),
        }
    }

    /// Return the underlying [`reqwest::Client`].
    ///
    /// On the [`Self::RateLimited`] variant this **bypasses the rate
    /// limiter** — requests dispatched through the returned reference
    /// are not paced by the limiter attached to this handle.
    pub fn inner_client(&self) -> &ReqwestClient {
        match self {
            Client::RateLimited(c) => c.inner_client(),
            Client::Unrestricted(c) => c,
        }
    }
}

/// A [`reqwest::Client`] that paces outgoing requests through a
/// [`governor::RateLimiter`] based on the GCRA algorithm.
#[derive(Debug, Clone)]
pub struct RateLimitedClient {
    inner: ReqwestClient,
    limiter: Arc<DirectLimiter>,
}

impl RateLimitedClient {
    /// Build a rate-limited client on top of a freshly-created
    /// [`reqwest::Client`] with default settings.
    ///
    /// # Arguments
    /// * `window` — how often cells are replenished.
    /// * `burst`  — maximum bucket size. When `None`, `governor`'s
    ///   default for the window is used.
    ///
    /// # Errors
    /// Returns [`UtilsError::Config`] when `window` is
    /// [`RateLimitWindow::Custom`] with a zero-length duration.
    pub fn new(window: RateLimitWindow, burst: Option<NonZeroU32>) -> UtilsResult<Self> {
        Self::with_client(ReqwestClient::new(), window, burst)
    }

    /// Build a rate-limited client that wraps a user-provided
    /// [`reqwest::Client`].
    ///
    /// Use this when you need to configure timeouts, default headers,
    /// TLS settings, user-agent, proxies, etc. on the underlying client
    /// while still getting rate-limit behaviour on top.
    ///
    /// # Errors
    /// Same as [`Self::new`].
    pub fn with_client(
        inner: ReqwestClient,
        window: RateLimitWindow,
        burst: Option<NonZeroU32>,
    ) -> UtilsResult<Self> {
        // Translate the high-level window into a `governor::Quota`.
        // `with_period` is the only path that can fail (period == 0).
        let mut quota = match window {
            RateLimitWindow::PerSecond(allowed) => Quota::per_second(allowed),
            RateLimitWindow::PerMinute(allowed) => Quota::per_minute(allowed),
            RateLimitWindow::Custom { period } => Quota::with_period(period).ok_or_else(|| {
                Report::new(UtilsError::Config)
                    .attach_printable("rate limit period must be non-zero")
            })?,
        };
        if let Some(b) = burst {
            quota = quota.allow_burst(b);
        }
        Ok(Self {
            inner,
            limiter: Arc::new(RateLimiter::direct(quota)),
        })
    }

    /// Reference to the underlying [`reqwest::Client`].
    ///
    /// Requests dispatched directly through this handle bypass the rate
    /// limiter.
    pub fn inner_client(&self) -> &ReqwestClient {
        &self.inner
    }

    /// Wait until the rate limiter releases a slot, then return.
    pub async fn wait_for_slot(&self) {
        self.limiter.until_ready().await;
    }

    /// Await a slot from the limiter and then dispatch the request.
    ///
    /// # Errors
    /// Same classification as [`Client::execute`].
    pub async fn execute(&self, req: Request) -> UtilsResult<Response> {
        self.wait_for_slot().await;
        self.inner.execute(req).await.map_err(into_network_or_http)
    }
}

/// Classify a `reqwest::Error` into the crate's error taxonomy.
///
/// Connect/timeout failures are [`UtilsError::Network`]; everything
/// else produced by `reqwest::Client::execute` (builder, redirect,
/// body, decode) is classified as [`UtilsError::Http`]. The original
/// `reqwest::Error` is attached as the root context so the source
/// chain remains reachable via the resulting report.
pub(crate) fn into_network_or_http(e: reqwest::Error) -> Report<UtilsError> {
    let kind = if e.is_connect() || e.is_timeout() {
        UtilsError::Network
    } else {
        UtilsError::Http
    };
    Report::new(e).change_context(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A `PerSecond(2)` quota should admit the initial burst of 2 cells
    /// immediately and delay the 3rd acquisition by roughly 500ms
    /// (one cell every 1s/2).
    #[tokio::test]
    async fn wait_for_slot_respects_per_second_window() {
        let client = RateLimitedClient::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(2).unwrap()),
            None,
        )
        .expect("quota with a non-zero period must build");

        // Drain the initial burst.
        client.wait_for_slot().await;
        client.wait_for_slot().await;

        // Measure the wait for the next slot. We allow some slack
        // under the theoretical 500ms to stay robust on busy CI.
        let start = Instant::now();
        client.wait_for_slot().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(400),
            "expected >= 400ms wait after draining burst, got {elapsed:?}"
        );
    }

    /// A custom window with a zero duration is rejected at construction
    /// time rather than panicking inside `governor`.
    #[test]
    fn custom_zero_period_is_rejected() {
        let report = RateLimitedClient::new(
            RateLimitWindow::Custom {
                period: Duration::ZERO,
            },
            None,
        )
        .expect_err("zero-length period must be rejected");

        assert!(matches!(report.current_context(), UtilsError::Config));
    }

    /// Clones must share the same limiter: draining the burst on one
    /// clone forces the other to wait.
    #[tokio::test]
    async fn clones_share_the_limiter() {
        let a = RateLimitedClient::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(1).unwrap()),
            None,
        )
        .unwrap();
        let b = a.clone();

        a.wait_for_slot().await; // consume the only cell in the burst

        let start = Instant::now();
        b.wait_for_slot().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(800),
            "clone should have waited ~1s, got {elapsed:?}"
        );
    }
}
