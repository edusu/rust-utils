//! NATS integration helpers built on top of [`async_nats`].
//!
//! The [`NatsClient`] wraps [`async_nats::Client`] with:
//!
//! * Typed request/reply and publish helpers that JSON-encode bodies
//!   under the hood and surface failures through
//!   [`UtilsResult`](crate::error::UtilsResult).
//! * Optional TLS setup from a PEM root-cert bundle, with the rustls
//!   `ring` crypto provider installed idempotently.
//! * A concurrent `serve_request_reply` loop that guards against
//!   oversized payloads, tolerates per-message failures, and replies
//!   only when the incoming message carries a `reply` subject.
//!
//! # Design notes
//! Message types are declared **per method** rather than on the
//! struct: a single [`NatsClient`] can request on one subject with
//! `(Req, Resp)` types and serve another subject with unrelated
//! types, without juggling two differently-parameterised wrappers.

mod client;
mod config;

pub use client::{NatsClient, install_default_crypto_provider};
pub use config::{NatsConnectOptions, NatsCredentials, SubscriptionConfig};
