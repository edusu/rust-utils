//! Protocol-agnostic rate limiters built on top of `governor` (GCRA).
//!
//! Two flavours, both `Clone`-cheap (state lives behind an `Arc` so all
//! clones share the same buckets):
//!
//! * [`RateLimiter`] — single global pace shared by every caller.
//! * [`KeyedRateLimiter<K>`] — independent pace per key (e.g. one
//!   bucket per chat-id, per tenant, per user).
//!
//! Unlike [`crate::network::RateLimitedClient`], these are not bound to
//! `reqwest`: their [`run`](RateLimiter::run) method takes any async
//! closure, so the same primitive paces HTTP calls, database writes,
//! message-bus publishes, or any other unit of work.
//!
//! # Stacking global + per-key
//! Per-key and global limits are independent constraints and frequently
//! stack. The recommended order is **per-key first, global second**:
//! the per-key bucket is usually the narrower one, so acquiring it
//! first prevents a global cell from being burned on a call that is
//! about to wait on the per-key bucket anyway.
//!
//! ```no_run
//! use std::num::NonZeroU32;
//! use rust_utils::concurrency::rate_limit::{KeyedRateLimiter, RateLimiter};
//! use rust_utils::network::RateLimitWindow;
//!
//! # async fn run() -> rust_utils::UtilsResult<()> {
//! let global = RateLimiter::new(
//!     RateLimitWindow::PerSecond(NonZeroU32::new(30).unwrap()),
//!     None,
//! )?;
//! let per_chat = KeyedRateLimiter::<i64>::new(
//!     RateLimitWindow::PerSecond(NonZeroU32::new(1).unwrap()),
//!     None,
//! )?;
//!
//! let chat_id: i64 = 42;
//! per_chat
//!     .run(&chat_id, || async {
//!         global.run(|| async { /* the rate-limited operation */ }).await
//!     })
//!     .await;
//! # Ok(()) }
//! ```
//!
//! # When NOT to use this
//! * If you only need to pace HTTP traffic, prefer
//!   [`crate::network::RateLimitedClient`] directly.
//! * If you need to *drop* calls within a cooldown window (instead of
//!   queuing them), use [`crate::concurrency::Throttle`].

use std::future::Future;
use std::hash::Hash;
use std::num::NonZeroU32;
use std::sync::Arc;

use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::state::{InMemoryState, NotKeyed};
use governor::RateLimiter as GovernorRateLimiter;

use crate::error::UtilsResult;
use crate::network::rate_limit::{quota_from_window, RateLimitWindow};

/// Concrete type of the in-memory, direct, non-keyed limiter used by
/// [`RateLimiter`]. Aliased because the four `governor` type parameters
/// turn every signature into noise.
type DirectLimiter = GovernorRateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

/// Concrete type of the in-memory, keyed limiter used by
/// [`KeyedRateLimiter`]. Uses `governor`'s default keyed state store
/// (a `HashMap` guarded by a `Mutex` unless the `dashmap` feature of
/// `governor` is enabled, in which case it switches to a `DashMap`).
type KeyedLimiter<K> =
    GovernorRateLimiter<K, DefaultKeyedStateStore<K>, DefaultClock, NoOpMiddleware>;

/// Generic, non-keyed rate limiter.
///
/// Wraps a [`governor::RateLimiter`] in an `Arc` so cloning is cheap
/// and every clone observes the same bucket — useful when the same
/// global limit must apply across tasks or modules.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    limiter: Arc<DirectLimiter>,
}

impl RateLimiter {
    /// Build a rate limiter with the given window and optional burst.
    ///
    /// # Arguments
    /// * `window` — how often cells are replenished.
    /// * `burst`  — maximum bucket size. When `None`, `governor`'s
    ///   default for the window is used.
    ///
    /// # Errors
    /// Returns [`crate::error::UtilsError::Config`] when `window` is
    /// [`RateLimitWindow::Custom`] with a zero-length duration.
    pub fn new(window: RateLimitWindow, burst: Option<NonZeroU32>) -> UtilsResult<Self> {
        let quota = quota_from_window(window, burst)?;
        Ok(Self {
            limiter: Arc::new(GovernorRateLimiter::direct(quota)),
        })
    }

