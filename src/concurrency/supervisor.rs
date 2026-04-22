//! Restart-policy supervisor for long-running Tokio tasks.
//!
//! A [`Supervisor`] runs a task-producing closure in a loop and
//! restarts the task on exit — whether the exit is clean, an error,
//! or a panic. Configurable budget and backoff prevent a crash-loop
//! from pinning a CPU.
//!
//! # Scope and non-goals
//! * **In scope:** keeping a single long-running service alive
//!   (NATS subscriber, metrics exporter, TCP acceptor, cache warmer,
//!   …). Panics inside the supervised task are contained and logged
//!   instead of unwinding the runtime worker.
//! * **Not in scope:** request-level retry (use `RetryingClient` in
//!   [`network`](crate::network)) or Erlang-style hierarchical
//!   supervision trees (out of scope for this crate).
//!
//! # Cooperation with shutdown
//! The supervisor takes a [`CancellationToken`]. The same token is
//! handed to every restarted instance of the supervised task, so the
//! task can `select!` on `token.cancelled()` and wind down. When the
//! token fires, the supervisor stops restarting and returns `Ok(())`
//! on its next join. The supervisor never aborts a non-cooperative
//! task — the outer [`ShutdownController`](super::ShutdownController)
//! bounds overall shutdown time.
//!
//! # Example
//! ```no_run
//! use std::num::NonZeroU32;
//! use std::time::Duration;
//! use rust_utils::concurrency::{ShutdownController, Supervisor};
//!
//! # async fn run() -> rust_utils::UtilsResult<()> {
//! let ctrl = ShutdownController::new();
//! let token = ctrl.token();
//!
//! ctrl.spawn(async move {
//!     let _ = Supervisor::new()
//!         .name("nats-subscriber")
//!         .max_restarts(NonZeroU32::new(10).unwrap())
//!         .restart_window(Duration::from_secs(60))
//!         .base_backoff(Duration::from_millis(200))
//!         .max_backoff(Duration::from_secs(30))
//!         .run(token, |tok| async move {
//!             // Run the supervised loop; observe `tok` for shutdown.
//!             let _ = tok;
//!             Ok(())
//!         })
//!         .await;
//! });
//! # Ok(())
//! # }
//! ```

use std::collections::VecDeque;
use std::future::Future;
use std::num::NonZeroU32;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use error_stack::Report;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{UtilsError, UtilsResult};

/// Builder + runner for a restart-policy supervisor.
///
/// Construct with [`Supervisor::new`], tune with the fluent setters,
/// then drive with [`Supervisor::run`]. The same builder can be
/// cloned and reused across several supervised tasks if they share a
/// policy.
#[derive(Debug, Clone)]
pub struct Supervisor {
    name: Option<String>,
    max_restarts: Option<NonZeroU32>,
    restart_window: Option<Duration>,
    base_backoff: Duration,
    max_backoff: Duration,
    jitter: bool,
    restart_on_ok: bool,
    restart_on_panic: bool,
}

impl Supervisor {
    /// Default policy: unlimited restarts, 200ms base backoff, 30s
    /// cap, full jitter, restart on everything (Ok/Err/panic).
    pub fn new() -> Self {
        Self {
            name: None,
            max_restarts: None,
            restart_window: None,
            base_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(30),
            jitter: true,
            restart_on_ok: true,
            restart_on_panic: true,
        }
    }

    /// Optional label used in `tracing` events emitted by the
    /// supervision loop. Helps distinguish supervisors when several
    /// run side-by-side.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Maximum number of restarts before the supervisor gives up and
    /// returns [`UtilsError::RetryExhausted`]. When paired with
    /// [`Self::restart_window`], only restarts inside the rolling
    /// window count towards the cap.
    ///
    /// Default: unlimited.
    pub fn max_restarts(mut self, n: NonZeroU32) -> Self {
        self.max_restarts = Some(n);
        self
    }

    /// Rolling time window for the restart count. Restart timestamps
    /// older than the window are pruned before the cap check, so a
    /// task that dies sporadically over hours does not exhaust its
    /// budget the way a true crash-loop does.
    ///
    /// Default: no window (cap applies to total restarts since start).
    pub fn restart_window(mut self, d: Duration) -> Self {
        self.restart_window = Some(d);
        self
    }

