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
| `RetryingClient::with_cancellation`         | fn       | Attach a `CancellationToken` so backoff sleeps abort on shutdown instead of stalling for the full `Retry-After` or exponential delay.                                                                                                                          |
| `RetryingClient::execute`                   | async fn | Send the request; on retriable failures, replay it respecting `Retry-After` or exponential backoff. Always returns the last observed result — a request that exhausts retries on a retriable status still yields `Ok(response)` so the caller can inspect it.   |
| `RetryingClient::inner` / `policy`          | fn       | Accessors for the wrapped executor and the active policy. **Calling through `inner` bypasses the retry policy.**                                                                                                                                                |

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

#### Input validation (`network::security`)

DoS-resistant JSON ingestion. `validate_and_parse_json` enforces a raw
payload-size cap and a JSON nesting-depth cap **before** the bytes reach
`serde_json`, so a single oversized or pathologically deep input cannot
tie up a worker. The depth scan tracks string literals and escaped
quotes, so brackets inside strings are ignored.

| Item                       | Kind | Summary                                                                                                                                                                                                                         |
| -------------------------- | ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `validate_and_parse_json`  | fn   | Reject oversize payloads or JSON that nests beyond a depth cap, then deserialize into `T: DeserializeOwned`. Returns a `UtilsResult<T>`; all failure modes classify as `UtilsError::Internal` with a distinguishing attachment. |

Re-exported at `rust_utils::network::validate_and_parse_json` for
convenience.

```rust,no_run
use rust_utils::network::validate_and_parse_json;
use rust_utils::error::UtilsResult;

fn decode(payload: &[u8]) -> UtilsResult<serde_json::Value> {
    validate_and_parse_json(payload, /* max_body = */ 1024 * 1024, /* max_depth = */ 32)
}
```

#### NATS (`network::nats`)