    /// Wait until the limiter releases a slot, then return.
    ///
    /// Useful when the work to be paced is built up across several
    /// statements and a closure-based [`run`](Self::run) is awkward.
    pub async fn wait_for_slot(&self) {
        self.limiter.until_ready().await;
    }

    /// Wait for a slot, then run `f` and return its output.
    ///
    /// The closure is only invoked once a cell has been acquired, so
    /// the rate limit caps *attempts*, not in-flight concurrency. If
    /// `f`'s future takes longer than the replenishment period, more
    /// than one closure can be in flight simultaneously.
    pub async fn run<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        self.wait_for_slot().await;
        f().await
    }
}

/// Generic, keyed rate limiter.
///
/// Each distinct key gets an independent bucket: keys do not steal
/// cells from one another. Combine with a [`RateLimiter`] to enforce a
/// global ceiling on top of per-key pacing — see the module-level docs
/// for the recommended stacking order.
///
/// `K` must be `Clone + Eq + Hash + Send + Sync + 'static`. Keys are
/// retained until [`retain_recent`](Self::retain_recent) is called, so
/// long-running processes that ingest unbounded keys (e.g. arbitrary
/// chat ids) should call it periodically to drop stale buckets.
#[derive(Debug, Clone)]
pub struct KeyedRateLimiter<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    limiter: Arc<KeyedLimiter<K>>,
}

impl<K> KeyedRateLimiter<K>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
{
    /// Build a keyed rate limiter with the given window and optional
    /// burst, applied **per key**.
    ///
    /// # Arguments
    /// * `window` — how often cells are replenished for each key.
    /// * `burst`  — maximum bucket size per key. When `None`,
    ///   `governor`'s default for the window is used.
    ///
    /// # Errors
    /// Returns [`crate::error::UtilsError::Config`] when `window` is
    /// [`RateLimitWindow::Custom`] with a zero-length duration.
    pub fn new(window: RateLimitWindow, burst: Option<NonZeroU32>) -> UtilsResult<Self> {
        let quota = quota_from_window(window, burst)?;
        Ok(Self {
            limiter: Arc::new(GovernorRateLimiter::keyed(quota)),
        })
    }

    /// Wait until the bucket associated with `key` releases a slot.
    pub async fn wait_for_slot(&self, key: &K) {
        self.limiter.until_key_ready(key).await;
    }

    /// Wait for a slot on `key`'s bucket, then run `f` and return its
    /// output. See [`RateLimiter::run`] for in-flight semantics.
    pub async fn run<F, Fut, T>(&self, key: &K, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        self.wait_for_slot(key).await;
        f().await
    }

    /// Drop buckets whose state is indistinguishable from "fresh".
    ///
    /// Keyed limiters never automatically forget keys, so a process
    /// that ingests an unbounded set of keys must call this (or
    /// [`shrink_to_fit`](Self::shrink_to_fit)) periodically to keep
    /// memory bounded. Cheap to call; safe under concurrent use.
    pub fn retain_recent(&self) {
        self.limiter.retain_recent();
    }

    /// Shrink the underlying state store's capacity to fit. Pair with
    /// [`retain_recent`](Self::retain_recent) for memory reclamation
    /// in long-running services.
    pub fn shrink_to_fit(&self) {
        self.limiter.shrink_to_fit();
    }

    /// Number of live keys currently tracked by the limiter.
    ///
    /// May be approximate depending on the state store; useful for
    /// metrics and tests, not for scheduling decisions.
    pub fn len(&self) -> usize {
        self.limiter.len()
    }

