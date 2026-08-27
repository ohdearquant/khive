//! Registerable MCP serving transports.
//!
//! A [`Transport`] owns the wire protocol used to serve the `request` surface.
//! Built-ins are registered via [`TransportRegistry::with_builtins`]; additional
//! transports (e.g. Streamable HTTP) register with [`TransportRegistry::register`]
//! before serving, so the serve path never hard-codes a transport enum.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;

use crate::server::KhiveMcpServer;

/// Cancels rmcp's root service token before reporting transport EOF — and,
/// with an idle timeout configured, before reporting a synthetic EOF when no
/// message has arrived within that window even though the pipe itself stays
/// open.
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
/// **An obligation is evidence of life only while it is fresh.** rmcp spawns
/// each request handler with `spawn_service_task` and drops the join handle
/// (`rmcp::service`), so a handler that panics never reaches the response
/// construction that would let this transport decrement. A bare counter would
/// then stay positive for the life of the process and the idle branch would
/// defer forever — the precise unbounded lifetime this type exists to close,
/// reintroduced through its own guard. `in_flight` therefore records the
/// *instant* each request was admitted, and the idle branch defers only while
/// the newest outstanding obligation is younger than `obligation_ttl`. An
/// obligation older than that is not distinguishable, by any signal available
/// at the transport, from a handler that died without answering.
///
/// `obligation_ttl` is deliberately NOT the idle window. Tying the two
/// together would make a long-running handler stop protecting its own session
/// after a single quiet window, which is the guarantee this counter exists to
/// provide. It is its own bound, set well above any real handler, and it
/// answers a different question: not "has this session been quiet" but "has
/// this request been outstanding so long that the handler must be gone".
/// `None` restores the unbounded defer, and with it the leak — it exists for
/// callers that do not enable idle reaping at all, where the defer decision is
/// never reached.
///
/// A response write that never resolves — a peer that admits a request,
/// then stops reading its response while keeping the pipe open — would
/// otherwise pin this session (and its reader-pool admission / DB
/// connection) forever: the obligation would never clear, so the idle
/// check above would defer for as long as the freshness rule allows. `response_deadline` bounds the
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
    /// Requests handed to rmcp whose response has not finished writing, each
    /// paired with the instant it was admitted. Ordered oldest-first; see the
    /// type doc for why the instants matter and not just the count.
    ///
    /// Entries are keyed by request id because they are **not**
    /// interchangeable. rmcp spawns each handler and each response send
    /// independently, so a newer request's response can finish first. Retiring
    /// the oldest entry on any completion would then drop a still-outstanding
    /// request's admission and leave a *completed* one as the newest entry —
    /// making the freshness test below read the completed request's timestamp
    /// and defer past the older obligation's TTL. Removing the entry whose id
    /// matches keeps `back()` the genuinely newest outstanding admission,
    /// which is what the freshness rule assumes.
    ///
    /// Because retirement is keyed, a request that never produces a response
    /// would hold its entry for the life of the session. Entries past
    /// `obligation_ttl` are therefore dropped whenever this queue is touched;
    /// see [`drop_stale_obligations`].
    in_flight: Arc<Mutex<VecDeque<(rmcp::model::RequestId, Instant)>>>,
    /// How long an outstanding request keeps deferring the idle close. `None`
    /// defers without bound. See the type doc.
    obligation_ttl: Option<std::time::Duration>,
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
    ///
    /// `obligation_ttl`: how long an admitted request whose response has not
    /// been written keeps deferring the idle close. `None` defers without
    /// bound, which is only safe where `idle_timeout` is also `None`, because
    /// a handler that panics never produces the response that would clear the
    /// obligation (see the type doc).
    pub(crate) fn with_idle_timeout(
        inner: T,
        root: tokio_util::sync::CancellationToken,
        idle_timeout: Option<std::time::Duration>,
        response_deadline: Option<std::time::Duration>,
        obligation_ttl: Option<std::time::Duration>,
    ) -> Self {
        Self {
            inner,
            root,
            idle_timeout,
            response_deadline,
            in_flight: Arc::new(Mutex::new(VecDeque::new())),
            obligation_ttl,
        }
    }

    /// The obligation queue, for tests that need to observe its size.
    #[cfg(test)]
    pub(crate) fn in_flight_handle(
        &self,
    ) -> Arc<Mutex<VecDeque<(rmcp::model::RequestId, Instant)>>> {
        self.in_flight.clone()
    }
}