    /// Base duration for the exponential backoff between restarts:
    /// `base * 2^(consecutive_restarts - 1)`, capped by
    /// [`Self::max_backoff`].
    pub fn base_backoff(mut self, d: Duration) -> Self {
        self.base_backoff = d;
        self
    }

    /// Cap on the backoff between restarts.
    pub fn max_backoff(mut self, d: Duration) -> Self {
        self.max_backoff = d;
        self
    }

    /// Enable full jitter on the computed backoff. Full jitter draws
    /// the sleep uniformly from `[0, computed]`, which desynchronises
    /// crash-looping replicas so they do not reconnect in lockstep.
    pub fn jitter(mut self, v: bool) -> Self {
        self.jitter = v;
        self
    }

    /// Whether to restart when the supervised task returns `Ok(())`.
    /// Default `true` — this supervisor is meant for tasks that are
    /// not supposed to terminate cleanly. Set to `false` when the
    /// task has a legitimate stop condition (e.g. draining a bounded
    /// queue).
    pub fn restart_on_ok(mut self, v: bool) -> Self {
        self.restart_on_ok = v;
        self
    }

    /// Whether to restart on panic. Default `true`. When disabled, a
    /// panic terminates the supervisor with
    /// [`UtilsError::Concurrency`] and the panic message attached.
    pub fn restart_on_panic(mut self, v: bool) -> Self {
        self.restart_on_panic = v;
        self
    }

