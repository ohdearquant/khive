use super::events_count_read_budget::CountReadBudget;
use super::paths::{
    DEFAULT_APPEND_QUEUE_BATCHES, DEFAULT_APPEND_QUEUE_BYTES, EVENTS_PROTOCOL_VERSION,
    FORWARDER_BACKOFF, REQUEST_TIMEOUT,
};
use super::protocol::{EventsRequest, EventsResponse};
use super::socket_path::validate_events_socket_path;
use crate::daemon::{read_frame, write_frame};
use khive_db::StorageBackend;
use khive_storage::{Event, EventStore, StorageError, StorageResult};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;

/// Connect to the events socket and verify who answered. Pathname
/// reachability is not identity: any process that can write the directory can
/// pre-bind the path. The kernel-reported peer uid is the one identity the
/// far side cannot choose, so every client connect — round-trips, the
/// forwarder, the supervisor probe — refuses a socket answered by a foreign
/// uid before a single frame is written.
#[cfg(unix)]
pub(super) async fn connect_verified(socket_path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(socket_path).await?;
    // SAFETY: `geteuid` is always successful and takes no arguments.
    let own_euid = unsafe { libc::geteuid() } as u32;
    let peer = crate::daemon::peer_uid(&stream)?;
    if !crate::daemon::uid_is_permitted(peer, own_euid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "events socket {} answered by uid {peer}, not this process's uid {own_euid}; \
                 refusing to exchange event frames with an unowned daemon",
                socket_path.display()
            ),
        ));
    }
    Ok(stream)
}

/// Counters describing the fire-and-forget lane's degradation. Zero drops is
/// the healthy state; any non-zero `dropped_batches` means the loss-tolerant
/// contract was exercised and says so.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EventsForwardingMetrics {
    pub forwarded_batches: u64,
    pub forwarded_events: u64,
    pub dropped_batches: u64,
    pub dropped_events: u64,
    /// Serialized request bytes reserved by queued and in-flight batches.
    pub queued_bytes: usize,
}

#[cfg(unix)]
#[derive(Debug, Default)]
pub(super) struct ForwardingCounters {
    pub(super) forwarded_batches: AtomicU64,
    pub(super) forwarded_events: AtomicU64,
    pub(super) dropped_batches: AtomicU64,
    pub(super) dropped_events: AtomicU64,
    pub(super) queued_bytes: AtomicUsize,
}

/// A reservation lives with the batch until delivery or a counted drop.
/// Dropping a forwarder mid-delivery also returns its byte budget.
#[cfg(unix)]
#[derive(Debug)]
pub(super) struct AppendByteReservation {
    pub(super) counters: Arc<ForwardingCounters>,
    pub(super) bytes: usize,
}

#[cfg(unix)]
impl Drop for AppendByteReservation {
    fn drop(&mut self) {
        self.counters
            .queued_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(unix)]
#[derive(Debug)]
pub(super) struct QueuedAppend {
    pub(super) request: EventsRequest,
    pub(super) count: u64,
    pub(super) _reservation: AppendByteReservation,
}

#[cfg(all(test, unix))]
impl QueuedAppend {
    pub(super) fn unmetered(
        namespace: &str,
        events: Vec<Event>,
        counters: Arc<ForwardingCounters>,
    ) -> Self {
        let count = events.len() as u64;
        Self {
            request: EventsRequest::AppendEvents {
                protocol_version: EVENTS_PROTOCOL_VERSION,
                namespace: namespace.to_string(),
                events,
            },
            count,
            _reservation: AppendByteReservation { counters, bytes: 0 },
        }
    }
}

/// Measure the actual wire request without retaining a second, potentially
/// huge serialized copy of the caller's batch. Serde stops at the frame cap.
#[cfg(unix)]
#[derive(Default)]
pub(super) struct BoundedFrameCounter {
    pub(super) bytes: usize,
    pub(super) exceeded: bool,
}

#[cfg(unix)]
impl std::io::Write for BoundedFrameCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.saturating_add(buf.len());
        if next > crate::daemon::MAX_FRAME_BYTES {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "events append request exceeds IPC frame cap",
            ));
        }
        self.bytes = next;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn reserve_append_bytes(
    counters: &Arc<ForwardingCounters>,
    bytes: usize,
    budget: usize,
) -> Option<AppendByteReservation> {
    let mut used = counters.queued_bytes.load(Ordering::Acquire);
    loop {
        let next = used.checked_add(bytes)?;
        if next > budget {
            return None;
        }
        match counters.queued_bytes.compare_exchange_weak(
            used,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return Some(AppendByteReservation {
                    counters: Arc::clone(counters),
                    bytes,
                });
            }
            Err(observed) => used = observed,
        }
    }
}

