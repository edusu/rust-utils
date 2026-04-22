//! Concurrency utilities built on top of the `tokio` runtime.
//!
//! * [`WorkerPool`](worker_pool::WorkerPool) — bounded-concurrency fan-out
//!   with backpressure on submission.
//! * [`Throttle`](throttle::Throttle) — leading-edge rate limiter that
//!   discards calls arriving within a cooldown window.
//! * [`ShutdownController`](shutdown::ShutdownController) — graceful
//!   shutdown coordinator that bundles a `CancellationToken` and a
//!   `TaskTracker` with a bounded-wait join.
//! * [`Supervisor`](supervisor::Supervisor) — restart-policy loop for
//!   long-running tasks, with backoff, budget, and panic containment.

pub mod shutdown;
pub mod supervisor;
pub mod throttle;
pub mod worker_pool;

pub use shutdown::ShutdownController;
pub use supervisor::Supervisor;
pub use throttle::Throttle;
pub use worker_pool::WorkerPool;
