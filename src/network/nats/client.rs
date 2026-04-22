//! Typed, error-stack-aware wrapper over [`async_nats::Client`].

use std::future::Future;
use std::sync::Arc;

use async_nats::{Client, ConnectOptions};
use error_stack::ResultExt;
use futures::stream::StreamExt;
use serde::{Serialize, de::DeserializeOwned};

use super::config::{NatsConnectOptions, SubscriptionConfig};
use crate::error::{UtilsError, UtilsResult};

/// Thin wrapper around [`async_nats::Client`] that exposes a typed
/// request/reply surface with JSON payloads, a payload-size guard on
/// the serve side, and error-stack classification.
///
/// The underlying `async_nats::Client` is `Clone` (cheap, internally
/// refcounted), so [`NatsClient`] is also cheaply cloneable and can
/// be shared across tasks.
///
/// Generics are declared per method rather than on the struct, so a
/// single client can handle multiple subjects with unrelated message
/// types.
///
/// # Example
///
/// ```no_run
/// # use rust_utils::network::nats::{NatsClient, NatsConnectOptions, SubscriptionConfig};
/// # use rust_utils::error::UtilsResult;
/// # async fn run() -> UtilsResult<()> {
/// let client = NatsClient::connect(
///     NatsConnectOptions::new("nats://localhost:4222")
/// ).await?;
///
/// // Request/reply as a client:
/// let reply: serde_json::Value = client
///     .request("svc.echo", &serde_json::json!({ "ping": true }))
///     .await?;
///
/// // Serve requests as a handler:
/// client.serve_request_reply(
///     "svc.echo",
///     SubscriptionConfig::default(),
///     |req: serde_json::Value| async move {
///         Ok::<_, rust_utils::error::UtilsReport>(req)
///     },
/// ).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct NatsClient {
    client: Client,
}

impl NatsClient {
    /// Connect to a NATS server using the provided options.
    ///
    /// When [`NatsConnectOptions::tls_root_certs`] is set, the rustls
    /// `ring` crypto provider is installed as the process default.
    ///
    /// # Errors
    /// * [`UtilsError::Network`] when the TCP/TLS connection or the
    ///   initial NATS handshake fails.
    pub async fn connect(opts: NatsConnectOptions) -> UtilsResult<Self> {
        let mut conn = ConnectOptions::new().request_timeout(Some(opts.protocol_request_timeout));

        if let Some(creds) = opts.credentials {
            conn = conn.user_and_password(creds.user, creds.password);
        }

        if let Some(path) = opts.tls_root_certs {
            install_default_crypto_provider();
            conn = conn.add_root_certificates(path).require_tls(true);
        }

        let url = opts.url;
        let client = async_nats::connect_with_options(&url, conn)
            .await
            .change_context(UtilsError::Network)
            .attach_printable_lazy(|| format!("nats url: {url}"))?;

        Ok(Self { client })
    }

    /// Wrap an existing [`async_nats::Client`]. Useful when the caller
    /// wants to manage [`ConnectOptions`] directly or already holds a
    /// shared client.
    pub fn from_async_nats(client: Client) -> Self {
        Self { client }
    }

    /// Borrow the underlying [`async_nats::Client`] for operations not
    /// exposed by this wrapper (JetStream, headers, flush, …).
    pub fn inner(&self) -> &Client {
        &self.client
    }

    /// Publish a JSON-encoded message on `subject`. Does not wait for
    /// any acknowledgement — NATS publish is fire-and-forget.
    ///
    /// # Errors
    /// * [`UtilsError::Internal`] when `msg` cannot be serialized.
    /// * [`UtilsError::Network`] when the server-bound publish fails.
    pub async fn publish<Msg>(&self, subject: impl Into<String>, msg: &Msg) -> UtilsResult<()>
    where
        Msg: Serialize,
    {
        let subject = subject.into();
        let payload = serde_json::to_vec(msg)
            .change_context(UtilsError::Internal)
            .attach_printable("failed to serialize outbound NATS payload")?;

        self.client
            .publish(subject.clone(), payload.into())
            .await
            .change_context(UtilsError::Network)
            .attach_printable_lazy(|| format!("nats subject: {subject}"))
    }