/// One per domain process: the connection to the events daemon plus the
/// bounded fire-and-forget append queue.
///
/// Synchronous round-trips each own a fresh connection: there is no shared
/// request connection and no lock in front of one, so concurrent reads and
/// audit flushes never queue behind a single stalled call, and the round-trip
/// timeout bounds each caller's whole attempt.
#[cfg(unix)]
pub struct EventsSplitClient {
    pub(super) socket_path: PathBuf,
    pub(super) append_tx: tokio::sync::mpsc::Sender<QueuedAppend>,
    pub(super) append_queue_byte_budget: usize,
    pub(super) counters: Arc<ForwardingCounters>,
    /// Flipped by the forwarder while the daemon is unreachable so the drop
    /// log fires once per outage, not once per batch.
    pub(super) outage_logged: Arc<AtomicBool>,
    /// In-memory validator backing `preflight_event` without I/O.
    pub(super) preflight_store: Arc<dyn EventStore>,
}

#[cfg(unix)]
impl std::fmt::Debug for EventsSplitClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventsSplitClient")
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl EventsSplitClient {
    /// Build the client and spawn its background forwarder.
    pub fn new(socket_path: PathBuf) -> crate::error::RuntimeResult<Arc<Self>> {
        Self::new_with_queue_depth(socket_path, DEFAULT_APPEND_QUEUE_BATCHES)
    }

    /// [`Self::new`] with an explicit fire-and-forget queue bound. Tests use a
    /// tiny depth to exercise the overflow drop arm deterministically.
    pub fn new_with_queue_depth(
        socket_path: PathBuf,
        queue_depth: usize,
    ) -> crate::error::RuntimeResult<Arc<Self>> {
        Self::new_with_queue_depth_and_delivery_timeout(socket_path, queue_depth, REQUEST_TIMEOUT)
    }

    /// [`Self::new_with_queue_depth`] with an explicit per-batch delivery
    /// timeout, so tests can prove the forwarder abandons a hung peer without
    /// waiting out the production clock.
    pub(super) fn new_with_queue_depth_and_delivery_timeout(
        socket_path: PathBuf,
        queue_depth: usize,
        delivery_timeout: Duration,
    ) -> crate::error::RuntimeResult<Arc<Self>> {
        Self::new_with_limits_and_delivery_timeout(
            socket_path,
            queue_depth,
            DEFAULT_APPEND_QUEUE_BYTES,
            delivery_timeout,
        )
    }

    pub(super) fn new_with_limits_and_delivery_timeout(
        socket_path: PathBuf,
        queue_depth: usize,
        byte_budget: usize,
        delivery_timeout: Duration,
    ) -> crate::error::RuntimeResult<Arc<Self>> {
        validate_events_socket_path(&socket_path)?;
        let preflight_backend = StorageBackend::memory()?;
        let preflight_store = preflight_backend.events()?;
        // The in-memory backend must outlive the store handle; the store holds
        // the pool Arc internally, so dropping the backend wrapper here is fine.

        let (append_tx, append_rx) = tokio::sync::mpsc::channel::<QueuedAppend>(queue_depth.max(1));
        let counters = Arc::new(ForwardingCounters::default());
        let outage_logged = Arc::new(AtomicBool::new(false));

        let client = Arc::new(Self {
            socket_path: socket_path.clone(),
            append_tx,
            append_queue_byte_budget: byte_budget,
            counters: Arc::clone(&counters),
            outage_logged: Arc::clone(&outage_logged),
            preflight_store,
        });

        crate::daemon::spawn_named_tracked_task(
            "events_forwarder",
            run_forwarder(
                socket_path,
                append_rx,
                counters,
                outage_logged,
                delivery_timeout,
                crate::daemon::daemon_shutdown_token(),
            ),
        );
        Ok(client)
    }

    /// Snapshot of the fire-and-forget lane's health.
    pub fn metrics(&self) -> EventsForwardingMetrics {
        EventsForwardingMetrics {
            forwarded_batches: self.counters.forwarded_batches.load(Ordering::Relaxed),
            forwarded_events: self.counters.forwarded_events.load(Ordering::Relaxed),
            dropped_batches: self.counters.dropped_batches.load(Ordering::Relaxed),
            dropped_events: self.counters.dropped_events.load(Ordering::Relaxed),
            queued_bytes: self.counters.queued_bytes.load(Ordering::Acquire),
        }
    }

    /// Enqueue a batch on the fire-and-forget lane. Never blocks; a full
    /// queue is a counted, logged drop.
    pub(super) fn enqueue(&self, namespace: &str, events: Vec<Event>) {
        let count = events.len() as u64;
        let request = EventsRequest::AppendEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: namespace.to_string(),
            events,
        };
        let mut frame_size = BoundedFrameCounter::default();
        if let Err(error) = serde_json::to_writer(&mut frame_size, &request) {
            self.count_append_drop(
                count,
                if frame_size.exceeded {
                    "frame cap"
                } else {
                    "serialization"
                },
            );
            tracing::warn!(error = %error, "events append batch could not fit a wire frame");
            return;
        }
        let Some(reservation) = reserve_append_bytes(
            &self.counters,
            frame_size.bytes,
            self.append_queue_byte_budget,
        ) else {
            self.count_append_drop(count, "queue byte budget");
            return;
        };
        let batch = QueuedAppend {
            request,
            count,
            _reservation: reservation,
        };
        match self.append_tx.try_send(batch) {
            Ok(()) => {}
            Err(_) => {
                // The rejected batch drops here and releases its reservation.
                self.count_append_drop(count, "queue batch limit");
            }
        }
    }

    pub(super) fn count_append_drop(&self, count: u64, reason: &'static str) {
        self.counters
            .dropped_batches
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .dropped_events
            .fetch_add(count, Ordering::Relaxed);
        if !self.outage_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                dropped_events = count,
                reason,
                "events append queue rejected loss-tolerant batch"
            );
        }
    }

    /// One synchronous framed round-trip with a bounded timeout, on its own
    /// fresh connection. No connection is shared between concurrent callers,
    /// so no caller ever waits on another's stall, and `REQUEST_TIMEOUT`
    /// bounds the whole attempt — connect, peer verification, write, read.
    pub(super) async fn round_trip(
        &self,
        request: &EventsRequest,
    ) -> StorageResult<EventsResponse> {
        let op = "events daemon round-trip";
        let payload = serialize_events_request(request)?;
        let attempt = events_round_trip_attempt(&self.socket_path, &payload);
        let bytes = match tokio::time::timeout(REQUEST_TIMEOUT, attempt).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(("read", error))) => {
                return Err(StorageError::Serialization {
                    capability: khive_storage::StorageCapability::Events,
                    message: format!(
                        "events daemon closed or broke the response after connection at {}: {error}",
                        self.socket_path.display()
                    ),
                });
            }
            Ok(Err((stage, error))) => {
                return Err(StorageError::Pool {
                    operation: op.into(),
                    message: format!(
                        "events daemon {stage} failed at {}: {error}",
                        self.socket_path.display()
                    ),
                });
            }
            Err(_elapsed) => {
                return Err(StorageError::Timeout {
                    operation: op.into(),
                });
            }
        };

        deserialize_events_response(&bytes)
    }

    pub(super) async fn round_trip_count(
        &self,
        request: &EventsRequest,
        budget: &CountReadBudget,
    ) -> StorageResult<EventsResponse> {
        budget.scope(async {
            let payload = serialize_events_request(request)?;
            let attempt = events_round_trip_attempt(&self.socket_path, &payload);
            let result = khive_storage::await_request_read_phase("events daemon count round-trip", attempt)
                .await.map_err(|error| budget.attribute(error))?;
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(("read", error)) => return Err(StorageError::Serialization {
                    capability: khive_storage::StorageCapability::Events,
                    message: format!("events daemon closed or broke the response after connection at {}: {error}", self.socket_path.display()),
                }),
                Err((stage, error)) => return Err(StorageError::Pool {
                    operation: "events daemon count round-trip".into(),
                    message: format!("events daemon {stage} failed at {}: {error}", self.socket_path.display()),
                }),
            };
            deserialize_events_response(&bytes)
        }).await
    }
}

