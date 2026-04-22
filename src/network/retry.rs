//! Retry layer for HTTP requests.
//!
//! Exposes [`HttpExecutor`] — the abstraction over anything that can
//! send a [`reqwest::Request`] — and [`RetryingClient`], a wrapper that
//! applies a [`RetryPolicy`] to any executor.
//!
//! # Rationale
//! Rate limiting is *proactive* (pace requests going out); retry-on-429
//! is *reactive* (inspect responses coming back). Keeping them
//! orthogonal lets callers pick the combination they need. Typical
//! stacking order is `Retry` → `RateLimit` → `reqwest::Client` so that
//! every retry also costs a cell in the internal rate limiter.
//!
//! # Error surface
//! Both the trait and the wrapper return [`UtilsResult<Response>`]. The
//! retry loop matches on the current context of the attached report
//! ([`UtilsError::Network`] for transport failures, anything else is
//! treated as a non-retryable HTTP failure) instead of poking at
//! `reqwest::Error` directly.

use std::future::Future;
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::RETRY_AFTER;
use reqwest::{Method, Request, Response, StatusCode};

use super::client::{Client, RateLimitedClient, into_network_or_http};
use crate::error::{UtilsError, UtilsResult};

/// Abstraction over anything capable of executing a
/// [`reqwest::Request`].
///
/// Implemented for [`reqwest::Client`], the crate's [`Client`] enum,
/// [`RateLimitedClient`], and [`RetryingClient`] (so wrappers can be
/// stacked). Implement it on your own types in tests or to plug in
/// additional middleware layers.
///
/// The returned future is required to be `Send` so implementations can
/// be used on multi-threaded async runtimes.
pub trait HttpExecutor: Send + Sync {
    /// Send the request and await its response.
    fn execute(&self, req: Request) -> impl Future<Output = UtilsResult<Response>> + Send;
}

impl HttpExecutor for reqwest::Client {
    async fn execute(&self, req: Request) -> UtilsResult<Response> {
        reqwest::Client::execute(self, req)
            .await
            .map_err(into_network_or_http)
    }
}

impl HttpExecutor for RateLimitedClient {
    fn execute(&self, req: Request) -> impl Future<Output = UtilsResult<Response>> + Send {
        RateLimitedClient::execute(self, req)
    }
}

impl HttpExecutor for Client {
    fn execute(&self, req: Request) -> impl Future<Output = UtilsResult<Response>> + Send {
        Client::execute(self, req)
    }
}

/// Declarative configuration of retry behaviour.
///
/// Build with [`RetryPolicy::default`] and chain setters. Fields are
/// private so additions in future versions do not break callers.
///
/// # Defaults
/// * `max_attempts` = 3 (initial attempt + up to 2 retries)
/// * Retries on HTTP `429` and `503`.
/// * Retries on connect/timeout network errors.
/// * Only retries idempotent methods (`GET`/`HEAD`/`PUT`/`DELETE`/`OPTIONS`/`TRACE`).
/// * Exponential backoff: base 200ms, cap 30s, full jitter enabled.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    max_attempts: NonZeroU32,
    retry_statuses: Vec<StatusCode>,
    retry_network_errors: bool,
    retry_non_idempotent: bool,
    base_backoff: Duration,
    max_backoff: Duration,
    jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            // SAFETY: literal 3 is non-zero.
            max_attempts: NonZeroU32::new(3).expect("3 is non-zero"),
            retry_statuses: vec![
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::SERVICE_UNAVAILABLE,
            ],
            retry_network_errors: true,
            retry_non_idempotent: false,
            base_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(30),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Set the total number of attempts (initial + retries).
    pub fn max_attempts(mut self, n: NonZeroU32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Replace the set of response statuses that trigger a retry.
    pub fn retry_statuses<I: IntoIterator<Item = StatusCode>>(mut self, statuses: I) -> Self {
        self.retry_statuses = statuses.into_iter().collect();
        self
    }

    /// Toggle whether connect/timeout network errors trigger a retry.
    pub fn retry_network_errors(mut self, v: bool) -> Self {
        self.retry_network_errors = v;
        self
    }

    /// Opt in to retrying non-idempotent methods (`POST` / `PATCH`).
    ///
    /// Disabled by default because retrying a non-idempotent request
    /// can cause duplicate side effects server-side (e.g. two orders
    /// placed). Only enable when the server API provides de-duplication
    /// guarantees (idempotency keys, request IDs, etc.).
    pub fn retry_non_idempotent(mut self, v: bool) -> Self {
        self.retry_non_idempotent = v;
        self
    }

    /// Base duration for the exponential backoff: `base * 2^(attempt-1)`.
    pub fn base_backoff(mut self, d: Duration) -> Self {
        self.base_backoff = d;
        self
    }

    /// Cap on the backoff between attempts. Also applied to
    /// `Retry-After` values when the server requests a longer wait.
    pub fn max_backoff(mut self, d: Duration) -> Self {
        self.max_backoff = d;
        self
    }

    /// Enable or disable full jitter on the computed backoff.
    ///
    /// Full jitter means each delay is drawn uniformly from
    /// `[0, computed]`, which avoids thundering-herd synchronization
    /// across distributed clients. Does **not** apply to server-sent
    /// `Retry-After` values (those are server-authoritative).
    pub fn jitter(mut self, v: bool) -> Self {
        self.jitter = v;
        self
    }
}