    /// Send `msg` as a NATS request and deserialize the reply.
    ///
    /// # Errors
    /// * [`UtilsError::Internal`] when serializing the request or
    ///   deserializing the reply fails.
    /// * [`UtilsError::Network`] when the request transport itself
    ///   fails (no responders, timeout, connection drop).
    pub async fn request<Req, Resp>(
        &self,
        subject: impl Into<String>,
        msg: &Req,
    ) -> UtilsResult<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let subject = subject.into();
        let payload = serde_json::to_vec(msg)
            .change_context(UtilsError::Internal)
            .attach_printable("failed to serialize NATS request")?;

        let reply = self
            .client
            .request(subject.clone(), payload.into())
            .await
            .change_context(UtilsError::Network)
            .attach_printable_lazy(|| format!("nats subject: {subject}"))?;

        serde_json::from_slice::<Resp>(&reply.payload)
            .change_context(UtilsError::Internal)
            .attach_printable_lazy(|| {
                format!(
                    "failed to deserialize NATS reply on subject {subject} ({} bytes)",
                    reply.payload.len()
                )
            })
    }

    /// Subscribe to `subject` and process each incoming message
    /// concurrently via `handler`.
    ///
    /// When [`SubscriptionConfig::queue_group`] is `Some`, the
    /// underlying subscription is a **queue subscribe** — multiple
    /// subscribers sharing that group name will have messages
    /// load-balanced among them by the server, instead of each
    /// receiving a copy. See [`SubscriptionConfig`] for the full
    /// semantics.
    ///
    /// Each message is:
    /// 1. **Size-checked** against
    ///    [`SubscriptionConfig::max_payload_bytes`] — oversized
    ///    messages are dropped with a `tracing::warn`.
    /// 2. **Deserialized** into `Req` — decode failures are logged at
    ///    `error` and dropped.
    /// 3. Passed to `handler`; if it returns `Ok`, the result is
    ///    serialized and published to the message's `reply` subject
    ///    (when present). If the message has no `reply` subject it is
    ///    treated as fire-and-forget and no response is sent. If the
    ///    handler returns `Err`, the report is logged and no reply is
    ///    produced (the peer will observe a timeout).
    ///
    /// The future resolves when the subscription stream ends (e.g.
    /// the server closes the connection). For long-running servers
    /// this is typically spawned onto a dedicated task:
    ///
    /// ```no_run
    /// # use rust_utils::network::nats::{NatsClient, NatsConnectOptions, SubscriptionConfig};
    /// # async fn run(client: NatsClient) {
    /// tokio::spawn(async move {
    ///     let _ = client
    ///         .serve_request_reply(
    ///             "svc.echo",
    ///             SubscriptionConfig::default(),
    ///             |req: serde_json::Value| async move {
    ///                 Ok::<_, rust_utils::error::UtilsReport>(req)
    ///             },
    ///         )
    ///         .await;
    /// });
    /// # }
    /// ```
    ///
    /// # Errors
    /// * [`UtilsError::Network`] when the initial `subscribe` call
    ///   fails. Per-message failures do not abort the loop.
    pub async fn serve_request_reply<Req, Resp, F, Fut>(
        &self,
        subject: impl Into<String>,
        config: SubscriptionConfig,
        handler: F,
    ) -> UtilsResult<()>
    where
        Req: DeserializeOwned + Send + 'static,
        Resp: Serialize + Send + 'static,
        F: Fn(Req) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = UtilsResult<Resp>> + Send + 'static,
    {
        let subject_name = subject.into();
        // Destructure once so we can move `queue_group` (a `String`)
        // out of the config without cloning, while still reading the
        // other two fields as plain values.
        let SubscriptionConfig {
            max_concurrency,
            max_payload_bytes,
            queue_group,
        } = config;

        // Queue subscribe vs plain subscribe is decided here: the
        // only difference on the wire is whether the SUB frame
        // carries a queue-group name, so the downstream pipeline is
        // identical either way.
        let subscriber = match queue_group {
            Some(group) => self
                .client
                .queue_subscribe(subject_name.clone(), group.clone())
                .await
                .change_context(UtilsError::Network)
                .attach_printable_lazy(|| {
                    format!("nats subject: {subject_name} (queue group: {group})")
                })?,
            None => self
                .client
                .subscribe(subject_name.clone())
                .await
                .change_context(UtilsError::Network)
                .attach_printable_lazy(|| format!("nats subject: {subject_name}"))?,
        };

        // Shared state moved into the concurrent stream handler. The
        // client is cheap to clone (refcounted); the handler and the
        // subject string are wrapped in `Arc` so every concurrent
        // call gets a cheap shared reference instead of per-message
        // allocations.
        let client = self.client.clone();
        let handler = Arc::new(handler);
        let subject_name = Arc::new(subject_name);
        let concurrency = if max_concurrency == 0 {
            None
        } else {
            Some(max_concurrency)
        };

        subscriber
            .for_each_concurrent(concurrency, move |message| {
                let client = client.clone();
                let handler = Arc::clone(&handler);
                let subject_name = Arc::clone(&subject_name);
                async move {
                    handle_message::<Req, Resp, F, Fut>(
                        &client,
                        &subject_name,
                        max_payload_bytes,
                        &handler,
                        message,
                    )
                    .await;
                }
            })
            .await;

        Ok(())
    }
}