#[cfg(unix)]
pub(super) fn serialize_events_request(request: &EventsRequest) -> StorageResult<Vec<u8>> {
    serde_json::to_vec(request).map_err(|error| StorageError::Serialization {
        capability: khive_storage::StorageCapability::Events,
        message: format!("events request serialization failed: {error}"),
    })
}

#[cfg(unix)]
pub(super) fn deserialize_events_response(bytes: &[u8]) -> StorageResult<EventsResponse> {
    serde_json::from_slice(bytes).map_err(|error| StorageError::Serialization {
        capability: khive_storage::StorageCapability::Events,
        message: format!("events response deserialization failed: {error}"),
    })
}

#[cfg(unix)]
pub(super) async fn events_round_trip_attempt(
    socket_path: &Path,
    payload: &[u8],
) -> Result<Vec<u8>, (&'static str, std::io::Error)> {
    let mut stream = connect_verified(socket_path)
        .await
        .map_err(|error| ("connect", error))?;
    write_frame(&mut stream, payload)
        .await
        .map_err(|error| ("write", error))?;
    read_frame(&mut stream)
        .await
        .map_err(|error| ("read", error))
}

/// Drain every batch currently sitting in the fire-and-forget queue,
/// counting each as a dropped batch/events and logging one summary line
/// (never one line per batch — a full queue at shutdown is exactly the
/// bursty case a per-batch log would flood). Non-blocking: `try_recv` only
/// consumes what is already queued, so it terminates as soon as the queue
/// (temporarily) empties even though the sender half is still live.
#[cfg(unix)]
pub(super) fn drain_dropped_queue(
    rx: &mut tokio::sync::mpsc::Receiver<QueuedAppend>,
    counters: &ForwardingCounters,
) {
    let mut dropped_batches = 0u64;
    let mut dropped_events = 0u64;
    while let Ok(batch) = rx.try_recv() {
        dropped_batches += 1;
        dropped_events += batch.count;
        // `batch` drops its byte reservation at the end of this iteration.
    }
    if dropped_batches > 0 {
        counters
            .dropped_batches
            .fetch_add(dropped_batches, Ordering::Relaxed);
        counters
            .dropped_events
            .fetch_add(dropped_events, Ordering::Relaxed);
        tracing::warn!(
            dropped_batches,
            dropped_events,
            "events forwarder shutting down; dropping queued loss-tolerant batches"
        );
    }
}

