# rust-utils

A personal library crate that collects reusable data structures and helpers
intended to be depended on from other Rust projects. The goal is to avoid
re-implementing the same small pieces of plumbing (HTTP clients with rate
limiting and retry, etc.) in every new project.

## Usage

```toml
[dependencies]
rust-utils = { git = "…", branch = "main" }
# or, locally:
rust-utils = { path = "../rust-utils" }
```

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

| Item                                       | Kind     | Summary                                                                                                                                                                             |
| ------------------------------------------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `RateLimitWindow`                          | enum     | Declarative time window for a rate limit: `PerSecond(n)`, `PerMinute(n)`, or `Custom { period }`. Marked `#[non_exhaustive]` so new variants can be added without breaking matches. |
| `RateLimitedClient`                        | struct   | `reqwest::Client` that waits on an internal `governor::RateLimiter` before every dispatch. Cheap to clone — all clones share the same limiter.                                      |
| `RateLimitedClient::new`                   | fn       | Build a client using a default `reqwest::Client`. Returns `Result<Self, BuildError>`.                                                                                               |
| `RateLimitedClient::with_client`           | fn       | Same, but on top of a user-provided `reqwest::Client` (custom timeouts, headers, TLS, …).                                                                                           |
| `RateLimitedClient::execute`               | async fn | Wait for a slot, then dispatch the request.                                                                                                                                         |
| `RateLimitedClient::wait_for_slot`         | async fn | Block until the limiter releases a slot. Useful when driving requests through the fluent `RequestBuilder` API.                                                                      |
| `RateLimitedClient::inner_client`          | fn       | Access the underlying `reqwest::Client`. **Bypasses the limiter** — use with care.                                                                                                  |
| `Client`                                   | enum     | Thin fan-out over `RateLimited(RateLimitedClient)` and `Unrestricted(reqwest::Client)`. Lets call sites accept "any of our HTTP clients" without generics.                          |
| `Client::execute` / `Client::inner_client` | fn       | Delegate to the chosen variant.                                                                                                                                                     |
| `BuildError`                               | enum     | Error returned when a `RateLimitedClient` cannot be constructed (e.g. `Custom` with a zero-length period).                                                                          |

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
let response = client.execute(request).await?;
```

#### Retry

| Item                                        | Kind     | Summary                                                                                                                                                                                                                                                         |
| ------------------------------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `HttpExecutor`                              | trait    | Abstraction over anything that can execute a `reqwest::Request`. Implemented for `reqwest::Client`, `Client`, `RateLimitedClient` and `RetryingClient` — which is what lets layers be stacked and enables mocking in tests.                                     |
| `RetryPolicy`                               | struct   | Declarative retry configuration (max attempts, retriable statuses, network-error handling, idempotency rules, backoff, jitter). Built with fluent setters on top of `Default::default()`. Private fields, so new options can be added without breaking callers. |
| `RetryPolicy::max_attempts`                 | fn       | Total number of attempts (initial + retries). Takes `NonZeroU32`.                                                                                                                                                                                               |
| `RetryPolicy::retry_statuses`               | fn       | Replace the set of response statuses that trigger a retry. Defaults to `429` and `503`.                                                                                                                                                                         |
| `RetryPolicy::retry_network_errors`         | fn       | Toggle retry on connect/timeout errors. Default `true`.                                                                                                                                                                                                         |
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
let response = client.execute(request).await?;
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
