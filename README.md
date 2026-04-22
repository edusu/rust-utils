# rust-utils

A personal library crate that collects reusable data structures and helpers
intended to be depended on from other Rust projects. The goal is to avoid
re-implementing the same small pieces of plumbing (HTTP clients with rate
limiting and retry, bounded worker pools, redacted secrets, …) in every new
project.

## Usage

```toml
[dependencies]
rust-utils = { git = "…", branch = "main" }
# or, locally:
rust-utils = { path = "../rust-utils" }
```

## Error handling

Every fallible function in the crate returns `UtilsResult<T>` — an alias for
`Result<T, error_stack::Report<UtilsError>>`. `UtilsError` is a small,
coarse-grained taxonomy of failures; context (underlying `reqwest::Error`,
URLs, status codes, …) rides along as attachments on the `Report`.

| Item           | Kind    | Summary                                                                                                                          |
| -------------- | ------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `UtilsError`   | enum    | `#[non_exhaustive]` taxonomy: `Network`, `Http`, `Config`, `RetryExhausted`, `Concurrency`, `Internal`. Derived with `thiserror`. |
| `UtilsReport`  | alias   | `error_stack::Report<UtilsError>`.                                                                                               |
| `UtilsResult`  | alias   | `Result<T, UtilsReport>`.                                                                                                        |

The variants are re-exported at the crate root, so typical usage is:

```rust
use rust_utils::{UtilsError, UtilsResult};
use error_stack::ResultExt;

fn parse_port(raw: &str) -> UtilsResult<u16> {
    raw.parse::<u16>()
        .map_err(error_stack::Report::from)
        .change_context(UtilsError::Config)
        .attach_printable_lazy(|| format!("raw input: {raw:?}"))
}
```

Consumers that need to branch on failure should match on
`report.current_context()`, not on the underlying dependency error.

## Modules

### `network`

