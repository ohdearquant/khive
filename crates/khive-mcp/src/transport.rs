//! Registerable MCP serving transports.
//!
//! A [`Transport`] owns the wire protocol used to serve the `request` surface.
//! Built-ins are registered via [`TransportRegistry::with_builtins`]; additional
//! transports (e.g. Streamable HTTP) register with [`TransportRegistry::register`]
//! before serving, so the serve path never hard-codes a transport enum.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;

use crate::server::KhiveMcpServer;

type RequestId = rmcp::model::RequestId;

#[cfg(not(test))]
type OutstandingEntries = HashMap<RequestId, OutstandingRequest>;

#[cfg(test)]
mod counted_outstanding_entries {
    use std::cell::Cell;

    use super::{HashMap, OutstandingRequest, RequestId};

    // Every method that reaches an element counts it; a traversal method,
    // should one be added, must count each element it yields.
    #[derive(Default)]
    pub(super) struct OutstandingEntries {
        inner: HashMap<RequestId, OutstandingRequest>,
        element_touches: Cell<usize>,
    }

    impl OutstandingEntries {
        fn record_element_touch(&self) {
            self.element_touches.set(self.element_touches.get() + 1);
        }

        pub(super) fn contains_key(&self, id: &RequestId) -> bool {
            self.record_element_touch();
            self.inner.contains_key(id)
        }

        pub(super) fn get(&self, id: &RequestId) -> Option<&OutstandingRequest> {
            self.record_element_touch();
            self.inner.get(id)
        }

        pub(super) fn get_mut(&mut self, id: &RequestId) -> Option<&mut OutstandingRequest> {
            self.record_element_touch();
            self.inner.get_mut(id)
        }

        pub(super) fn remove(&mut self, id: &RequestId) -> Option<OutstandingRequest> {
            self.record_element_touch();
            self.inner.remove(id)
        }

        pub(super) fn insert(
            &mut self,
            id: RequestId,
            obligation: OutstandingRequest,
        ) -> Option<OutstandingRequest> {
            self.record_element_touch();
            self.inner.insert(id, obligation)
        }

        pub(super) fn len(&self) -> usize {
            self.inner.len()
        }

        pub(super) fn is_empty(&self) -> bool {
            self.inner.is_empty()
        }

        pub(super) fn take_element_touches(&self) -> usize {
            self.element_touches.replace(0)
        }
    }
}

#[cfg(test)]
use counted_outstanding_entries::OutstandingEntries;

/// Outstanding request obligations in admission order.
///
/// The map makes duplicate detection and retirement O(1) on average. Each
/// entry also links its predecessor and successor, so the linked ordering
/// structure makes stale-obligation expiry deadline-ordered without requiring
/// a linear scan when a response retires a newer request.
#[derive(Default)]
pub(crate) struct OutstandingRequests {
    entries: OutstandingEntries,
    oldest: Option<RequestId>,
    newest: Option<RequestId>,
}

struct OutstandingRequest {
    admitted_at: Instant,
    previous: Option<RequestId>,
    next: Option<RequestId>,
}

impl OutstandingRequests {
    fn admit(&mut self, id: RequestId, admitted_at: Instant, capacity: usize) -> bool {
        if self.contains(&id) || self.entries.len() >= capacity {
            return false;
        }

        let previous = self.newest.clone();
        if let Some(previous_id) = previous.as_ref() {
            self.entries
                .get_mut(previous_id)
                .expect("newest obligation must remain in the map")
                .next = Some(id.clone());
        } else {
            self.oldest = Some(id.clone());
        }
        self.newest = Some(id.clone());
        self.entries.insert(
            id,
            OutstandingRequest {
                admitted_at,
                previous,
                next: None,
            },
        );
        true
    }

    fn contains(&self, id: &RequestId) -> bool {
        self.entries.contains_key(id)
    }

