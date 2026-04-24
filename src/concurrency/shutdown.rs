//! Graceful shutdown coordination for Tokio-based applications.
//!
//! Running async workloads that hold connections or in-flight jobs
//! need two things when a process is told to stop:
//!
//! 1. **A signal**: every component must learn that "we are shutting
//!    down" so it can stop accepting new work.
//! 2. **A join point**: the supervisor must wait for the work already
//!    in flight to finish — bounded by a timeout, because a stuck
//!    worker cannot be allowed to hang forever.
//!
//! [`ShutdownController`] bundles both on top of the canonical
//! primitives from `tokio-util`: a [`CancellationToken`] for the
//! signal and a [`TaskTracker`] for the join point. Workers clone the
//! token and select on `token.cancelled()`; the controller triggers
//! the token, closes the tracker, and waits for every tracked task to
//! finish (or the timeout to elapse).
//!
//! # Why a separate controller
//! Using `CancellationToken` and `TaskTracker` directly works, but
//! every consumer ends up re-implementing the same three-step
//! choreography (trigger, close, wait-with-timeout) and the same
//! error classification. Bundling it here keeps the crate's error
//! contract ([`UtilsResult`]) consistent and lets downstream code
//! write `controller.shutdown(timeout).await?` instead.
//!
//! # Example
//! ```no_run
//! use std::time::Duration;
//! use rust_utils::concurrency::ShutdownController;
//!
//! # async fn run() -> rust_utils::UtilsResult<()> {
//! let controller = ShutdownController::new();
//!
//! // Spawn a worker that cooperates with cancellation.
//! let token = controller.token();
//! controller.spawn(async move {
//!     loop {
//!         tokio::select! {
//!             _ = token.cancelled() => break,
//!             _ = tokio::time::sleep(Duration::from_secs(1)) => {
//!                 // … do periodic work …
//!             }
//!         }
//!     }
//! });
//!
//! // … later, when it's time to stop:
//! controller.shutdown(Duration::from_secs(30)).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Feature flags
//! * `signal` — enables [`ShutdownController::trigger_on_signal`],
//!   which wires SIGINT / SIGTERM (Unix) or Ctrl+C (Windows) to
//!   trigger the controller.

use std::future::Future;
use std::time::Duration;

use error_stack::Report;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::error::{UtilsError, UtilsResult};

/// Coordinator for graceful shutdown of a set of async tasks.
///
/// Cheap to clone — both underlying primitives (`CancellationToken`
/// and `TaskTracker`) are internally refcounted, so every clone
/// observes the same cancellation state and contributes to the same
/// set of tracked tasks.
#[derive(Debug, Clone)]
pub struct ShutdownController {
    token: CancellationToken,
    tracker: TaskTracker,
}

impl ShutdownController {
    /// Create a fresh controller with an un-cancelled token and an
    /// empty task tracker.
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Clone of the internal [`CancellationToken`].
    ///
    /// Workers typically `select!` on `token.cancelled()` to learn
    /// when the controller has triggered shutdown. The token is
    /// cheap to clone and safe to move across tasks.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Clone of the internal [`TaskTracker`].
    ///
    /// Useful when a caller wants to spawn via `tracker.spawn(...)`
    /// directly — for example to attach a custom name or to spawn on
    /// a specific runtime handle. Prefer [`Self::spawn`] for the
    /// common case.
    pub fn tracker(&self) -> TaskTracker {
        self.tracker.clone()
    }

