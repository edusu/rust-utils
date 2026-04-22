//! Bounded-concurrency worker pool with submission backpressure.
//!
//! [`WorkerPool`] runs an async handler over submitted jobs with a cap
//! on the number of concurrently-running jobs. Submission applies
//! backpressure: once `max_concurrent` tasks are in flight,
//! [`WorkerPool::submit`] suspends until one of them finishes.
//!
//! # Design
//! The pool is built around a [`tokio::sync::Semaphore`] sized to the
//! concurrency cap and a [`tokio::task::JoinSet`] that owns the
//! spawned tasks. Each submitted job acquires a permit, spawns a task
//! that runs the handler, and releases the permit on completion.
//!
//! This avoids the pitfall of a shared `Arc<Mutex<Receiver>>` — which
//! would serialize all workers on a single lock and defeat the point
//! of a pool — while keeping the implementation dependency-free
//! beyond `tokio`.
//!
//! # Example
//! ```no_run
//! use std::num::NonZeroUsize;
//! use rust_utils::concurrency::WorkerPool;
//!
//! # async fn run() {
//! let mut pool = WorkerPool::new(NonZeroUsize::new(4).unwrap(), |job: u32| async move {
//!     // … do work with `job` …
//!     let _ = job;
//! });
//! for job in 0..100 {
//!     pool.submit(job).await;
//! }
//! pool.join().await;
//! # }
//! ```

use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Boxed future returned by the handler after job substitution.
type BoxedFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Type-erased handler stored in the pool.
type Handler<T> = Arc<dyn Fn(T) -> BoxedFuture + Send + Sync + 'static>;

/// Fan-out pool that runs jobs through a fixed-concurrency async
/// handler with backpressure on submission.
pub struct WorkerPool<T: Send + 'static> {
    semaphore: Arc<Semaphore>,
    handler: Handler<T>,
    joins: JoinSet<()>,
}

impl<T: Send + 'static> WorkerPool<T> {
    /// Create a new pool.
    ///
    /// # Arguments
    /// * `max_concurrent` — maximum number of jobs running at the
    ///   same time; also the depth of backpressure on
    ///   [`Self::submit`].
    /// * `handler` — async function applied to every submitted job.
    ///   Must be `Fn` (callable repeatedly), `Send + Sync` (shared
    ///   across worker tasks) and `'static` (outlives the pool).
    pub fn new<F, Fut>(max_concurrent: NonZeroUsize, handler: F) -> Self
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // Box and type-erase the handler so the pool's type stays
        // independent of the concrete `Fut` returned by the closure.
        let handler: Handler<T> = Arc::new(move |job: T| Box::pin(handler(job)) as BoxedFuture);
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent.get())),
            handler,
            joins: JoinSet::new(),
        }
    }

    /// Submit a job. Suspends when `max_concurrent` tasks are already
    /// running, resuming once a slot frees up.
    pub async fn submit(&mut self, job: T) {
        // `acquire_owned` returns a permit tied to the semaphore's
        // lifetime via `Arc`. The permit is dropped inside the
        // spawned task, freeing the slot on completion.
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is never closed while the pool is alive");
        let handler = Arc::clone(&self.handler);
        self.joins.spawn(async move {
            // Hold the permit for the full duration of the handler.
            let _permit = permit;
            handler(job).await;
        });
    }

    /// Wait for every submitted job to finish. Consumes the pool.
    pub async fn join(mut self) {
        while self.joins.join_next().await.is_some() {}
    }

    /// Number of jobs whose task is still tracked in the pool.
    ///
    /// Note: this counts spawned tasks that have not yet been reaped
    /// by `join_next`, which slightly overestimates the number of
    /// running tasks under high churn. It is intended for metrics and
    /// tests, not for scheduling decisions.
    pub fn active(&self) -> usize {
        self.joins.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// All submitted jobs must run to completion and the observed
    /// maximum concurrency must not exceed the configured cap.
    #[tokio::test]
    async fn respects_max_concurrency() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let total = Arc::new(AtomicUsize::new(0));

        let handler = {
            let running = Arc::clone(&running);
            let peak = Arc::clone(&peak);
            let total = Arc::clone(&total);
            move |_job: usize| {
                let running = Arc::clone(&running);
                let peak = Arc::clone(&peak);
                let total = Arc::clone(&total);
                async move {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    total.fetch_add(1, Ordering::SeqCst);
                }
            }
        };

        let mut pool = WorkerPool::new(NonZeroUsize::new(3).unwrap(), handler);
        for i in 0..12 {
            pool.submit(i).await;
        }
        pool.join().await;

        assert_eq!(total.load(Ordering::SeqCst), 12);
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "observed concurrency {} exceeds cap 3",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(running.load(Ordering::SeqCst), 0);
    }

    /// When the cap is reached, `submit` should suspend until a slot
    /// frees up. We submit `cap + 1` slow jobs and confirm the total
    /// submission time dominates the single-job duration.
    #[tokio::test]
    async fn submit_applies_backpressure() {
        let handler = |_: usize| async {
            tokio::time::sleep(Duration::from_millis(80)).await;
        };
        let mut pool = WorkerPool::new(NonZeroUsize::new(2).unwrap(), handler);

        let start = tokio::time::Instant::now();
        for i in 0..3 {
            pool.submit(i).await;
        }
        let submit_elapsed = start.elapsed();
        pool.join().await;

        // First two submits are immediate, the third waits for a slot:
        // overall submission time >= one handler duration.
        assert!(
            submit_elapsed >= Duration::from_millis(70),
            "expected backpressure >= 70ms, got {submit_elapsed:?}"
        );
    }
}