    fn retire(&mut self, id: &RequestId) {
        let Some(obligation) = self.entries.remove(id) else {
            return;
        };

        if let Some(previous_id) = obligation.previous.as_ref() {
            self.entries
                .get_mut(previous_id)
                .expect("previous obligation must remain in the map")
                .next = obligation.next.clone();
        } else {
            self.oldest = obligation.next.clone();
        }

        if let Some(next_id) = obligation.next.as_ref() {
            self.entries
                .get_mut(next_id)
                .expect("next obligation must remain in the map")
                .previous = obligation.previous.clone();
        } else {
            self.newest = obligation.previous;
        }
    }

    fn drop_stale(&mut self, obligation_ttl: Option<std::time::Duration>) {
        let Some(ttl) = obligation_ttl else {
            return;
        };

        while let Some(oldest_id) = self.oldest.clone() {
            let stale = self
                .entries
                .get(&oldest_id)
                .is_some_and(|obligation| obligation.admitted_at.elapsed() >= ttl);
            if !stale {
                break;
            }
            self.retire(&oldest_id);
        }
    }

    fn newest_admitted_at(&self) -> Option<Instant> {
        self.newest
            .as_ref()
            .and_then(|id| self.entries.get(id))
            .map(|obligation| obligation.admitted_at)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn front(&self) -> Option<(RequestId, Instant)> {
        self.oldest.as_ref().and_then(|id| {
            self.entries
                .get(id)
                .map(|obligation| (id.clone(), obligation.admitted_at))
        })
    }
}

struct ObligationRetirementGuard {
    in_flight: Arc<Mutex<OutstandingRequests>>,
    id: Option<RequestId>,
    obligation_ttl: Option<std::time::Duration>,
    root: tokio_util::sync::CancellationToken,
}

impl ObligationRetirementGuard {
    fn retire(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        match self.in_flight.lock() {
            Ok(mut outstanding) => {
                outstanding.retire(&id);
                outstanding.drop_stale(self.obligation_ttl);
            }
            Err(_) => {
                tracing::error!(
                    request_id = ?id,
                    "stdio bridge outstanding-request tracker is poisoned during retirement; \
                     cancelling the session rather than losing the obligation"
                );
                self.root.cancel();
            }
        }
    }
}

impl Drop for ObligationRetirementGuard {
    fn drop(&mut self) {
        self.retire();
    }
}

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
    /// matches keeps the linked list's newest entry genuinely outstanding,
    /// which is what the freshness rule assumes.
    ///
    /// Because retirement is keyed, a request that never produces a response
    /// would hold its entry for the life of the session. Entries past
    /// `obligation_ttl` are therefore dropped whenever this tracker is
    /// touched.
    in_flight: Arc<Mutex<OutstandingRequests>>,
    /// Maximum number of requests admitted to rmcp without a completed
    /// response. Reaching this bound closes the session before another
    /// handler can be spawned.
    max_outstanding_requests: usize,
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
    #[cfg(test)]
    pub(crate) fn with_idle_timeout(
        inner: T,
        root: tokio_util::sync::CancellationToken,
        idle_timeout: Option<std::time::Duration>,
        response_deadline: Option<std::time::Duration>,
        obligation_ttl: Option<std::time::Duration>,
    ) -> Self {
        Self::with_idle_timeout_and_max_outstanding(
            inner,
            root,
            idle_timeout,
            response_deadline,
            obligation_ttl,
            DEFAULT_MAX_OUTSTANDING_REQUESTS,
        )
    }

    pub(crate) fn with_idle_timeout_and_max_outstanding(
        inner: T,
        root: tokio_util::sync::CancellationToken,
        idle_timeout: Option<std::time::Duration>,
        response_deadline: Option<std::time::Duration>,
        obligation_ttl: Option<std::time::Duration>,
        max_outstanding_requests: usize,
    ) -> Self {
        assert!(
            max_outstanding_requests > 0,
            "maximum outstanding requests must be positive"
        );
        Self {
            inner,
            root,
            idle_timeout,
            response_deadline,
            in_flight: Arc::new(Mutex::new(OutstandingRequests::default())),
            max_outstanding_requests,
            obligation_ttl,
        }
    }

