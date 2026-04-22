//! Leading-edge throttle that discards calls during a cooldown window.
//!
//! [`Throttle`] enforces "at most one invocation per interval": the
//! first call runs, any further call arriving before the interval has
//! elapsed since the last run is *discarded* (returns `None`). The
//! next call to run resets the window.
//!
//! # When to use this
//! * Metrics or status logs that must not spam (`log stats at most
//!   once per 5s, no matter how often called`).
//! * User-facing actions that should be idempotent within a short
//!   window (e.g. a button that triggers a refresh).
//!
//! # When NOT to use this
//! If you need *every* call to run but paced, use the rate-limited
//! HTTP client in [`network`](crate::network) (or a
//! `governor::RateLimiter` directly). This throttle deliberately
//! drops calls; it never queues them.
//!
//! # Example
//! ```
//! use std::time::Duration;
//! use rust_utils::concurrency::Throttle;
//!
//! # async fn run() {
//! let t = Throttle::new(Duration::from_millis(500));
//! // First call runs:
//! assert!(t.try_run(|| async { 1 }).await.is_some());
//! // Second call within 500ms is discarded:
//! assert!(t.try_run(|| async { 2 }).await.is_none());
//! # }
//! ```

use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Leading-edge throttle. See module-level docs.
///
/// Uses a plain `std::sync::Mutex` because the guarded state is only
/// held synchronously — never across an `.await` — so the lock never
/// contends with async scheduling.
pub struct Throttle {
    min_interval: Duration,
    last_run: Mutex<Option<Instant>>,
}

impl Throttle {
    /// Create a throttle that admits at most one invocation per
    /// `min_interval`.
    pub const fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_run: Mutex::new(None),
        }
    }

    /// Minimum interval between successful runs.
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Run `f` if the last successful run is at least
    /// `min_interval` ago; otherwise return `None` without invoking
    /// the closure.
    ///
    /// The "last run" timestamp is updated *before* the closure is
    /// awaited, so concurrent callers that arrive while `f` is still
    /// running observe the cooldown. This matches the intent: the
    /// throttle paces attempts, not successful completions.
    pub async fn try_run<F, Fut, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = R>,
    {
        // Critical section: check-and-set the last_run stamp. We drop
        // the guard before awaiting the future so the mutex is never
        // held across `.await`.
        {
            let mut last = self
                .last_run
                .lock()
                .expect("throttle mutex poisoned by a panicked holder");
            let now = Instant::now();
            if let Some(previous) = *last
                && now.duration_since(previous) < self.min_interval
            {
                return None;
            }
            *last = Some(now);
        }
        Some(f().await)
    }

    /// Forget the last-run timestamp. The next call to
    /// [`try_run`](Self::try_run) will run immediately regardless of
    /// when the previous one happened.
    pub fn reset(&self) {
        *self
            .last_run
            .lock()
            .expect("throttle mutex poisoned by a panicked holder") = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn first_call_runs_subsequent_calls_are_dropped() {
        let throttle = Throttle::new(Duration::from_millis(200));
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            let counter = Arc::clone(&counter);
            let _ = throttle
                .try_run(|| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                })
                .await;
        }

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn call_after_interval_is_admitted() {
        let throttle = Throttle::new(Duration::from_millis(50));
        assert!(throttle.try_run(|| async {}).await.is_some());
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(throttle.try_run(|| async {}).await.is_some());
    }

    #[tokio::test]
    async fn reset_lets_the_next_call_through() {
        let throttle = Throttle::new(Duration::from_secs(60));
        assert!(throttle.try_run(|| async {}).await.is_some());
        // Without reset, this would be throttled for 60 seconds.
        assert!(throttle.try_run(|| async {}).await.is_none());
        throttle.reset();
        assert!(throttle.try_run(|| async {}).await.is_some());
    }

    #[tokio::test]
    async fn try_run_returns_closure_output_when_admitted() {
        let throttle = Throttle::new(Duration::from_millis(10));
        let value = throttle.try_run(|| async { 42_u32 }).await;
        assert_eq!(value, Some(42));
    }

    /// Concurrent callers that race must not double-run the closure
    /// within the cooldown window.
    #[tokio::test]
    async fn concurrent_callers_respect_the_interval() {
        let throttle = Arc::new(Throttle::new(Duration::from_millis(200)));
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let throttle = Arc::clone(&throttle);
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                throttle
                    .try_run(|| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}