    /// Drive the supervision loop.
    ///
    /// # Arguments
    /// * `token` — cancellation signal. Cloned and handed to every
    ///   spawned instance; the task is expected to observe it and
    ///   return promptly when it fires.
    /// * `factory` — closure producing the supervised `Future`. Must
    ///   be `Fn` (callable repeatedly, once per restart) and
    ///   `Send + Sync + 'static` so the future can be spawned onto
    ///   the Tokio runtime.
    ///
    /// # Returns
    /// * `Ok(())` on clean shutdown (`token` cancelled, or the task
    ///   returned `Ok(())` and `restart_on_ok` is `false`).
    ///
    /// # Errors
    /// * [`UtilsError::RetryExhausted`] when the configured restart
    ///   budget is depleted. The last error from the task is chained
    ///   into the report as context.
    /// * [`UtilsError::Concurrency`] when the task panics and
    ///   `restart_on_panic` is `false`.
    pub async fn run<F, Fut>(self, token: CancellationToken, factory: F) -> UtilsResult<()>
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = UtilsResult<()>> + Send + 'static,
    {
        // Rolling window of restart timestamps. The front is the
        // oldest; pruned lazily before each cap check.
        let mut restart_history: VecDeque<Instant> = VecDeque::new();
        // Consecutive restart count controls the exponential backoff
        // curve. Reset on a "healthy run" (longer than `max_backoff`,
        // since that is the point beyond which the task is clearly
        // not in a tight crash loop).
        let mut consecutive: u32 = 0;
        let name: &str = self.name.as_deref().unwrap_or("supervisor");

        loop {
            if token.is_cancelled() {
                return Ok(());
            }

            let run_started_at = Instant::now();
            let fut = factory(token.clone());

            // Use a JoinSet so panics surface as `JoinError` rather
            // than unwinding the supervising task.
            let mut set = JoinSet::new();
            set.spawn(fut);
            // `set` was just populated with one task, so
            // `join_next().await` yields `Some(...)`.
            let outcome = set.join_next().await.expect("just spawned one task");

            if token.is_cancelled() {
                // Shutdown in progress: stop supervising even if the
                // task returned an error. The outer layer decides
                // how to report it (typically it is just a natural
                // consequence of the cancellation).
                return Ok(());
            }

            // Healthy-run heuristic: if the instance lived longer
            // than the backoff cap, we consider the process "not in
            // a crash loop" and reset the consecutive counter so the
            // next transient failure starts from the short end of
            // the backoff curve again.
            let ran_for = run_started_at.elapsed();
            if ran_for >= self.max_backoff {
                consecutive = 0;
            }

            match outcome {
                Ok(Ok(())) => {
                    if !self.restart_on_ok {
                        return Ok(());
                    }
                    tracing::info!(
                        supervisor = name,
                        "supervised task returned Ok; restarting per policy"
                    );
                }
                Ok(Err(report)) => {
                    tracing::error!(
                        supervisor = name,
                        error = ?report,
                        "supervised task returned an error"
                    );
                }
                Err(join_err) if join_err.is_panic() => {
                    let panic_msg = panic_message(&join_err);
                    if !self.restart_on_panic {
                        return Err(Report::new(UtilsError::Concurrency)
                            .attach_printable(format!(
                                "supervised task [{name}] panicked and restart_on_panic is disabled"
                            ))
                            .attach_printable(format!("panic: {panic_msg}")));
                    }
                    tracing::error!(
                        supervisor = name,
                        panic = %panic_msg,
                        "supervised task panicked"
                    );
                }
                Err(join_err) => {
                    // JoinError that is neither panic nor cancel-by-user
                    // shouldn't happen in our usage (we never call
                    // `abort`). Treat defensively as terminal.
                    return Err(Report::new(UtilsError::Concurrency)
                        .attach_printable(format!("supervised task [{name}] join failed"))
                        .attach_printable(format!("{join_err}")));
                }
            }

            // Record and prune the rolling history before the budget
            // check so callers see an accurate "restarts in window".
            let now = Instant::now();
            restart_history.push_back(now);
            if let Some(window) = self.restart_window {
                while let Some(&front) = restart_history.front() {
                    if now.duration_since(front) > window {
                        restart_history.pop_front();
                    } else {
                        break;
                    }
                }
            }

            if let Some(cap) = self.max_restarts
                && restart_history.len() as u32 > cap.get()
            {
                let window_note = self
                    .restart_window
                    .map(|w| format!(" within {w:?}"))
                    .unwrap_or_default();
                return Err(Report::new(UtilsError::RetryExhausted).attach_printable(format!(
                    "supervisor [{name}] exhausted restart budget: {count} restarts{window_note} exceeds cap {cap}",
                    count = restart_history.len(),
                    cap = cap.get(),
                )));
            }

            consecutive = consecutive.saturating_add(1);
            let sleep_for = compute_backoff(
                self.base_backoff,
                self.max_backoff,
                consecutive,
                self.jitter,
            );

            if !sleep_for.is_zero() {
                tracing::debug!(
                    supervisor = name,
                    backoff_ms = sleep_for.as_millis() as u64,
                    consecutive,
                    "sleeping before restart"
                );
                // Interruptible sleep: a shutdown signal during the
                // backoff window aborts the loop immediately instead
                // of waiting out the full delay.
                tokio::select! {
                    _ = tokio::time::sleep(sleep_for) => {},
                    _ = token.cancelled() => return Ok(()),
                }
            }
        }
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the next backoff: `base * 2^(attempt-1)`, capped by `max`,
/// optionally passed through full jitter.
fn compute_backoff(base: Duration, max: Duration, attempt: u32, jitter: bool) -> Duration {
    // `2^(attempt-1)` saturates at `attempt >= 33` for `u32`, which is
    // well past what any reasonable cap would tolerate.
    let factor = 1u32.checked_shl(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    let uncapped = base.saturating_mul(factor);
    let capped = uncapped.min(max);
    if jitter { full_jitter(capped) } else { capped }
}

/// Draw a duration uniformly from `[0, capped]` using the current
/// time's nanoseconds as a cheap entropy source.
///
/// Duplicated from `network::retry` on purpose — each sub-module of
/// the crate is meant to stand alone, so `concurrency::supervisor`
/// does not reach across the tree for a private helper. Not
/// cryptographically random — adequate for backoff jitter.
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

/// Best-effort panic message extraction from a [`tokio::task::JoinError`].
fn panic_message(err: &tokio::task::JoinError) -> String {
    // `JoinError::into_panic` consumes the error, so we cannot use it
    // here — we want to keep the `Debug` of the original error too.
    // The `Display` impl of `JoinError` already renders "task panicked"
    // and includes the payload when available; it is good enough for
    // logs and attachments.
    format!("{err}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A supervised task that always returns `Err` gets restarted
    /// until the budget is exhausted, then the supervisor returns
    /// `RetryExhausted`.
    #[tokio::test]
    async fn restart_budget_is_enforced() {
        let calls = Arc::new(AtomicU32::new(0));
        let token = CancellationToken::new();

        let factory_calls = Arc::clone(&calls);
        let outcome = Supervisor::new()
            .name("always-fails")
            .max_restarts(NonZeroU32::new(3).unwrap())
            .base_backoff(Duration::from_millis(1))
            .max_backoff(Duration::from_millis(5))
            .jitter(false)
            .run(token, move |_tok| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(Report::new(UtilsError::Internal)
                        .attach_printable("synthetic failure"))
                }
            })
            .await;

        let err = outcome.expect_err("budget must be exhausted");
        assert!(matches!(err.current_context(), UtilsError::RetryExhausted));
        // initial run + 3 restarts = 4 calls; the 4th restart is the
        // one that overflows the budget.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    /// A clean return with `restart_on_ok = false` stops the
    /// supervisor after the first run.
    #[tokio::test]
    async fn restart_on_ok_disabled_stops_after_one_run() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        Supervisor::new()
            .restart_on_ok(false)
            .run(CancellationToken::new(), move |_tok| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .await
            .expect("Ok exit must be treated as terminal");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Cancelling the token during a run makes the supervisor return
    /// `Ok(())` once the cooperative task observes the token.
    #[tokio::test]
    async fn cancellation_stops_the_loop() {
        let token = CancellationToken::new();
        let token_for_trigger = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            token_for_trigger.cancel();
        });

        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        Supervisor::new()
            .run(token, move |tok| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tok.cancelled().await;
                    Ok(())
                }
            })
            .await
            .expect("cancellation must not surface as an error");

        // Only one run — cancellation ends the loop before any restart.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A panic is caught, logged, and triggers a restart when
    /// `restart_on_panic` is enabled (the default).
    #[tokio::test]
    async fn panic_restarts_by_default() {
        let calls = Arc::new(AtomicU32::new(0));
        let factory_calls = Arc::clone(&calls);

        let outcome = Supervisor::new()
            .max_restarts(NonZeroU32::new(2).unwrap())
            .base_backoff(Duration::from_millis(1))
            .max_backoff(Duration::from_millis(2))
            .jitter(false)
            .run(CancellationToken::new(), move |_tok| {
                let calls = Arc::clone(&factory_calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    panic!("synthetic panic");
                    #[allow(unreachable_code)]
                    Ok(())
                }
            })
            .await;

        let err = outcome.expect_err("budget must be exhausted");
        assert!(matches!(err.current_context(), UtilsError::RetryExhausted));
        // initial + 2 restarts = 3 panics before the 3rd restart
        // push exceeds the cap.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// With `restart_on_panic = false`, the first panic terminates
    /// the supervisor with `UtilsError::Concurrency`.
    #[tokio::test]
    async fn panic_terminates_when_disabled() {
        let outcome = Supervisor::new()
            .restart_on_panic(false)
            .run(CancellationToken::new(), move |_tok| async move {
                panic!("synthetic panic");
                #[allow(unreachable_code)]
                Ok(())
            })
            .await;

        let err = outcome.expect_err("panic must terminate supervisor");
        assert!(matches!(err.current_context(), UtilsError::Concurrency));
    }

    /// Backoff grows exponentially up to the cap.
    #[test]
    fn backoff_saturates_at_cap_without_jitter() {
        let base = Duration::from_millis(100);
        let cap = Duration::from_secs(1);
        assert_eq!(compute_backoff(base, cap, 1, false), Duration::from_millis(100));
        assert_eq!(compute_backoff(base, cap, 2, false), Duration::from_millis(200));
        assert_eq!(compute_backoff(base, cap, 3, false), Duration::from_millis(400));
        assert_eq!(compute_backoff(base, cap, 4, false), Duration::from_millis(800));
        // 5 -> 1600ms, capped at 1000ms
        assert_eq!(compute_backoff(base, cap, 5, false), cap);
        // extreme attempt count still saturates, does not overflow
        assert_eq!(compute_backoff(base, cap, 1_000, false), cap);
    }
}