    /// The outstanding-obligation tracker, for tests that need to observe it.
    #[cfg(test)]
    pub(crate) fn in_flight_handle(&self) -> Arc<Mutex<OutstandingRequests>> {
        self.in_flight.clone()
    }
}

/// Default per-session cap for requests whose responses have not finished
/// writing. The server can raise or lower it through its environment setting.
pub(crate) const DEFAULT_MAX_OUTSTANDING_REQUESTS: usize = 1024;

#[cfg(test)]
mod outstanding_request_tests {
    use super::*;

    const SMALL_POPULATION: usize = 64;
    const LARGE_POPULATION: usize = 1024;

    #[derive(Clone, Copy)]
    struct TrackerWork {
        contains: usize,
        admit: usize,
        retire: usize,
        sweep: usize,
    }

    fn request_id(value: usize) -> RequestId {
        RequestId::Number(i64::try_from(value).expect("test request id must fit in i64"))
    }

    fn populated_tracker(population: usize) -> OutstandingRequests {
        let mut outstanding = OutstandingRequests::default();
        let admitted_at = Instant::now();
        for value in 0..population {
            assert!(outstanding.admit(request_id(value), admitted_at, population));
        }
        outstanding.entries.take_element_touches();
        outstanding
    }

    fn measure_tracker_work(population: usize) -> TrackerWork {
        let mut outstanding = populated_tracker(population);

        assert!(!outstanding.contains(&request_id(population + 1)));
        let contains = outstanding.entries.take_element_touches();

        assert!(outstanding.admit(request_id(population), Instant::now(), population + 1));
        let admit = outstanding.entries.take_element_touches();

        outstanding.retire(&request_id(population / 2));
        let retire = outstanding.entries.take_element_touches();

        outstanding.drop_stale(Some(std::time::Duration::from_secs(60)));
        let sweep = outstanding.entries.take_element_touches();

        TrackerWork {
            contains,
            admit,
            retire,
            sweep,
        }
    }

    // Exact counts are the oracle: a counter that stops incrementing reads 0
    // and a scan reads the population, so neither can pass as keyed work.
    fn assert_keyed_work(
        operation: &str,
        expected_touches: usize,
        small_touches: usize,
        large_touches: usize,
    ) {
        assert!(
            small_touches == expected_touches && large_touches == expected_touches,
            "{operation} must touch exactly {expected_touches} element(s) at any population: \
             {SMALL_POPULATION} entries touched {small_touches}, {LARGE_POPULATION} entries \
             touched {large_touches}"
        );
    }

    #[test]
    fn outstanding_request_tracker_work_is_population_independent() {
        let small = measure_tracker_work(SMALL_POPULATION);
        let large = measure_tracker_work(LARGE_POPULATION);

        // contains: one lookup. admit: the duplicate check, the newest link
        // update, the insert. retire (middle): the removal plus both neighbour
        // links. sweep with nothing stale: one look at the oldest entry.
        for (operation, expected_touches, small_touches, large_touches) in [
            ("contains", 1, small.contains, large.contains),
            ("admit", 3, small.admit, large.admit),
            ("retire", 3, small.retire, large.retire),
            ("non-stale sweep", 1, small.sweep, large.sweep),
        ] {
            assert_keyed_work(operation, expected_touches, small_touches, large_touches);
        }
    }

    #[test]
    fn outstanding_request_tracker_rejects_admission_past_capacity() {
        let mut outstanding = OutstandingRequests::default();
        let admitted_at = Instant::now();

        assert!(outstanding.admit(rmcp::model::RequestId::Number(1), admitted_at, 2,));
        assert!(outstanding.admit(rmcp::model::RequestId::Number(2), admitted_at, 2,));
        assert!(!outstanding.admit(rmcp::model::RequestId::Number(3), admitted_at, 2,));
        assert_eq!(outstanding.len(), 2);
    }