/// Wrapper around any [`HttpExecutor`] that retries failed requests
/// according to a [`RetryPolicy`].
///
/// Retries replay the original request via [`Request::try_clone`]; if
/// the request body cannot be cloned (e.g. a streaming upload), no
/// retry is attempted and the first observed result is returned.
#[derive(Debug, Clone)]
pub struct RetryingClient<E> {
    inner: E,
    policy: RetryPolicy,
}

impl<E: HttpExecutor> RetryingClient<E> {
    /// Wrap `inner` with the given retry `policy`.
    pub fn new(inner: E, policy: RetryPolicy) -> Self {
        Self { inner, policy }
    }

    /// Reference to the wrapped executor.
    pub fn inner(&self) -> &E {
        &self.inner
    }

    /// Reference to the active retry policy.
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Execute `req`, retrying according to the configured policy.
    ///
    /// Always returns the last observed outcome — either the final
    /// retry's response/error, or the first non-retryable result.
    /// A retryable status code that still exhausts `max_attempts`
    /// comes back as `Ok(response)`, not as an error; inspect
    /// [`Response::status`] to decide what to do.
    pub async fn execute(&self, req: Request) -> UtilsResult<Response> {
        // Template cloned once, re-cloned per retry. `None` means the
        // body is not replayable (e.g. streaming), so we won't retry.
        let template = req.try_clone();
        let method = req.method().clone();
        let mut last = self.inner.execute(req).await;

        for attempt in 1..self.policy.max_attempts.get() {
            if !self.should_retry(&last, &method) {
                break;
            }
            let Some(retry_req) = template.as_ref().and_then(|r| r.try_clone()) else {
                // Body not cloneable — give up on further retries.
                break;
            };
            let delay = self.compute_delay(attempt, &last);
            tokio::time::sleep(delay).await;
            last = self.inner.execute(retry_req).await;
        }

        last
    }

    /// Whether the last result qualifies for another attempt.
    fn should_retry(&self, result: &UtilsResult<Response>, method: &Method) -> bool {
        if !self.policy.retry_non_idempotent && !is_idempotent(method) {
            return false;
        }
        match result {
            Ok(resp) => self.policy.retry_statuses.contains(&resp.status()),
            Err(report) => {
                self.policy.retry_network_errors
                    && matches!(report.current_context(), UtilsError::Network)
            }
        }
    }

    /// How long to wait before the next attempt.
    ///
    /// A server-sent `Retry-After` header takes precedence (capped by
    /// `max_backoff`). Otherwise exponential backoff is used, with
    /// full jitter when enabled.
    fn compute_delay(&self, attempt: u32, result: &UtilsResult<Response>) -> Duration {
        if let Ok(resp) = result
            && let Some(server_delay) = parse_retry_after(resp)
        {
            return server_delay.min(self.policy.max_backoff);
        }
        let factor = 2u32.saturating_pow(attempt - 1);
        let uncapped = self.policy.base_backoff.saturating_mul(factor);
        let capped = uncapped.min(self.policy.max_backoff);
        if self.policy.jitter {
            full_jitter(capped)
        } else {
            capped
        }
    }
}