/// Drops obligations that are already past their TTL.
///
/// An entry older than `obligation_ttl` is, by this transport's own rule, no
/// longer evidence of life: the freshness check will not defer on it. Keeping
/// it buys nothing, and keeping it *forever* is a real cost, because
/// retirement is keyed by request id and so only ever removes the entry whose
/// response was actually written. A request that never produces one — a
/// handler that panics, a request the peer cancels — would otherwise hold its
/// entry for the life of the session, and a long-lived session accumulates one
/// per such request without bound.
///
/// Entries are pushed in admission order, so the stale ones are a prefix.
///
/// With no TTL configured there is no notion of staleness and nothing is
/// dropped. That is the configuration which already restores the unbounded
/// defer (see the type doc), so it is unbounded in the same way for the same
/// reason.
fn drop_stale_obligations(
    queue: &mut VecDeque<(rmcp::model::RequestId, Instant)>,
    obligation_ttl: Option<std::time::Duration>,
) {
    let Some(ttl) = obligation_ttl else {
        return;
    };
    while queue.front().is_some_and(|(_, at)| at.elapsed() >= ttl) {
        queue.pop_front();
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
        // Which obligation this write discharges. A `Response` always names
        // its request. A `JsonRpcError` carries `Option<RequestId>`: the MCP
        // spec omits the id when the server could not read it (parse error,
        // invalid request), and such an error answers no admitted request, so
        // it must retire nothing rather than retire an arbitrary one.
        let retire_id: Option<rmcp::model::RequestId> = match &item {
            rmcp::model::JsonRpcMessage::Response(response) => Some(response.id.clone()),
            rmcp::model::JsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        let in_flight = self.in_flight.clone();
        let root = self.root.clone();
        let response_deadline = self.response_deadline;
        let obligation_ttl = self.obligation_ttl;
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
            // Clear THIS request's obligation once the write is resolved
            // either way — see the `in_flight` field doc for why a failed
            // write must not be left permanently counted as pending, and why
            // the entry removed has to be the matching one rather than the
            // oldest. An id-less error matches nothing and removes nothing.
            if let Ok(mut q) = in_flight.lock() {
                if let Some(id) = retire_id {
                    if let Some(position) = q.iter().position(|(entry, _)| *entry == id) {
                        q.remove(position);
                    }
                }
                drop_stale_obligations(&mut q, obligation_ttl);
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
        let obligation_ttl = self.obligation_ttl;
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
                                // Defer only on a FRESH obligation. The newest
                                // entry is the test: if even that one predates a
                                // full idle window, everything outstanding is
                                // stale and none of it is evidence of life. See
                                // the type doc for why a bare count cannot be
                                // trusted here.
                                let fresh = in_flight
                                    .lock()
                                    .ok()
                                    .and_then(|q| q.back().map(|(_, at)| *at))
                                    .is_some_and(|newest| match obligation_ttl {
                                        Some(ttl) => newest.elapsed() < ttl,
                                        None => true,
                                    });
                                if fresh {
                                    tracing::debug!(
                                        idle_timeout_secs = timeout.as_secs(),
                                        "stdio bridge idle timeout elapsed but an admitted \
                                         request is still awaiting its response; deferring \
                                         session close"
                                    );
                                    continue;
                                }
                                if in_flight.lock().is_ok_and(|q| !q.is_empty()) {
                                    tracing::warn!(
                                        idle_timeout_secs = timeout.as_secs(),
                                        obligation_ttl_secs =
                                            obligation_ttl.map(|d| d.as_secs()),
                                        "stdio bridge closing with admitted requests whose \
                                         responses never arrived and whose admission is older \
                                         than the request-obligation TTL; treating them as \
                                         dead rather than deferring indefinitely"
                                    );
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
                // A second outstanding obligation under an id that already has
                // one is refused rather than admitted. Two entries sharing an
                // id make retirement ambiguous, and BOTH ways of resolving it
                // are wrong: retiring the first match can leave a completed
                // request's instant as the newest entry and defer past the
                // older obligation's TTL (the leak), while retiring the last
                // match can leave the older instant and close the session out
                // from under a live handler (the opposite failure). There is
                // no third choice, because the transport cannot tell the two
                // apart — the ambiguity has to be refused where it enters.
                //
                // MCP requires a request id to be unused within a session
                // (2025-11-25, "Requests"), so a conforming peer never reaches
                // this. A non-conforming one must not be able to corrupt the
                // session's lifetime accounting from the wire, which is what
                // makes this the transport's problem rather than the peer's.
                let mut duplicate_id = None;
                if let Some(rmcp::model::JsonRpcMessage::Request(request)) = &message {
                    if let Ok(mut q) = in_flight.lock() {
                        drop_stale_obligations(&mut q, obligation_ttl);
                        if q.iter().any(|(entry, _)| *entry == request.id) {
                            duplicate_id = Some(request.id.clone());
                        } else {
                            q.push_back((request.id.clone(), Instant::now()));
                        }
                    }
                }
                if let Some(id) = duplicate_id {
                    tracing::warn!(
                        duplicate_request_id = ?id,
                        "stdio bridge received a request whose id already has an outstanding \
                         obligation; the response could not be attributed to either, so this \
                         session is closed rather than left with corrupt lifetime accounting"
                    );
                    root.cancel();
                    return None;
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