/// Per-message pipeline extracted from the `for_each_concurrent`
/// closure so the control flow (size check → decode → run → reply)
/// reads top-to-bottom without deeply nested matches.
async fn handle_message<Req, Resp, F, Fut>(
    client: &Client,
    subject_name: &str,
    max_payload_bytes: usize,
    handler: &F,
    message: async_nats::Message,
) where
    Req: DeserializeOwned,
    Resp: Serialize,
    F: Fn(Req) -> Fut,
    Fut: Future<Output = UtilsResult<Resp>>,
{
    if message.payload.len() > max_payload_bytes {
        tracing::warn!(
            subject = %subject_name,
            payload_bytes = message.payload.len(),
            max_payload_bytes,
            "dropping oversized NATS message"
        );
        return;
    }

    let req: Req = match serde_json::from_slice(&message.payload) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!(
                subject = %subject_name,
                error = %err,
                "failed to deserialize NATS message"
            );
            return;
        }
    };

    let resp = match handler(req).await {
        Ok(resp) => resp,
        Err(report) => {
            tracing::error!(
                subject = %subject_name,
                error = ?report,
                "NATS handler returned an error"
            );
            return;
        }
    };

    let Some(reply) = message.reply else {
        // Fire-and-forget subject — handler already did the work.
        return;
    };

    let payload = match serde_json::to_vec(&resp) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(
                subject = %subject_name,
                reply = %reply,
                error = %err,
                "failed to serialize NATS reply"
            );
            return;
        }
    };

    if let Err(err) = client.publish(reply.clone(), payload.into()).await {
        tracing::error!(
            subject = %subject_name,
            reply = %reply,
            error = %err,
            "failed to publish NATS reply"
        );
    }
}

/// Install the rustls `ring` crypto provider as the process default
/// if none has been installed yet. No-op when a default is already
/// present.
///
/// Exposed publicly because callers that bypass
/// [`NatsClient::connect`] and build their own `ConnectOptions`
/// still need a default provider installed before opening a TLS
/// connection.
pub fn install_default_crypto_provider() {
    // `install_default` returns `Err(provider)` when a default is
    // already installed; we deliberately ignore that — callers who
    // want a specific provider should install it themselves before
    // this runs.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
