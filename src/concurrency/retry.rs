//! Generic retry combinator for fallible async operations.
//!
//! [`Retry`] runs a closure that produces a `UtilsResult<T>` up to a
//! configured number of times, sleeping with exponential backoff and
//! optional full jitter between attempts. Unlike the HTTP-specific
//! [`RetryingClient`](crate::network::RetryingClient) and the long-running
//! [`Supervisor`](super::Supervisor) — both of which model very specific
//! restart semantics — this combinator is the bare "try this thing N
//! times" primitive that arbitrary call sites need.
//!
//! # Cooperation with shutdown
//! An optional [`CancellationToken`] makes the backoff sleeps
//! interruptible: a cancel during a backoff window aborts the loop and
//! surfaces the last observed error as
//! [`UtilsError::RetryExhausted`] with a "cancelled during backoff"
//! attachment.
//!
//! # Error semantics
//! * The closure returns `UtilsResult<T>`; the predicate decides which
//!   reports are worth retrying.
//! * A non-retryable error (predicate returns `false`) is returned
//!   *unchanged* so callers can match on its existing context.
//! * When the budget is exhausted on a retryable error, the report's
//!   root context is rotated to [`UtilsError::RetryExhausted`] via
//!   `change_context` — the original cause stays reachable through the
//!   report's frame chain.
//!
//! # Example
//! ```no_run
//! use std::num::NonZeroU32;
//! use std::time::Duration;
//! use rust_utils::concurrency::Retry;
//! use rust_utils::error::{UtilsError, UtilsResult};
//! use error_stack::Report;
//!
//! # async fn fetch_state() -> UtilsResult<u32> {
//! #     Ok(0)
//! # }
//! # async fn run() -> UtilsResult<()> {
//! let value: u32 = Retry::new()
//!     .max_attempts(NonZeroU32::new(5).unwrap())
//!     .base_backoff(Duration::from_millis(100))
//!     .max_backoff(Duration::from_secs(5))
//!     .run(|_attempt| async { fetch_state().await })
//!     .await?;
//! # let _ = value;
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use error_stack::Report;
use tokio_util::sync::CancellationToken;

use crate::backoff::{compute_delay, sleep_or_cancel};
use crate::error::{UtilsError, UtilsReport, UtilsResult};

/// Builder + driver for the generic retry combinator.
///
/// Construct with [`Retry::new`], tune with the fluent setters, then
/// drive with [`Retry::run`] or [`Retry::run_if`]. The builder is
/// `Clone`, so a single tuned policy can be shared across many call
/// sites.
///
/// # Defaults
/// * `max_attempts` = 3 (initial attempt + up to 2 retries)
/// * `base_backoff` = 200ms, `max_backoff` = 30s
/// * Full jitter enabled
/// * No cancellation token, no name
#[derive(Debug, Clone)]
pub struct Retry {
    max_attempts: NonZeroU32,
    base_backoff: Duration,
    max_backoff: Duration,
    jitter: bool,
    cancel: Option<CancellationToken>,
    name: Option<String>,
}

impl Retry {
    /// Build a new policy with the documented defaults.
    pub fn new() -> Self {
        Self {
            // SAFETY: literal 3 is non-zero.
            max_attempts: NonZeroU32::new(3).expect("3 is non-zero"),
            base_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(30),
            jitter: true,
            cancel: None,
            name: None,
        }
    }