Typed request/reply on top of [`async_nats`](https://docs.rs/async-nats). JSON
payloads, optional TLS from a PEM bundle, and a concurrent handler loop that
classifies errors through [`UtilsResult`](#error-handling).

Message types are declared **per method** rather than on the struct: a single
`NatsClient` can request on one subject with types `(Req, Resp)` and serve
another subject with unrelated types. Handlers return `UtilsResult<Resp>`, so
business errors log + drop (no reply) instead of being papered over with
infallible responses.

| Item                                 | Kind     | Summary                                                                                                                                                                                                                                                               |
| ------------------------------------ | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `NatsCredentials`                    | struct   | User/password pair consumed by `NatsConnectOptions::credentials`. `password` is a `Secret<String>` so it is redacted in `Debug`/`Display`.                                                                                                                            |
| `NatsConnectOptions`                 | struct   | Builder for connection options (`url`, `credentials`, `tls_root_certs`, `protocol_request_timeout`). Fluent setters; private fields so new options can be added without breaking callers.                                                                             |
| `SubscriptionConfig`                 | struct   | Runtime config for `serve_request_reply`. Private fields + fluent setters. Defaults: 16 concurrent handlers, 1 MiB per-message cap, no queue group.                                                                                                                   |
| `SubscriptionConfig::new`            | fn       | Build an explicit config: `max_concurrency: Option<NonZeroUsize>` (`None` = unbounded) and a `max_payload_bytes` cap.                                                                                                                                                  |
| `SubscriptionConfig::max_concurrency` / `max_payload_bytes` | fn | Fluent setters that override the corresponding defaults.                                                                                                                                                                                                               |
| `SubscriptionConfig::queue_group`    | fn       | Fluent setter that opts into a NATS **queue group**: subscribers sharing a group name have messages load-balanced among them by the server (exactly one member receives each message) instead of every subscriber getting a copy.                                     |
| `NatsClient`                         | struct   | Thin wrapper around `async_nats::Client`. `Clone`-cheap (refcounted).                                                                                                                                                                                                 |
| `NatsClient::connect`                | async fn | Establish a connection from a `NatsConnectOptions`. Installs the rustls `ring` crypto provider idempotently when TLS is requested.                                                                                                                                    |
| `NatsClient::from_async_nats`        | fn       | Wrap an existing `async_nats::Client`. Lets callers manage `ConnectOptions` themselves or share one client across multiple wrappers.                                                                                                                                  |
| `NatsClient::inner`                  | fn       | Borrow the underlying `async_nats::Client` for features not surfaced here (JetStream, headers, flush, …).                                                                                                                                                             |
| `NatsClient::publish`                | async fn | JSON-encode `msg` and publish on `subject`. Fire-and-forget; returns once the publish is flushed.                                                                                                                                                                     |
| `NatsClient::request`                | async fn | Serialize, send, await the reply and deserialize into `Resp`. Serialize errors → `UtilsError::Internal`, transport errors → `UtilsError::Network`, decode errors of the reply → `UtilsError::Internal` (with the raw size attached for debugging).                    |
| `NatsClient::serve_request_reply`    | async fn | Subscribe to `subject` and fan out incoming messages through `handler` concurrently (capped by `SubscriptionConfig`). Replies only when the message carries a `reply` subject; handler errors and decode failures are logged + dropped so a bad message never crashes the loop. |
| `install_default_crypto_provider`    | fn       | Idempotently install the rustls `ring` crypto provider. Exposed for callers that bypass `connect` and build their own `ConnectOptions`.                                                                                                                               |

Example — acting as both a requester and a responder:

```rust,no_run
use rust_utils::error::{UtilsReport, UtilsResult};
use rust_utils::network::nats::{NatsClient, NatsConnectOptions, SubscriptionConfig};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Echo { payload: String }

# async fn run() -> UtilsResult<()> {
let client = NatsClient::connect(
    NatsConnectOptions::new("nats://localhost:4222"),
).await?;

// Server role: spawn a handler loop on `svc.echo`.
let serving = client.clone();
tokio::spawn(async move {
    let _ = serving
        .serve_request_reply(
            "svc.echo",
            SubscriptionConfig::default(),
            |req: Echo| async move { Ok::<_, UtilsReport>(req) },
        )
        .await;
});

// Client role: issue a request.
let reply: Echo = client
    .request("svc.echo", &Echo { payload: "ping".into() })
    .await?;
assert_eq!(reply.payload, "ping");
# Ok(())
# }
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
| `ShutdownController`                    | struct   | Graceful-shutdown coordinator that bundles a `tokio_util::sync::CancellationToken` with a `tokio_util::task::TaskTracker`. `Clone`-cheap (both primitives are refcounted); every clone shares the same cancellation state and contributes to the same task set. |
| `ShutdownController::new` / `default`   | fn       | Build a fresh controller with an un-cancelled token and an empty tracker.                                                                                                                       |
| `ShutdownController::token`             | fn       | Clone of the internal `CancellationToken` — hand it to workers so they can `select!` on `token.cancelled()`.                                                                                    |
| `ShutdownController::tracker`           | fn       | Clone of the internal `TaskTracker` for callers that want to spawn/track tasks through the `TaskTracker` API directly.                                                                          |
| `ShutdownController::spawn`             | fn       | Convenience: `tokio::spawn` a future and track it under this controller. The task is **isolated** — a panic or error does not affect siblings. Panics if called outside a Tokio runtime.        |
| `ShutdownController::spawn_critical`    | fn       | Spawn a task whose failure cancels the controller's token, triggering shutdown of every sibling. Task must return `UtilsResult<()>`. `Err` and panics are logged at `error` and propagated as a cancel; `Ok(())` is a clean exit. Useful for leader-election, heartbeats, main event loops. |
| `ShutdownController::trigger`           | fn       | Cancel the token so cooperating workers wind down. Idempotent.                                                                                                                                  |
| `ShutdownController::is_shutdown`       | fn       | Whether `trigger` (or an equivalent on a clone) has been called.                                                                                                                                |
| `ShutdownController::shutdown`          | async fn | Trigger, close the tracker, and await every tracked task bounded by a timeout. Returns `UtilsResult<()>`; a timeout surfaces as `UtilsError::Concurrency` with the count of still-running tasks attached. |
| `ShutdownController::trigger_on_signal` | fn       | Feature-gated (`signal`). Spawns a tracked listener that triggers shutdown on `SIGINT`/`SIGTERM` (Unix) or Ctrl+C (Windows).                                                                    |
| `Supervisor`                            | struct   | Restart-policy loop for a long-running Tokio task. Catches panics via `JoinSet`, reclassifies them, and restarts with exponential backoff + optional full jitter. Cooperative shutdown via `CancellationToken` — the token is handed to every restarted instance. |
| `Supervisor::new` / `default`           | fn       | Build with the default policy: unlimited restarts, 200ms base backoff, 30s cap, jitter on, restart on Ok/Err/panic.                                                                             |
| `Supervisor::name`                      | fn       | Optional label emitted by `tracing` events from the supervision loop.                                                                                                                           |
| `Supervisor::max_restarts`              | fn       | Cap on the number of restarts before giving up. Takes `NonZeroU32`.                                                                                                                             |
| `Supervisor::restart_window`            | fn       | Rolling time window for the restart counter — only restarts within the window count towards the cap, so sporadic failures do not exhaust the budget the way a crash-loop does.                 |
| `Supervisor::base_backoff` / `max_backoff` | fn    | Exponential backoff between restarts: `base * 2^(consecutive-1)` capped at `max_backoff`.                                                                                                       |
| `Supervisor::jitter`                    | fn       | Enable or disable full jitter (`[0, computed]`) on the backoff. Default `true` — desynchronises crash-looping replicas.                                                                         |
| `Supervisor::restart_on_ok`             | fn       | Whether to restart when the task returns `Ok(())`. Default `true`.                                                                                                                              |
| `Supervisor::restart_on_panic`          | fn       | Whether to restart when the task panics. Default `true`. When disabled, a panic terminates the supervisor with `UtilsError::Concurrency`.                                                       |
| `Supervisor::run`                       | async fn | Drive the loop. Takes a `CancellationToken` (cloned into every restarted instance) and a factory `Fn(CancellationToken) -> Fut`. Returns `UtilsResult<()>`; budget exhaustion surfaces as `UtilsError::RetryExhausted` with the last error chained in. |

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

Example — graceful shutdown:

```rust,no_run
use std::time::Duration;
use rust_utils::concurrency::ShutdownController;

# async fn run() -> rust_utils::UtilsResult<()> {
let controller = ShutdownController::new();

// Spawn a cooperating worker that exits when cancellation fires.
let token = controller.token();
controller.spawn(async move {
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                // … periodic work …
            }
        }
    }
});