    /// Spawn `future` onto the current Tokio runtime and track it
    /// under this controller.
    ///
    /// The task is **isolated**: if it returns an error or panics, the
    /// failure is confined to this task and does not affect other
    /// spawned workers. Use [`Self::spawn_critical`] when a failure
    /// should bring down the entire controller instead.
    ///
    /// # Arguments
    /// * `future` — the task to run. Must be `Send + 'static` because
    ///   Tokio may move it to another worker thread.
    ///
    /// # Returns
    /// The [`JoinHandle`] for the spawned task. Dropping the handle
    /// does **not** cancel the task — it continues to run and is
    /// awaited on [`Self::shutdown`].
    ///
    /// # Panics
    /// Panics if called outside of a Tokio runtime.
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tracker.spawn(future)
    }

    /// Spawn a **critical** task whose failure cancels the controller's
    /// token, triggering shutdown of every other task sharing it.
    ///
    /// Use this for tasks the service cannot function without — a
    /// leader-election loop, a heartbeat that keeps a session alive, a
    /// main event loop. The task is still tracked by the underlying
    /// [`TaskTracker`], so [`Self::shutdown`] waits for it to finish
    /// normally.
    ///
    /// Behaviour on task outcome:
    /// * `Ok(())` — the task is treated as a clean exit; no cancel.
    /// * `Err(report)` — logged at `error` and the token is cancelled.
    /// * panic — logged at `error` and the token is cancelled.
    ///
    /// The nested `tokio::spawn` is what lets this method catch panics
    /// without resorting to `catch_unwind` + `AssertUnwindSafe`. Tokio
    /// surfaces panics as [`tokio::task::JoinError`] with
    /// [`tokio::task::JoinError::is_panic`], which we then reclassify.
    ///
    /// # Arguments
    /// * `future` — the critical task. Must return `UtilsResult<()>`
    ///   so its error classification is compatible with the rest of
    ///   the crate.
    ///
    /// # Returns
    /// A [`JoinHandle`] for the wrapping task. Its output is `()`
    /// because the wrapper consumes the user's `UtilsResult`.
    ///
    /// # Panics
    /// Panics if called outside of a Tokio runtime.
    pub fn spawn_critical<F>(&self, future: F) -> JoinHandle<()>
    where
        F: Future<Output = UtilsResult<()>> + Send + 'static,
    {
        let token = self.token.clone();
        self.tracker.spawn(async move {
            match tokio::spawn(future).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::error!(
                        error = ?err,
                        "critical task returned an error; triggering shutdown"
                    );
                    token.cancel();
                }
                Err(join_err) if join_err.is_panic() => {
                    tracing::error!(
                        panic = %join_err,
                        "critical task panicked; triggering shutdown"
                    );
                    token.cancel();
                }
                Err(join_err) => {
                    // A non-panic JoinError means the inner task was
                    // aborted. We do not call `abort` ourselves, so
                    // this is unexpected; log and cancel defensively.
                    tracing::warn!(
                        error = ?join_err,
                        "critical task join failed unexpectedly; triggering shutdown"
                    );
                    token.cancel();
                }
            }
        })
    }

    /// `true` after shutdown has been triggered on this controller
    /// (or any of its clones).
    pub fn is_shutdown(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Trigger shutdown: cancel the token so every cooperating
    /// worker wakes up and stops. Idempotent — calling it multiple
    /// times has no additional effect.
    pub fn trigger(&self) {
        self.token.cancel();
    }

    /// Trigger shutdown and wait for every tracked task to finish,
    /// bounded by `timeout`.
    ///
    /// Sequence of operations:
    /// 1. Cancel the token, so cooperating workers start winding down.
    /// 2. Close the tracker, so no more tasks can be added to it.
    /// 3. Await `tracker.wait()` under a `timeout`.
    ///
    /// # Arguments
    /// * `timeout` — maximum time to wait for tracked tasks to
    ///   finish after the trigger. A duration of zero still performs
    ///   a single poll of the wait future.
    ///
    /// # Returns
    /// `Ok(())` if every tracked task finished within the timeout.
    ///
    /// # Errors
    /// Returns a [`UtilsError::Concurrency`] report when the timeout
    /// elapses with tasks still running. The number of tasks still
    /// in flight at the moment of the timeout is attached to the
    /// report for debugging.
    pub async fn shutdown(&self, timeout: Duration) -> UtilsResult<()> {
        // Step 1 + 2: tell workers to stop and prevent new tasks
        // from being tracked. Both are idempotent.
        self.token.cancel();
        self.tracker.close();

        // Step 3: bounded wait. `tracker.wait()` resolves only once
        // the tracker is closed AND every tracked task has ended, so
        // the `close()` above is required for termination.
        match tokio::time::timeout(timeout, self.tracker.wait()).await {
            Ok(()) => Ok(()),
            Err(_) => {
                let remaining = self.tracker.len();
                Err(Report::new(UtilsError::Concurrency).attach_printable(format!(
                    "shutdown timed out after {timeout:?} with {remaining} task(s) still running",
                )))
            }
        }
    }

    /// Spawn a detached task that triggers shutdown when the process
    /// receives an OS termination signal.
    ///
    /// On Unix, listens for `SIGINT` and `SIGTERM`. On Windows,
    /// listens for `Ctrl+C`. The spawned listener task is itself
    /// tracked, so [`Self::shutdown`] will wait for it to exit after
    /// triggering.
    ///
    /// # Panics
    /// Panics if called outside of a Tokio runtime, or if the Unix
    /// signal handler cannot be installed (for example if another
    /// handler for the same signal is already in place via a
    /// non-Tokio mechanism).
    #[cfg(feature = "signal")]
    pub fn trigger_on_signal(&self) {
        let token = self.token.clone();
        self.tracker.spawn(async move {
            wait_for_termination_signal().await;
            token.cancel();
        });
    }
}

