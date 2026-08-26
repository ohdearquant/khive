//! Registerable MCP serving transports.
//!
//! A [`Transport`] owns the wire protocol used to serve the `request` surface.
//! Built-ins are registered via [`TransportRegistry::with_builtins`]; additional
//! transports (e.g. Streamable HTTP) register with [`TransportRegistry::register`]
//! before serving, so the serve path never hard-codes a transport enum.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::server::KhiveMcpServer;

/// Cancels rmcp's root service token before reporting transport EOF — and,
/// with an idle timeout configured, before reporting a synthetic EOF when no
/// message has arrived within that window even though the pipe itself stays
/// open (#1921).
///
/// rmcp otherwise classifies EOF as a graceful close and drains in-flight
/// handlers for five seconds without cancelling their per-request child
/// tokens. The same token is passed to `serve_with_ct`/`serve_directly_with_ct`,
/// so cancelling it here reaches every admitted handler before that drain
/// begins. Send and close remain transparent, allowing this wrapper to sit
/// outside the Unix post-flush self-heal transport without changing its flush
/// ordering.
///
/// An abandoned stdio bridge — a client whose pipe never closes but which
/// has stopped sending requests — is otherwise indistinguishable from a live
/// one: nothing reaps it, so it holds its reader-pool admission and DB
/// connection for as long as the parent process happens to stay alive
/// (observed up to tens of hours in production). Idle timeout treats "no
/// request for `idle_timeout`" the same as real EOF: `receive()` yields
/// `None`, which cancels `root` and lets rmcp's normal graceful-close path
/// tear the session down — the same clean-exit path that already releases
/// pooled resources on a real disconnect.
///
/// Wire-idle is not session-idle: rmcp keeps racing `transport.receive()`
/// while each admitted request runs in its own task, so a handler that
/// outlasts `idle_timeout` (slow verb, large batch) must not have its
/// session torn down out from under it. `in_flight` counts JSON-RPC
/// `Request` messages this transport has handed to rmcp minus the
/// `Response`/`Error` messages it has *finished writing* back for them —
/// not merely handed to `inner.send`, since rmcp spawns the response send
/// and the underlying framed write awaits the transport writer, so a slow
/// or stalled reader leaves the write pending well after `send` is called.
/// The idle branch only treats a quiet window as EOF when that count is
/// zero, i.e. nothing admitted has an undelivered response. A response
/// write that fails also decrements the counter (once resolved, one way or
/// another, it is no longer pending) — the broken transport will surface
/// through `receive()` returning `None` on its own, so this counter must
/// not be the thing that gets stuck open forever on a write error.
///
/// A response write that never resolves — a peer that admits a request,
/// then stops reading its response while keeping the pipe open — would
/// otherwise pin this session (and its reader-pool admission / DB
/// connection) forever: `in_flight` would never return to zero, so the idle
/// check above would defer indefinitely. `response_deadline` bounds the
/// write itself (see [`Self::send`]) rather than the idle check: rmcp's
/// underlying `AsyncRwTransport` serializes writes through a
/// `tokio::sync::Mutex` held across the pending write's `.await`, so a
/// write that never resolves also blocks any later `close()` on the same
/// transport — merely cancelling the root token would not free that lock.
/// Timing out the write future instead *drops* it, which releases the
/// mutex guard the same way any other future cancellation would, so a
/// subsequent `close()` (part of rmcp's normal post-cancellation drain)
/// can still proceed.
pub(crate) struct CancelOnEofTransport<T> {
    inner: T,
    root: tokio_util::sync::CancellationToken,
    idle_timeout: Option<std::time::Duration>,
    response_deadline: Option<std::time::Duration>,
    in_flight: Arc<AtomicI64>,
}

impl<T> CancelOnEofTransport<T> {
    /// `idle_timeout`: a `receive()` call that yields no message within this
    /// window is treated as EOF (see the type doc), but only when no
    /// admitted request is still awaiting its response. `None` disables it.
    ///
    /// `response_deadline`: the longest a single response write may stay
    /// pending before it is abandoned (see [`Self::send`]) — independent of
    /// `idle_timeout`, and meaningful whether or not an idle timeout is
    /// configured. `None` disables the bound (a response write waits
    /// unbounded, matching this transport's pre-existing behavior).
    pub(crate) fn with_idle_timeout(
        inner: T,
        root: tokio_util::sync::CancellationToken,
        idle_timeout: Option<std::time::Duration>,
        response_deadline: Option<std::time::Duration>,
    ) -> Self {
        Self {
            inner,
            root,
            idle_timeout,
            response_deadline,
            in_flight: Arc::new(AtomicI64::new(0)),
        }
    }
}