HTTP utilities built on top of [`reqwest`](https://docs.rs/reqwest) and
[`governor`](https://docs.rs/governor) (GCRA algorithm). The module is
split into two orthogonal layers that can be used independently or
stacked: **rate limiting** (proactive pacing of outgoing requests) and
**retry** (reactive handling of failed responses).

Recommended stacking order when combining both:

```text
RetryingClient → RateLimitedClient → reqwest::Client
```

so that every retry attempt also consumes a cell from the internal rate
limiter.

#### Rate limiting

| Item                                       | Kind     | Summary                                                                                                                                                                                        |
| ------------------------------------------ | -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `RateLimitWindow`                          | enum     | Declarative time window for a rate limit: `PerSecond(n)`, `PerMinute(n)`, or `Custom { period }`. Marked `#[non_exhaustive]` so new variants can be added without breaking matches.            |
| `RateLimitWindow::from_string`             | fn       | Parse shorthand forms (`"10s"`, `"1m"`, `"2h"`, `"1d"`) into a window. Returns `None` when the string is empty, has a bad suffix, or a non-positive number.                                    |
| `RateLimitedClient`                        | struct   | `reqwest::Client` that waits on an internal `governor::RateLimiter` before every dispatch. Cheap to clone — all clones share the same limiter.                                                 |
| `RateLimitedClient::new`                   | fn       | Build a client using a default `reqwest::Client`. Returns `UtilsResult<Self>`; a zero-length `Custom { period }` surfaces as `UtilsError::Config`.                                             |
| `RateLimitedClient::with_client`           | fn       | Same, but on top of a user-provided `reqwest::Client` (custom timeouts, headers, TLS, …).                                                                                                      |
| `RateLimitedClient::execute`               | async fn | Wait for a slot, then dispatch the request. Classifies transport failures as `UtilsError::Network` (connect/timeout) or `UtilsError::Http` (everything else).                                  |
| `RateLimitedClient::wait_for_slot`         | async fn | Block until the limiter releases a slot. Useful when driving requests through the fluent `RequestBuilder` API.                                                                                 |
| `RateLimitedClient::inner_client`          | fn       | Access the underlying `reqwest::Client`. **Bypasses the limiter** — use with care.                                                                                                             |
| `Client`                                   | enum     | Thin fan-out over `RateLimited(RateLimitedClient)` and `Unrestricted(reqwest::Client)`. Lets call sites accept "any of our HTTP clients" without generics.                                     |
| `Client::execute` / `Client::inner_client` | fn       | Delegate to the chosen variant; `execute` shares the same error classification as `RateLimitedClient::execute`.                                                                                |

Example:

```rust
use std::num::NonZeroU32;
use rust_utils::network::{Client, RateLimitedClient, RateLimitWindow};

let limited = RateLimitedClient::new(
    RateLimitWindow::PerSecond(NonZeroU32::new(10).unwrap()),
    None,
)?;
let client = Client::RateLimited(limited);
let request = reqwest::Request::new(reqwest::Method::GET, "https://example.com/".parse()?);
let response = client.execute(request).await?; // UtilsResult<Response>
```

#### Retry

| Item                                        | Kind     | Summary                                                                                                                                                                                                                                                         |
| ------------------------------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `HttpExecutor`                              | trait    | Abstraction over anything that can execute a `reqwest::Request` and return a `UtilsResult<Response>`. Implemented for `reqwest::Client`, `Client`, `RateLimitedClient` and `RetryingClient` — which is what lets layers be stacked and enables mocking in tests. |
| `RetryPolicy`                               | struct   | Declarative retry configuration (max attempts, retriable statuses, network-error handling, idempotency rules, backoff, jitter). Built with fluent setters on top of `Default::default()`. Private fields, so new options can be added without breaking callers. |
| `RetryPolicy::max_attempts`                 | fn       | Total number of attempts (initial + retries). Takes `NonZeroU32`.                                                                                                                                                                                               |
| `RetryPolicy::retry_statuses`               | fn       | Replace the set of response statuses that trigger a retry. Defaults to `429` and `503`.                                                                                                                                                                         |
| `RetryPolicy::retry_network_errors`         | fn       | Toggle retry on transport failures (reports whose current context is `UtilsError::Network`). Default `true`.                                                                                                                                                    |
| `RetryPolicy::retry_non_idempotent`         | fn       | Opt in to retrying `POST`/`PATCH`. Default `false` (avoids duplicated side effects).                                                                                                                                                                            |
| `RetryPolicy::base_backoff` / `max_backoff` | fn       | Exponential backoff parameters: `base * 2^(attempt-1)`, capped by `max_backoff`. The cap also bounds server-sent `Retry-After` values.                                                                                                                          |
| `RetryPolicy::jitter`                       | fn       | Enable or disable full jitter (`[0, computed]`) on the computed backoff.                                                                                                                                                                                        |
| `RetryingClient<E>`                         | struct   | Wraps any `HttpExecutor` and retries according to a `RetryPolicy`. Also implements `HttpExecutor`, so layers can be stacked.                                                                                                                                    |
| `RetryingClient::new`                       | fn       | Wrap an executor with a policy.                                                                                                                                                                                                                                 |
| `RetryingClient::execute`                   | async fn | Send the request; on retriable failures, replay it respecting `Retry-After` or exponential backoff. Always returns the last observed result — a request that exhausts retries on a retriable status still yields `Ok(response)` so the caller can inspect it.   |
| `RetryingClient::inner` / `policy`          | fn       | Accessors for the wrapped executor and the active policy.                                                                                                                                                                                                       |

Notes:

- `Retry-After` is parsed as an integer number of seconds; the RFC 7231
  HTTP-date form is not supported.
- When the request body is not cloneable (e.g. a streaming upload), no
  retry is attempted and the first observed result is returned.
- Retry decisions inspect `report.current_context()`; transport failures
  classified by the executor as `UtilsError::Network` are retried when
  enabled, anything else (`Http`, `Config`, …) is surfaced immediately.

Example:

```rust
use std::num::NonZeroU32;
use rust_utils::network::{RateLimitedClient, RateLimitWindow, RetryingClient, RetryPolicy};

let limited = RateLimitedClient::new(
    RateLimitWindow::PerSecond(NonZeroU32::new(10).unwrap()),
    None,
)?;
let client = RetryingClient::new(
    limited,
    RetryPolicy::default().max_attempts(NonZeroU32::new(5).unwrap()),
);
let request = reqwest::Request::new(reqwest::Method::GET, "https://example.com/".parse()?);
let response = client.execute(request).await?; // UtilsResult<Response>
```

### `secret`

Opaque wrapper that redacts its inner value in `Debug` and `Display`
output, so API tokens, passwords and cryptographic keys do not leak
through `tracing`/`log` or stray `dbg!` calls.

| Item                   | Kind     | Summary                                                                                                                                                         |
| ---------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Secret<T>`            | struct   | Wraps a `T` and redacts it in `Debug` (`"Secret([REDACTED])"`) and `Display` (`"[REDACTED]"`). Implements `Clone` when `T: Clone`. Does not implement `serde`.   |
| `Secret::new`          | fn       | `const fn` constructor. `Secret::from(value)` also works via `From<T>`.                                                                                         |
| `Secret::expose`       | fn       | Borrow the inner value. Prefer short-lived access — the longer the reference lives, the higher the chance it gets formatted by mistake.                        |
| `Secret::into_inner`   | fn       | Consume the wrapper and return the inner value.                                                                                                                 |
| `Secret::ct_eq`        | fn       | Available when `T: AsRef<[u8]>`. Constant-time equality on the byte representation; lengths are not considered secret (mismatched lengths return `false` fast). |

Example:

```rust
use rust_utils::secret::Secret;

let token = Secret::new(String::from("sk_live_abcdef"));
assert_eq!(format!("{token}"), "[REDACTED]");
assert!(!format!("{token:?}").contains("sk_live"));
assert!(token.expose().starts_with("sk_"));
```

Not protected against: `serde` serialization (deliberately not
implemented), core dumps, `/proc/<pid>/mem`, or memory residue after
drop. For zero-on-drop semantics, wrap the inner value in `Zeroizing<T>`
from the `zeroize` crate before constructing the `Secret`.

### `concurrency`

Concurrency utilities built on top of the `tokio` runtime.

| Item                        | Kind     | Summary                                                                                                                                                                                         |
| --------------------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `WorkerPool<T>`             | struct   | Bounded-concurrency fan-out that runs an async handler over submitted jobs. Built around `tokio::sync::Semaphore` + `tokio::task::JoinSet`. Submission applies backpressure when the cap is hit. |
| `WorkerPool::new`           | fn       | Build a pool from a `NonZeroUsize` concurrency cap and an `Fn(T) -> Fut` handler (`Send + Sync + 'static`).                                                                                     |
| `WorkerPool::submit`        | async fn | Enqueue a job; awaits a free slot when the cap is saturated.                                                                                                                                    |
| `WorkerPool::join`          | async fn | Consume the pool and wait for every outstanding task to finish.                                                                                                                                  |
| `WorkerPool::active`        | fn       | Number of spawned tasks still tracked by the pool. Intended for metrics/tests, not scheduling.                                                                                                  |
| `Throttle`                  | struct   | Leading-edge throttle: admits at most one call per `min_interval`; calls arriving within the cooldown are *discarded*, not queued.                                                              |
| `Throttle::new`             | fn       | `const fn` constructor.                                                                                                                                                                         |
| `Throttle::try_run`         | async fn | Run the supplied closure if the window allows it and return `Some(result)`; otherwise return `None` without invoking the closure.                                                               |
| `Throttle::reset`           | fn       | Forget the last-run timestamp; the next call runs immediately.                                                                                                                                  |
| `Throttle::min_interval`    | fn       | Accessor for the configured window.                                                                                                                                                             |

`WorkerPool` vs. `Throttle`: the pool runs *every* submitted job and paces
by slot availability, while the throttle deliberately drops calls to
enforce "at most one per interval". For paced-but-not-dropped requests
against an HTTP endpoint, use `RateLimitedClient` instead.

Example — bounded fan-out:

```rust
use std::num::NonZeroUsize;
use rust_utils::concurrency::WorkerPool;

let mut pool = WorkerPool::new(NonZeroUsize::new(4).unwrap(), |job: u32| async move {
    // … do work with `job` …
    let _ = job;
});
for job in 0..100 {
    pool.submit(job).await;
}
pool.join().await;
```

Example — leading-edge throttle:

```rust
use std::time::Duration;
use rust_utils::concurrency::Throttle;

let throttle = Throttle::new(Duration::from_millis(500));
assert!(throttle.try_run(|| async { 1 }).await.is_some()); // runs
assert!(throttle.try_run(|| async { 2 }).await.is_none()); // dropped
```

## Commands

```bash
cargo check                                  # Fast type-check
cargo test                                   # All unit + doc tests
cargo test <name>                            # Single test by substring
cargo clippy --all-targets -- -D warnings    # Lint
cargo fmt                                    # Format
cargo doc --open                             # Build and open rustdoc
```

## Status

Early-stage and opinionated. The public API is expected to grow module by
module as concrete needs arise in downstream projects. Treat versions
below `1.0` as potentially breaking on minor bumps.