impl<E: HttpExecutor> HttpExecutor for RetryingClient<E> {
    fn execute(&self, req: Request) -> impl Future<Output = UtilsResult<Response>> + Send {
        RetryingClient::execute(self, req)
    }
}

/// Methods considered idempotent per RFC 7231 §4.2.2.
fn is_idempotent(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::HEAD | Method::PUT | Method::DELETE | Method::OPTIONS | Method::TRACE
    )
}

/// Parse the `Retry-After` header as an integer number of seconds.
///
/// The HTTP-date form defined by RFC 7231 is **not** supported;
/// rate-limited APIs in practice favour the integer-seconds form.
fn parse_retry_after(resp: &Response) -> Option<Duration> {
    let raw = resp.headers().get(RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    Some(Duration::from_secs(secs))
}

/// Draw a duration uniformly from `[0, capped]` using the current
/// time's nanoseconds as a cheap entropy source.
///
/// Not cryptographically random — adequate for backoff jitter.
fn full_jitter(capped: Duration) -> Duration {
    let capped_nanos = u64::try_from(capped.as_nanos()).unwrap_or(u64::MAX);
    if capped_nanos == 0 {
        return Duration::ZERO;
    }
    let source = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Duration::from_nanos(source % capped_nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use error_stack::Report;
    use std::sync::Mutex;

    /// Scripted response kinds consumed by `MockExecutor`.
    enum ResponseKind {
        Status(StatusCode),
        WithRetryAfter { status: StatusCode, seconds: u64 },
        NetworkError,
    }

    /// Test-only executor that returns responses from a pre-loaded queue
    /// and counts how many times it is invoked.
    struct MockExecutor {
        queue: Mutex<Vec<ResponseKind>>,
        calls: Mutex<u32>,
    }

    impl MockExecutor {
        fn new(queue: Vec<ResponseKind>) -> Self {
            Self {
                queue: Mutex::new(queue),
                calls: Mutex::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl HttpExecutor for MockExecutor {
        fn execute(&self, _req: Request) -> impl Future<Output = UtilsResult<Response>> + Send {
            // Pop the next scripted response synchronously so the lock
            // never crosses an await point.
            let kind = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                let mut queue = self.queue.lock().unwrap();
                queue
                    .drain(..1)
                    .next()
                    .expect("MockExecutor ran out of scripted responses")
            };

            async move {
                match kind {
                    ResponseKind::NetworkError => Err(Report::new(UtilsError::Network)
                        .attach_printable("scripted network failure")),
                    other => {
                        let (status, retry_after) = match other {
                            ResponseKind::Status(s) => (s, None),
                            ResponseKind::WithRetryAfter { status, seconds } => {
                                (status, Some(seconds))
                            }
                            ResponseKind::NetworkError => unreachable!(),
                        };
                        let mut builder = http::Response::builder().status(status);
                        if let Some(seconds) = retry_after {
                            builder = builder.header("retry-after", seconds.to_string());
                        }
                        let http_resp = builder
                            .body(Vec::<u8>::new())
                            .expect("static response must build");
                        Ok(Response::from(http_resp))
                    }
                }
            }
        }
    }

    fn req(method: Method, url: &str) -> Request {
        Request::new(method, url.parse().expect("valid URL"))
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy::default()
            .base_backoff(Duration::from_millis(1))
            .max_backoff(Duration::from_millis(10))
            .jitter(false)
    }

    #[tokio::test]
    async fn retries_until_success_on_429() {
        let mock = MockExecutor::new(vec![
            ResponseKind::Status(StatusCode::TOO_MANY_REQUESTS),
            ResponseKind::Status(StatusCode::TOO_MANY_REQUESTS),
            ResponseKind::Status(StatusCode::OK),
        ]);
        let client = RetryingClient::new(
            mock,
            fast_policy().max_attempts(NonZeroU32::new(3).unwrap()),
        );
        let resp = client.execute(req(Method::GET, "http://x/")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(client.inner().call_count(), 3);
    }

    #[tokio::test]
    async fn returns_last_response_when_attempts_exhausted() {
        let mock = MockExecutor::new(vec![
            ResponseKind::Status(StatusCode::TOO_MANY_REQUESTS),
            ResponseKind::Status(StatusCode::TOO_MANY_REQUESTS),
        ]);
        let client = RetryingClient::new(
            mock,
            fast_policy().max_attempts(NonZeroU32::new(2).unwrap()),
        );
        let resp = client.execute(req(Method::GET, "http://x/")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(client.inner().call_count(), 2);
    }

    #[tokio::test]
    async fn respects_retry_after_header() {
        let mock = MockExecutor::new(vec![
            ResponseKind::WithRetryAfter {
                status: StatusCode::TOO_MANY_REQUESTS,
                seconds: 1,
            },
            ResponseKind::Status(StatusCode::OK),
        ]);
        let client = RetryingClient::new(
            mock,
            RetryPolicy::default()
                .max_attempts(NonZeroU32::new(2).unwrap())
                .base_backoff(Duration::from_millis(1))
                .max_backoff(Duration::from_secs(5))
                .jitter(false),
        );
        let start = std::time::Instant::now();
        let resp = client.execute(req(Method::GET, "http://x/")).await.unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status(), StatusCode::OK);
        // Retry-After: 1 → expect ~1s wait; allow generous slack.
        assert!(
            elapsed >= Duration::from_millis(900),
            "expected >= 900ms wait from Retry-After, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn does_not_retry_non_idempotent_by_default() {
        let mock = MockExecutor::new(vec![ResponseKind::Status(StatusCode::TOO_MANY_REQUESTS)]);
        let client = RetryingClient::new(
            mock,
            fast_policy().max_attempts(NonZeroU32::new(3).unwrap()),
        );
        let resp = client
            .execute(req(Method::POST, "http://x/"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(client.inner().call_count(), 1);
    }

    #[tokio::test]
    async fn retries_non_idempotent_when_opted_in() {
        let mock = MockExecutor::new(vec![
            ResponseKind::Status(StatusCode::TOO_MANY_REQUESTS),
            ResponseKind::Status(StatusCode::OK),
        ]);
        let client = RetryingClient::new(
            mock,
            fast_policy()
                .max_attempts(NonZeroU32::new(3).unwrap())
                .retry_non_idempotent(true),
        );
        let resp = client
            .execute(req(Method::POST, "http://x/"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(client.inner().call_count(), 2);
    }

    #[tokio::test]
    async fn ignores_statuses_outside_the_retry_set() {
        let mock = MockExecutor::new(vec![ResponseKind::Status(
            StatusCode::INTERNAL_SERVER_ERROR,
        )]);
        let client = RetryingClient::new(
            mock,
            fast_policy().max_attempts(NonZeroU32::new(3).unwrap()),
        );
        let resp = client.execute(req(Method::GET, "http://x/")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(client.inner().call_count(), 1);
    }

    /// A scripted `UtilsError::Network` should be retried until the
    /// policy either succeeds or exhausts its budget.
    #[tokio::test]
    async fn retries_on_network_error_context() {
        let mock = MockExecutor::new(vec![
            ResponseKind::NetworkError,
            ResponseKind::Status(StatusCode::OK),
        ]);
        let client = RetryingClient::new(
            mock,
            fast_policy().max_attempts(NonZeroU32::new(3).unwrap()),
        );
        let resp = client.execute(req(Method::GET, "http://x/")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(client.inner().call_count(), 2);
    }

    #[test]
    fn idempotency_classification_matches_rfc_7231() {
        for m in [
            Method::GET,
            Method::HEAD,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
            Method::TRACE,
        ] {
            assert!(is_idempotent(&m), "expected {m} to be idempotent");
        }
        for m in [Method::POST, Method::PATCH] {
            assert!(!is_idempotent(&m), "expected {m} to be non-idempotent");
        }
    }
}