impl<T> rmcp::transport::Transport<rmcp::RoleServer> for CancelOnEofTransport<T>
where
    T: rmcp::transport::Transport<rmcp::RoleServer>,
    T::Error: From<std::io::Error>,
{
    type Error = T::Error;

    /// Bounds a response/error write by `response_deadline` (requests pass
    /// straight through, untimed — only a response can leave `in_flight`
    /// permanently pinned). On timeout the write future is dropped —
    /// releasing whatever lock the inner transport held across it, see the
    /// type doc — `in_flight` is decremented the same as any other resolved
    /// write, and `root` is cancelled directly: the peer has demonstrated it
    /// is not going to read this response, so there is no reason to wait for
    /// the next idle tick to notice.
    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let is_response = matches!(
            item,
            rmcp::model::JsonRpcMessage::Response(_) | rmcp::model::JsonRpcMessage::Error(_)
        );
        let in_flight = self.in_flight.clone();
        let root = self.root.clone();
        let response_deadline = self.response_deadline;
        let send = self.inner.send(item);
        async move {
            let result = match (is_response, response_deadline) {
                (true, Some(deadline)) => match tokio::time::timeout(deadline, send).await {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!(
                            deadline_secs = deadline.as_secs(),
                            "stdio bridge response-delivery deadline elapsed while a response \
                             write was still pending (peer stopped reading); abandoning the \
                             write and closing this session"
                        );
                        root.cancel();
                        Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "response-delivery deadline elapsed before the write completed",
                        )
                        .into())
                    }
                },
                _ => send.await,
            };
            // Decrement once the write is resolved either way — see the
            // `in_flight` field doc for why a failed write must not be left
            // permanently counted as pending.
            if is_response {
                in_flight.fetch_sub(1, Ordering::AcqRel);
            }
            result
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>>
           + Send {
        let root = self.root.clone();
        let idle_timeout = self.idle_timeout;
        let in_flight = self.in_flight.clone();
        async move {
            // Created once and reused across every retry below: `select!`ing
            // a *fresh* `self.inner.receive()` each time the idle sleep wins
            // would drop the in-progress read and restart it next iteration.
            // rmcp's line-buffered receive clears its buffer at the start of
            // each call, so restarting mid-read silently discards whatever
            // prefix of the next request had already arrived before the
            // deferred timeout — the remainder then parses as garbage.
            // Racing the sleep against `&mut` this same pinned future keeps
            // the partial read alive across as many deferrals as it takes.
            let recv_fut = self.inner.receive();
            tokio::pin!(recv_fut);
            loop {
                let message = match idle_timeout {
                    Some(timeout) => {
                        tokio::select! {
                            message = &mut recv_fut => message,
                            () = tokio::time::sleep(timeout) => {
                                if in_flight.load(Ordering::Acquire) > 0 {
                                    tracing::debug!(
                                        idle_timeout_secs = timeout.as_secs(),
                                        "stdio bridge idle timeout elapsed but an admitted \
                                         request is still awaiting its response; deferring \
                                         session close"
                                    );
                                    continue;
                                }
                                tracing::info!(
                                    idle_timeout_secs = timeout.as_secs(),
                                    "stdio bridge idle timeout elapsed with no request; \
                                     closing this session to release its pooled resources"
                                );
                                None
                            }
                        }
                    }
                    None => recv_fut.await,
                };
                if let Some(rmcp::model::JsonRpcMessage::Request(_)) = &message {
                    in_flight.fetch_add(1, Ordering::AcqRel);
                }
                if message.is_none() {
                    root.cancel();
                }
                return message;
            }
        }
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

/// Options passed to a transport at serve time.
#[derive(Debug, Default, Clone)]
pub struct ServeOptions {
    /// Bind address for network transports (e.g. `0.0.0.0:8080`). Ignored by stdio.
    pub bind: Option<String>,
}

/// A way to serve the MCP `request` surface.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Name selected via `--transport <name>`.
    fn name(&self) -> &'static str;

    /// One-line description for help and listing.
    fn about(&self) -> &'static str;

    /// Serve until the connection closes. Consumes the server.
    async fn serve(&self, server: KhiveMcpServer, opts: &ServeOptions) -> anyhow::Result<()>;
}

/// MCP over stdio — the default transport, used by the deno/npm wrapper.
pub struct StdioTransport;

#[async_trait]
impl Transport for StdioTransport {
    fn name(&self) -> &'static str {
        "stdio"
    }

    fn about(&self) -> &'static str {
        "MCP over stdio (default)"
    }

    async fn serve(&self, server: KhiveMcpServer, _opts: &ServeOptions) -> anyhow::Result<()> {
        server.serve_stdio().await
    }
}

/// Named registry of serving transports.
pub struct TransportRegistry {
    transports: BTreeMap<&'static str, Box<dyn Transport>>,
}

impl TransportRegistry {
    /// Empty registry — no transports.
    pub fn new() -> Self {
        Self {
            transports: BTreeMap::new(),
        }
    }

    /// Registry pre-populated with the built-in transports (`stdio`).
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(StdioTransport));
        registry
    }

    /// Add (or replace) a transport. Keyed by [`Transport::name`].
    pub fn register(&mut self, transport: Box<dyn Transport>) {
        self.transports.insert(transport.name(), transport);
    }

    /// Look up a transport by name.
    pub fn get(&self, name: &str) -> Option<&dyn Transport> {
        self.transports.get(name).map(|t| t.as_ref())
    }

    /// All registered transport names, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        self.transports.keys().copied().collect()
    }
}

impl Default for TransportRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}