// With the `signal` feature enabled, a single call wires SIGINT/SIGTERM
// to `trigger()`; without it, call `controller.trigger()` yourself when
// the process decides to stop.
# #[cfg(feature = "signal")]
controller.trigger_on_signal();

// Trigger shutdown and wait for in-flight work, bounded by a timeout.
// A timeout surfaces as `UtilsError::Concurrency` with the count of
// still-running tasks attached.
controller.shutdown(Duration::from_secs(30)).await?;
# Ok(())
# }
```

Example — supervise a long-running task:

```rust,no_run
use std::num::NonZeroU32;
use std::time::Duration;
use rust_utils::concurrency::{ShutdownController, Supervisor};

# async fn run() -> rust_utils::UtilsResult<()> {
let ctrl = ShutdownController::new();
let token = ctrl.token();

// Supervise a subscriber loop. If it exits (error, panic, or clean
// return) the supervisor restarts it with exponential backoff until
// either the budget is exhausted or shutdown is triggered.
ctrl.spawn(async move {
    let _ = Supervisor::new()
        .name("nats-subscriber")
        .max_restarts(NonZeroU32::new(10).unwrap())
        .restart_window(Duration::from_secs(60))
        .base_backoff(Duration::from_millis(200))
        .max_backoff(Duration::from_secs(30))
        .run(token, |tok| async move {
            // Run the service; observe `tok` for cooperative shutdown.
            let _ = tok;
            Ok(())
        })
        .await;
});
# Ok(())
# }
```

## Feature flags

| Flag     | What it enables                                                                                                                                            |
| -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `signal` | `ShutdownController::trigger_on_signal`, which spawns a tracked task that triggers shutdown on `SIGINT`/`SIGTERM` (Unix) or Ctrl+C (Windows). Off by default. |

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