    fn poison_tracker(tracker: &Arc<Mutex<OutstandingRequests>>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = tracker
                .lock()
                .expect("tracker mutex must initially be healthy");
            panic!("deliberately poison the tracker mutex");
        }));
        assert!(
            result.is_err(),
            "the poisoning fixture must panic while holding the mutex"
        );
    }

    #[tokio::test]
    async fn poisoned_tracker_fails_closed_on_admission_and_retirement() {
        use rmcp::transport::async_rw::AsyncRwTransport;
        use rmcp::transport::Transport as _;
        use tokio::io::AsyncWriteExt;

        let root = tokio_util::sync::CancellationToken::new();
        let (server_io, mut client_io) = tokio::io::duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let mut transport = CancelOnEofTransport::with_idle_timeout(
            AsyncRwTransport::new_server(server_read, server_write),
            root.clone(),
            None,
            None,
            None,
        );
        let tracker = transport.in_flight_handle();
        poison_tracker(&tracker);

        client_io
            .write_all(
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"probe","arguments":{}}}
"#,
            )
            .await
            .expect("write the request used to exercise poisoned admission");

        assert!(
            transport.receive().await.is_none(),
            "poisoned admission must return transport EOF instead of handing the request to rmcp"
        );
        assert!(
            root.is_cancelled(),
            "poisoned admission must cancel the rmcp root token"
        );

        let retirement_root = tokio_util::sync::CancellationToken::new();
        let mut retirement = ObligationRetirementGuard {
            in_flight: tracker,
            id: Some(rmcp::model::RequestId::Number(1)),
            obligation_ttl: None,
            root: retirement_root.clone(),
        };
        retirement.retire();
        assert!(
            retirement_root.is_cancelled(),
            "poisoned retirement must cancel the session instead of consuming the id silently"
        );
    }
}

/// Whether a failed outbound write names an operation that may simply be
/// repeated, leaving the writer usable.
///
/// `ErrorKind::Interrupted` is the one such class the stdio path can reach, and
/// it reaches it through flush rather than through write. Tokio's blocking
/// stdout adapter restores `State::Idle` and puts its writer back before it
/// returns the flush result (`tokio-1.52.4/src/io/blocking.rs:146-176`), and
/// the `uninterruptibly!` macro that repeats interrupted operations
/// (`:183-192`) is not applied to that branch. `FramedWrite` propagates the
/// error up unchanged (`tokio-util-0.7.18/src/codec/framed_impl.rs:277-303`).
/// So an EINTR during flush arrives here as `Err` from a writer that is still
/// able to carry the next message, and cancelling on it would lose a healthy
/// session to a signal.
///
/// The message that hit it is still lost, and what happens next depends on
/// which class it belonged to. For a server-initiated request rmcp hands the
/// error to the local responder that was awaiting it
/// (`rmcp-1.8.0/src/service.rs:1066-1073`), and it does the same for a
/// notification (`:1074-1093`), so in both cases a local caller learns. For a
/// response it only logs (`:1095-1112`): nothing is sent to the peer, no local
/// caller is waiting, and the serve loop stays alive, so the client that asked
/// the question waits for an answer that will never arrive and cannot tell that
/// from a slow one.
///
/// That asymmetry is why this predicate is not the whole condition. Keeping the
/// session is the better trade only when losing the message leaves someone able
/// to observe the loss. It never does for a response, so the caller pairs this
/// with the message class rather than using it alone.
pub(crate) trait RepeatableWriteError {
    fn is_repeatable(&self) -> bool;
}

impl RepeatableWriteError for std::io::Error {
    fn is_repeatable(&self) -> bool {
        self.kind() == std::io::ErrorKind::Interrupted
    }
}