    /// `true` when no keys are tracked.
    pub fn is_empty(&self) -> bool {
        self.limiter.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// `PerSecond(2)` admits the initial burst of 2 cells immediately
    /// and delays the 3rd acquisition by ~500ms.
    #[tokio::test]
    async fn rate_limiter_paces_calls() {
        let limiter = RateLimiter::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(2).unwrap()),
            None,
        )
        .expect("non-zero quota must build");

        limiter.wait_for_slot().await;
        limiter.wait_for_slot().await;

        let start = Instant::now();
        limiter.wait_for_slot().await;
        assert!(
            start.elapsed() >= Duration::from_millis(400),
            "expected >= 400ms wait after draining burst"
        );
    }

    /// `run` waits for a slot and then invokes the closure exactly
    /// once per call, returning its output.
    #[tokio::test]
    async fn run_executes_closure_after_wait() {
        let limiter = RateLimiter::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(5).unwrap()),
            None,
        )
        .unwrap();
        let counter = Arc::new(AtomicUsize::new(0));

        let value = limiter
            .run(|| {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    42_u32
                }
            })
            .await;

        assert_eq!(value, 42);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    /// Clones must share the same bucket: draining the burst on one
    /// clone forces the other to wait.
    #[tokio::test]
    async fn rate_limiter_clones_share_state() {
        let a = RateLimiter::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(1).unwrap()),
            None,
        )
        .unwrap();
        let b = a.clone();

        a.wait_for_slot().await;

        let start = Instant::now();
        b.wait_for_slot().await;
        assert!(
            start.elapsed() >= Duration::from_millis(800),
            "clone should have waited ~1s"
        );
    }

    /// Keyed: two distinct keys must not interfere with one another.
    /// With `PerSecond(1)`, draining key A's bucket should not delay
    /// the first acquisition on key B at all.
    #[tokio::test]
    async fn keyed_rate_limiter_isolates_keys() {
        let limiter = KeyedRateLimiter::<i64>::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(1).unwrap()),
            None,
        )
        .unwrap();

        limiter.wait_for_slot(&1).await;

        let start = Instant::now();
        limiter.wait_for_slot(&2).await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "different keys must have independent buckets"
        );
    }

    /// Keyed: repeated calls on the same key share the same bucket
    /// and pace each other.
    #[tokio::test]
    async fn keyed_rate_limiter_paces_same_key() {
        let limiter = KeyedRateLimiter::<i64>::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(1).unwrap()),
            None,
        )
        .unwrap();

        limiter.wait_for_slot(&7).await;

        let start = Instant::now();
        limiter.wait_for_slot(&7).await;
        assert!(
            start.elapsed() >= Duration::from_millis(800),
            "same key should pace itself"
        );
    }

    /// `retain_recent` reclaims buckets whose state is fresh again.
    /// Use a per-second cadence so the test does not have to sleep
    /// long.
    #[tokio::test]
    async fn keyed_rate_limiter_retain_recent_drops_idle_keys() {
        let limiter = KeyedRateLimiter::<i64>::new(
            RateLimitWindow::PerSecond(NonZeroU32::new(2).unwrap()),
            None,
        )
        .unwrap();

        limiter.wait_for_slot(&1).await;
        limiter.wait_for_slot(&2).await;
        assert_eq!(limiter.len(), 2);

        // Wait long enough that both buckets are back to a "fresh"
        // theoretical arrival time, then ask for cleanup.
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        limiter.retain_recent();
        assert_eq!(limiter.len(), 0, "idle keys must be reclaimed");
    }

    /// A custom window with a zero duration is rejected at construction
    /// time rather than panicking inside `governor`.
    #[test]
    fn zero_period_is_rejected() {
        let report = RateLimiter::new(
            RateLimitWindow::Custom {
                period: Duration::ZERO,
            },
            None,
        )
        .expect_err("zero-length period must be rejected");
        assert!(matches!(
            report.current_context(),
            crate::error::UtilsError::Config
        ));
    }
}
