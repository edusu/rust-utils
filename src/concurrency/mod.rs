//! Concurrency utilities built on top of the `tokio` runtime.
//!
//! * [`WorkerPool`](worker_pool::WorkerPool) — bounded-concurrency fan-out
//!   with backpressure on submission.
//! * [`Throttle`](throttle::Throttle) — leading-edge rate limiter that
//!   discards calls arriving within a cooldown window.

pub mod throttle;
pub mod worker_pool;

pub use throttle::Throttle;
pub use worker_pool::WorkerPool;