impl<T> rmcp::transport::Transport<rmcp::RoleServer> for CancelOnEofTransport<T>
where
    T: rmcp::transport::Transport<rmcp::RoleServer>,
    T::Error: From<std::io::Error> + RepeatableWriteError,
{
    type Error = T::Error;

    /// Bounds a response/error write by `response_deadline` (requests pass
    /// straight through, untimed — only a response can leave `in_flight`
    /// permanently pinned). On timeout the write future is dropped —
    /// releasing whatever lock the inner transport held across it, see the
    /// type doc — and `in_flight` is decremented the same as any other
    /// resolved write.
    ///
    /// A response write that RESOLVES AS AN ERROR cancels `root` for the same
    /// reason a timed-out one does, and the two together are what actually
    /// bound the write side. The deadline only covers a write left *pending*:
    /// a peer that has stopped reading while its pipe stays open. A peer that
    /// closes the side it reads from fails the write immediately instead, well
    /// inside any deadline, so nothing here would fire — and rmcp does not
    /// close the session on its own behalf: the response-send task logs the
    /// error and returns (`rmcp-1.8.0`, `src/service.rs:1105-1112`), while the
    /// inner transport reports writer errors independently of the reader
    /// (`src/transport/async_rw.rs:107-122`). With the read side still open
    /// and silent, the receive loop stays pending, and with idle reaping
    /// disabled by default there is no later tick to notice. So the session
    /// would outlive the peer's ability to receive anything from it.
    ///
    /// **Every** failed outbound write cancels, not only a response. An
    /// earlier version of this scoped the cancel to responses, on the ground
    /// that a response discharges an admitted obligation while server-initiated
    /// requests and notifications "carry their own send accounting inside rmcp
    /// (`SendTaskResult::Request`)". That accounting exists and does not do
    /// this job: on a failed request send, rmcp removes the caller's responder
    /// and hands it `ServiceError::TransportSend`
    /// (`rmcp-1.8.0`, `src/service.rs:1066-1073`); on a failed notification
    /// send it does the same to the notification's responder (`:1074-1093`).
    /// Neither branch breaks the serve loop, whose only exits are receive EOF,
    /// token cancellation, and a send-task join error (`:1028-1062`). So a
    /// failed request or notification write strands the session exactly as a
    /// failed response did — the argument for the narrower rule pointed at a
    /// mechanism without reading what it does, and is withdrawn.
    ///
    /// One class of failure is excepted, and it is excepted on read evidence
    /// rather than on the same kind of argument: an error saying the operation
    /// may simply be repeated leaves the writer usable, so cancelling on it
    /// would lose a healthy session. See `RepeatableWriteError` for which class
    /// that is and how it reaches this transport.
    ///
    /// A failed write is reported once, here, whichever way it failed: the
    /// deadline arm's error names its own cause, so a second log line at the
    /// timeout would only repeat it.
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
        let retirement = retire_id.map(|id| ObligationRetirementGuard {
            in_flight,
            id: Some(id),
            obligation_ttl,
            root: root.clone(),
        });
        let send = self.inner.send(item);
        async move {
            // The guard retires the matching id both after a resolved write and
            // if rmcp cancels/drops this send future before it resolves.
            let _retirement = retirement;
            let result = match (is_response, response_deadline) {
                (true, Some(deadline)) => match tokio::time::timeout(deadline, send).await {
                    Ok(result) => result,
                    Err(_) => Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "response-delivery deadline elapsed before the write completed",
                    )
                    .into()),
                },
                _ => send.await,
            };
            // Either failure — the write outlived its deadline, or it resolved
            // as an error — means this peer is not receiving what the session
            // just tried to send it, and rmcp will not close on its own behalf
            // for any of the three message classes. Close here rather than fail
            // one write and carry on; see the method doc for why this covers
            // requests and notifications too, and why the error case needs
            // saying separately from the timeout case.
            //
            // The exception is a write error that says the operation may simply
            // be repeated, on a message that is not a response. There the writer
            // is still usable and the loss is observable by someone, so
            // cancelling would trade a lost message for a lost session. A
            // response gets no such exception: rmcp only logs a failed response
            // send, so the peer is left waiting on an answer that is not coming
            // and closing is the only way it finds out. See
            // `RepeatableWriteError`.
            if let Err(error) = &result {
                if error.is_repeatable() && !is_response {
                    tracing::warn!(
                        %error,
                        is_response,
                        "stdio bridge write was interrupted; the writer is still usable and this \
                         is not a response, so the session stays open and the error is reported \
                         unchanged"
                    );
                } else {
                    tracing::warn!(
                        %error,
                        is_response,
                        response_deadline_secs = response_deadline.map(|d| d.as_secs()),
                        "stdio bridge could not deliver an outbound message to its peer; closing \
                         this session rather than leaving it alive unable to answer"
                    );
                    root.cancel();
                }
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
        let max_outstanding_requests = self.max_outstanding_requests;
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
                            // The read branch MUST win a tie, so it is first and
                            // the arms are polled in order. Without `biased`,
                            // `select!` picks a starting branch at random for
                            // fairness (tokio 1.52.4, `src/macros/select.rs`
                            // lines 61-68), which here means that a request
                            // already sitting in the buffer loses a coin flip to
                            // an idle timer that has just elapsed. The session is
                            // then closed on a peer that did answer inside the
                            // window, and it happens only under the scheduling
                            // delay that makes both branches ready at once, so it
                            // is load-dependent rather than reproducible.
                            //
                            // Starving the timer is the usual cost of `biased`
                            // and is not a cost here: a read that is continuously
                            // ready means the session is not idle, which is
                            // exactly when the timer must not fire. The sleep is
                            // also rebuilt on every iteration, so deferring never
                            // accumulates elapsed time against the next window.
                            biased;
                            message = &mut recv_fut => message,
                            () = tokio::time::sleep(timeout) => {
                                // Defer only on a FRESH obligation. The newest
                                // entry is the test: if even that one predates a
                                // full idle window, everything outstanding is
                                // stale and none of it is evidence of life. See
                                // the type doc for why a bare count cannot be
                                // trusted here.
                                let (fresh, has_outstanding) = match in_flight.lock() {
                                    Ok(q) => {
                                        let fresh = q
                                            .newest_admitted_at()
                                            .is_some_and(|newest| match obligation_ttl {
                                                Some(ttl) => newest.elapsed() < ttl,
                                                None => true,
                                            });
                                        (fresh, !q.is_empty())
                                    }
                                    Err(_) => {
                                        tracing::error!(
                                            "stdio bridge outstanding-request tracker is \
                                             poisoned during idle handling; cancelling the \
                                             session"
                                        );
                                        root.cancel();
                                        return None;
                                    }
                                };
                                if fresh {
                                    tracing::debug!(
                                        idle_timeout_secs = timeout.as_secs(),
                                        "stdio bridge idle timeout elapsed but an admitted \
                                         request is still awaiting its response; deferring \
                                         session close"
                                    );
                                    continue;
                                }
                                if has_outstanding {
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
                //
                // The duplicate scan runs BEFORE the staleness drop, and that
                // order is load-bearing. A stale entry is an id whose response
                // was never observed — the handler may still be running, since
                // rmcp keeps it alive independently of this receive loop until
                // it constructs its response. Dropping it first would convert
                // exactly the ambiguous case into a silent re-admission: the
                // id passes the check, is pushed as a fresh entry, and the
                // first of the two eventual responses retires the NEW entry by
                // id match, leaving the older live handler untracked and the
                // idle branch free to close out from under it. Scanning the
                // full queue first refuses that reuse instead. It costs
                // nothing a conforming peer can notice, because an id whose
                // response WAS written is not in the queue at all.
                let mut duplicate_id = None;
                let mut capacity_exceeded = false;
                if let Some(rmcp::model::JsonRpcMessage::Request(request)) = &message {
                    match in_flight.lock() {
                        Ok(mut q) => {
                            if q.contains(&request.id) {
                                duplicate_id = Some(request.id.clone());
                            } else {
                                q.drop_stale(obligation_ttl);
                                capacity_exceeded = !q.admit(
                                    request.id.clone(),
                                    Instant::now(),
                                    max_outstanding_requests,
                                );
                            }
                        }
                        Err(_) => {
                            tracing::error!(
                                request_id = ?request.id,
                                "stdio bridge outstanding-request tracker is poisoned during \
                                 admission; cancelling the session rather than admitting \
                                 untracked work"
                            );
                            root.cancel();
                            return None;
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
                if capacity_exceeded {
                    tracing::warn!(
                        max_outstanding_requests,
                        "stdio bridge reached its outstanding request limit; closing the session"
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