    /// Total number of attempts the closure is allowed to make
    /// (initial call + retries).
    pub fn max_attempts(mut self, n: NonZeroU32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Base duration for the exponential backoff:
    /// `base * 2^(attempt - 1)`, capped at [`Self::max_backoff`].
    pub fn base_backoff(mut self, d: Duration) -> Self {
        self.base_backoff = d;
        self
    }

    /// Cap on the backoff between attempts.
    pub fn max_backoff(mut self, d: Duration) -> Self {
        self.max_backoff = d;
        self
    }

    /// Enable or disable full jitter on the computed backoff.
    ///
    /// Full jitter draws each delay uniformly from `[0, computed]`,
    /// which desynchronises crash-looping replicas. Default `true`.
    pub fn jitter(mut self, v: bool) -> Self {
        self.jitter = v;
        self
    }

    /// Attach a cancellation token so backoff sleeps observe shutdown.
    ///
    /// When the token fires during a backoff window the retry loop
    /// stops and surfaces the last observed error wrapped in
    /// [`UtilsError::RetryExhausted`].
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Optional label included in `tracing` events emitted on retry.
    /// Helps disambiguate concurrent retry loops.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Run `op` until it succeeds, the budget is exhausted, or the
    /// cancellation token (if any) fires.
    ///
    /// Equivalent to [`Retry::run_if`] with a predicate that always
    /// returns `true`: every error is treated as retryable.
    ///
    /// # Arguments
    /// * `op` — closure invoked once per attempt with the 1-based
    ///   attempt counter. Must be `FnMut` so it can be called
    ///   repeatedly; the produced future is awaited inline.
    ///
    /// # Errors
    /// * The original report when the closure returns a non-retryable
    ///   error (only possible via [`Retry::run_if`]).
    /// * [`UtilsError::RetryExhausted`] when the configured budget is
    ///   exhausted or a cancellation aborts the loop. The last cause
    ///   is preserved through `change_context`.
    pub async fn run<F, Fut, T>(&self, op: F) -> UtilsResult<T>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = UtilsResult<T>>,
    {
        self.run_if(op, |_| true).await
    }

    /// Run `op` with a custom retry predicate.
    ///
    /// `should_retry` is consulted on every error and decides whether
    /// the loop should attempt again or surface the report immediately.
    /// Returning `false` short-circuits the loop and returns the report
    /// unchanged — useful when classifying errors (e.g. "retry
    /// `UtilsError::Network`, give up on `UtilsError::Config`").
    ///
    /// # Arguments
    /// * `op` — see [`Retry::run`].
    /// * `should_retry` — predicate over the report's current context.
    ///   Called once per failed attempt.
    pub async fn run_if<F, Fut, T, P>(&self, mut op: F, mut should_retry: P) -> UtilsResult<T>
    where
        F: FnMut(u32) -> Fut,
        Fut: Future<Output = UtilsResult<T>>,
        P: FnMut(&UtilsReport) -> bool,
    {
        let max = self.max_attempts.get();
        let label: &str = self.name.as_deref().unwrap_or("retry");

        // Pre-loop cancellation: if shutdown already fired, skip even
        // the first call. The closure is expected to potentially do
        // real work (open sockets, hit databases, …); honour the
        // cancellation contract before launching it.
        if self.cancelled() {
            return Err(Report::new(UtilsError::RetryExhausted)
                .attach(format!("retry [{label}] cancelled before first attempt")));
        }

        // Attempts 1..max-1: a failure here may schedule another
        // attempt after a backoff sleep.
        for attempt in 1..max {
            match op(attempt).await {
                Ok(value) => return Ok(value),
                Err(report) => {
                    if !should_retry(&report) {
                        return Err(report);
                    }
                    tracing::debug!(retry = label, attempt, "operation failed; scheduling retry");
                    let delay =
                        compute_delay(self.base_backoff, self.max_backoff, attempt, self.jitter);
                    if !sleep_or_cancel(self.cancel.as_ref(), delay).await {
                        return Err(report.change_context(UtilsError::RetryExhausted).attach(
                            format!(
                                "retry [{label}] cancelled during backoff after attempt {attempt}"
                            ),
                        ));
                    }
                }
            }
        }

        // Final attempt: success returns Ok; a retryable failure means
        // the budget is exhausted and we wrap the cause; a
        // non-retryable failure is returned as-is.
        match op(max).await {
            Ok(value) => Ok(value),
            Err(report) => {
                if should_retry(&report) {
                    Err(report
                        .change_context(UtilsError::RetryExhausted)
                        .attach(format!(
                            "retry [{label}] exhausted budget after {max} attempts"
                        )))
                } else {
                    Err(report)
                }
            }
        }
    }

    /// Whether the attached cancellation token (if any) has fired.
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .map(CancellationToken::is_cancelled)
            .unwrap_or(false)
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use error_stack::Report;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn fast_policy() -> Retry {
        Retry::new()
            .base_backoff(Duration::from_millis(1))
            .max_backoff(Duration::from_millis(5))
            .jitter(false)
    }

    /// A successful first attempt skips the retry loop entirely.
    #[tokio::test]
    async fn succeeds_on_first_attempt() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let value: u32 = fast_policy()
            .max_attempts(NonZeroU32::new(5).unwrap())
            .run(move |_attempt| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, UtilsReport>(7)
                }
            })
            .await
            .expect("Ok must propagate");

        assert_eq!(value, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Retries the closure until it succeeds, then returns the value.
    /// The 1-based attempt counter is forwarded faithfully.
    #[tokio::test]
    async fn retries_until_success() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let value: u32 = fast_policy()
            .max_attempts(NonZeroU32::new(5).unwrap())
            .run(move |attempt| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    assert_eq!(attempt, n, "attempt counter must be 1-based and contiguous");
                    if n < 3 {
                        Err(Report::new(UtilsError::Network).attach("transient"))
                    } else {
                        Ok(42)
                    }
                }
            })
            .await
            .expect("third attempt must succeed");

        assert_eq!(value, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// All attempts fail: the budget is exhausted and the report is
    /// re-rooted at `UtilsError::RetryExhausted` while the cause stays
    /// reachable in the frame chain.
    #[tokio::test]
    async fn exhausts_budget_when_all_fail() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let outcome = fast_policy()
            .max_attempts(NonZeroU32::new(3).unwrap())
            .run(move |_attempt| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, _>(Report::new(UtilsError::Network).attach("always down"))
                }
            })
            .await;

        let err = outcome.expect_err("budget must be exhausted");
        assert!(matches!(err.current_context(), UtilsError::RetryExhausted));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// A predicate returning `false` short-circuits the loop and
    /// surfaces the original report unchanged.
    #[tokio::test]
    async fn non_retryable_error_short_circuits() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let outcome = fast_policy()
            .max_attempts(NonZeroU32::new(5).unwrap())
            .run_if(
                move |_attempt| {
                    let calls = Arc::clone(&factory_calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err::<u32, _>(Report::new(UtilsError::Config).attach("bad input"))
                    }
                },
                |report| matches!(report.current_context(), UtilsError::Network),
            )
            .await;

        let err = outcome.expect_err("Config must surface immediately");
        assert!(matches!(err.current_context(), UtilsError::Config));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Cancelling the token during a backoff aborts the retry loop and
    /// reports the last cause wrapped in `RetryExhausted`.
    #[tokio::test]
    async fn cancellation_during_backoff_aborts_with_exhausted() {
        let token = CancellationToken::new();
        let trigger = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });

        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let outcome = Retry::new()
            .max_attempts(NonZeroU32::new(10).unwrap())
            .base_backoff(Duration::from_secs(60))
            .max_backoff(Duration::from_secs(60))
            .jitter(false)
            .with_cancellation(token)
            .run(move |_attempt| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, _>(Report::new(UtilsError::Network).attach("transient"))
                }
            })
            .await;

        let err = outcome.expect_err("cancellation must surface as Err");
        assert!(matches!(err.current_context(), UtilsError::RetryExhausted));
        // Exactly one attempt happened before the long backoff was
        // cancelled.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A pre-cancelled token is observed before the first attempt
    /// runs, so the closure is never invoked.
    #[tokio::test]
    async fn pre_cancelled_token_skips_first_attempt() {
        let token = CancellationToken::new();
        token.cancel();

        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let outcome = Retry::new()
            .max_attempts(NonZeroU32::new(3).unwrap())
            .with_cancellation(token)
            .run(move |_attempt| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, UtilsReport>(0_u32)
                }
            })
            .await;

        let err = outcome.expect_err("pre-cancelled token must short-circuit");
        assert!(matches!(err.current_context(), UtilsError::RetryExhausted));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