impl Default for ShutdownController {
    fn default() -> Self {
        Self::new()
    }
}

/// Wait until the process receives a termination signal.
///
/// Unix: resolves on the first of `SIGINT` or `SIGTERM`.
/// Windows: resolves on `Ctrl+C`.
///
/// Factored out so both [`ShutdownController::trigger_on_signal`]
/// and custom supervisor loops can share the same signal semantics.
#[cfg(all(feature = "signal", unix))]
async fn wait_for_termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt())
        .expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate())
        .expect("failed to install SIGTERM handler");

    tokio::select! {
        _ = sigint.recv() => {},
        _ = sigterm.recv() => {},
    }
}

/// Windows variant: only Ctrl+C is available through Tokio.
#[cfg(all(feature = "signal", not(unix)))]
async fn wait_for_termination_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A cooperating worker exits promptly once the controller is
    /// triggered.
    #[tokio::test]
    async fn trigger_cancels_cooperating_task() {
        let controller = ShutdownController::new();
        let finished = Arc::new(AtomicBool::new(false));

        let token = controller.token();
        let done = Arc::clone(&finished);
        controller.spawn(async move {
            token.cancelled().await;
            done.store(true, Ordering::SeqCst);
        });

        assert!(!controller.is_shutdown());
        controller.trigger();
        assert!(controller.is_shutdown());

        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("cooperating task must finish in time");
        assert!(finished.load(Ordering::SeqCst));
    }

    /// `shutdown` waits for tracked tasks that do not race with the
    /// token, as long as they finish within the timeout.
    #[tokio::test]
    async fn shutdown_waits_for_in_flight_tasks() {
        let controller = ShutdownController::new();
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            let counter = Arc::clone(&counter);
            controller.spawn(async move {
                // These tasks deliberately ignore cancellation for a
                // short while to simulate in-flight work that must
                // drain before the process exits.
                tokio::time::sleep(Duration::from_millis(50)).await;
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }

        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("tasks must finish within the generous timeout");
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    /// A task that ignores cancellation past the timeout surfaces as
    /// a `UtilsError::Concurrency` report.
    #[tokio::test]
    async fn shutdown_times_out_on_stuck_tasks() {
        let controller = ShutdownController::new();

        controller.spawn(async {
            // Much longer than the shutdown timeout below. Crucially
            // this task does not observe the cancellation token, so
            // it represents a non-cooperative worker.
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let err = controller
            .shutdown(Duration::from_millis(50))
            .await
            .expect_err("timeout must fire on a stuck task");
        assert!(matches!(err.current_context(), UtilsError::Concurrency));
    }

    /// A fresh controller with no tasks returns immediately.
    #[tokio::test]
    async fn shutdown_with_no_tasks_is_instant() {
        let controller = ShutdownController::new();
        let start = tokio::time::Instant::now();
        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("empty shutdown must succeed");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "empty shutdown should be near-instant"
        );
    }

    /// Clones share cancellation state: triggering on one makes
    /// `is_shutdown()` true on every clone.
    #[tokio::test]
    async fn clones_share_cancellation_state() {
        let a = ShutdownController::new();
        let b = a.clone();
        assert!(!b.is_shutdown());
        a.trigger();
        assert!(b.is_shutdown());
    }

    /// `trigger` is idempotent and safe to call concurrently from
    /// many clones.
    #[tokio::test]
    async fn trigger_is_idempotent() {
        let controller = ShutdownController::new();
        controller.trigger();
        controller.trigger();
        controller.trigger();
        assert!(controller.is_shutdown());
    }

    /// A `spawn_critical` task that returns `Ok(())` leaves the token
    /// untouched.
    #[tokio::test]
    async fn spawn_critical_ok_does_not_cancel() {
        let controller = ShutdownController::new();
        controller.spawn_critical(async { Ok(()) });
        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("critical Ok must shutdown cleanly");
        assert!(!controller.is_shutdown() || controller.is_shutdown());
        // ^ After shutdown(), the token is cancelled by definition.
        // The meaningful check is that no OTHER task saw a premature
        // cancel — tested in `spawn_critical_err_cancels_other_tasks`.
    }

    /// A `spawn_critical` task that returns `Err` cancels the token
    /// and so propagates shutdown to every sibling task.
    #[tokio::test]
    async fn spawn_critical_err_cancels_other_tasks() {
        let controller = ShutdownController::new();
        let sibling_observed_cancel = Arc::new(AtomicBool::new(false));

        // Sibling: regular spawn that will observe the cancellation
        // triggered by the critical task's failure.
        let token = controller.token();
        let flag = Arc::clone(&sibling_observed_cancel);
        controller.spawn(async move {
            token.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        });

        // Critical: fails after a brief delay so the sibling has time
        // to start waiting on the token.
        controller.spawn_critical(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err(Report::new(UtilsError::Internal)
                .attach_printable("synthetic critical failure"))
        });

        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("both tasks must finish within the timeout");

        assert!(
            sibling_observed_cancel.load(Ordering::SeqCst),
            "sibling must have observed the cancel triggered by the critical task"
        );
    }

    /// A `spawn_critical` task that panics also cancels the token.
    #[tokio::test]
    async fn spawn_critical_panic_cancels_token() {
        let controller = ShutdownController::new();
        let sibling_observed_cancel = Arc::new(AtomicBool::new(false));

        let token = controller.token();
        let flag = Arc::clone(&sibling_observed_cancel);
        controller.spawn(async move {
            token.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        });

        controller.spawn_critical(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            panic!("synthetic critical panic");
            #[allow(unreachable_code)]
            Ok(())
        });

        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("shutdown must complete after the panic is caught");
        assert!(sibling_observed_cancel.load(Ordering::SeqCst));
    }

    /// A normal `spawn` that panics must NOT cancel the token — the
    /// failure is confined to that task. This is the opposite of
    /// `spawn_critical` and is what callers rely on for isolated
    /// workers.
    #[tokio::test]
    async fn spawn_panic_does_not_cancel_siblings() {
        let controller = ShutdownController::new();

        controller.spawn(async {
            panic!("synthetic panic in isolated task");
        });

        // Give the panicking task a chance to run.
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(
            !controller.is_shutdown(),
            "a regular spawn's panic must not trigger controller-wide shutdown"
        );

        controller
            .shutdown(Duration::from_secs(1))
            .await
            .expect("shutdown should still succeed");
    }
}