/// The background fire-and-forget forwarder: drains the bounded queue into
/// framed appends on its own connection, reconnecting with backoff. A batch
/// that cannot be delivered is dropped and counted — never retried, never
/// blocking the queue behind it.
#[cfg(unix)]
pub(super) async fn run_forwarder(
    socket_path: PathBuf,
    mut rx: tokio::sync::mpsc::Receiver<QueuedAppend>,
    counters: Arc<ForwardingCounters>,
    outage_logged: Arc<AtomicBool>,
    delivery_timeout: Duration,
    shutdown: tokio_util::sync::CancellationToken,
) {
    // The client lives in a process-global registry, so its sender is never
    // dropped and `rx.recv()` alone would keep this tracked task alive
    // through daemon shutdown, forcing `drain()` to its full timeout.
    // Observe the shutdown token directly: queued batches at shutdown are a
    // counted drop, exactly the lane's loss-tolerant contract — enforced by
    // draining and counting `rx` below on every shutdown exit, not just
    // implied by the comment.
    let mut conn: Option<UnixStream> = None;
    loop {
        let batch = tokio::select! {
            _ = shutdown.cancelled() => {
                drain_dropped_queue(&mut rx, &counters);
                break;
            },
            received = rx.recv() => match received {
                Some(batch) => batch,
                None => break,
            },
        };
        let count = batch.count;
        let payload = match serde_json::to_vec(&batch.request) {
            Ok(payload) => payload,
            Err(error) => {
                counters.dropped_batches.fetch_add(1, Ordering::Relaxed);
                counters.dropped_events.fetch_add(count, Ordering::Relaxed);
                tracing::error!(error = %error, "events forwarder serialization failed; batch dropped");
                continue;
            }
        };

        // The delivery itself must stay both cancellable and bounded: a
        // connected but non-responding daemon would otherwise park this
        // tracked task inside write_frame/read_frame, beyond the reach of the
        // recv-side shutdown select — and daemon drain would wait its full
        // timeout on it. Shutdown mid-delivery abandons the batch (the
        // lane's loss-tolerant contract); a timeout poisons the connection,
        // since the peer may answer the abandoned frame later.
        let delivered = tokio::select! {
            _ = shutdown.cancelled() => {
                counters.dropped_batches.fetch_add(1, Ordering::Relaxed);
                counters.dropped_events.fetch_add(count, Ordering::Relaxed);
                tracing::warn!(
                    dropped_events = count,
                    "events forwarder shutting down mid-delivery; in-flight batch dropped"
                );
                drain_dropped_queue(&mut rx, &counters);
                break;
            },
            outcome = tokio::time::timeout(
                delivery_timeout,
                deliver_batch(&socket_path, &mut conn, &payload),
            ) => match outcome {
                Ok(delivered) => delivered,
                Err(_elapsed) => {
                    conn = None;
                    false
                }
            },
        };
        if delivered {
            counters.forwarded_batches.fetch_add(1, Ordering::Relaxed);
            counters
                .forwarded_events
                .fetch_add(count, Ordering::Relaxed);
            if outage_logged.swap(false, Ordering::Relaxed) {
                tracing::info!("events daemon reachable again; forwarding resumed");
            }
        } else {
            counters.dropped_batches.fetch_add(1, Ordering::Relaxed);
            counters.dropped_events.fetch_add(count, Ordering::Relaxed);
            if !outage_logged.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    socket = %socket_path.display(),
                    "events daemon unreachable; dropping loss-tolerant events until it returns"
                );
            }
            tokio::select! {
                _ = shutdown.cancelled() => {
                    drain_dropped_queue(&mut rx, &counters);
                    break;
                },
                _ = tokio::time::sleep(FORWARDER_BACKOFF) => {}
            }
        }
    }
}

/// Try to deliver one framed append over the forwarder connection,
/// (re)connecting at most once. Returns whether the daemon acknowledged.
/// Connections are peer-verified: a socket answered by a foreign uid is a
/// failed connect, never a delivery target.
#[cfg(unix)]
pub(super) async fn deliver_batch(
    socket_path: &Path,
    conn: &mut Option<UnixStream>,
    payload: &[u8],
) -> bool {
    for _attempt in 0..2u8 {
        if conn.is_none() {
            match connect_verified(socket_path).await {
                Ok(stream) => *conn = Some(stream),
                Err(_) => return false,
            }
        }
        let stream = conn.as_mut().expect("connection populated above");
        let ok = async {
            write_frame(stream, payload).await?;
            let bytes = read_frame(stream).await?;
            std::io::Result::Ok(bytes)
        }
        .await;
        match ok {
            Ok(bytes) => {
                return !matches!(
                    serde_json::from_slice::<EventsResponse>(&bytes),
                    Ok(EventsResponse::Error { .. }) | Err(_)
                );
            }
            Err(_) => {
                // Stale connection (daemon restarted): drop it and retry once
                // with a fresh connect; a second failure is a real outage.
                *conn = None;
            }
        }
    }
    false
}
