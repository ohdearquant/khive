//! Events-daemon split (ADR-170): the audit lane leaves the domain store.
//!
//! The ADR-133 idempotent audit batch is the measured bulk of event write
//! volume, and in a single-store deployment its rows queue on the same SQLite
//! writer lane as domain mutations. This module moves that lane into a
//! dedicated events daemon that owns the events database, reachable over a
//! Unix socket with the same length-prefixed framing and peer-uid admission the
//! main daemon socket uses. Plain event appends stay on the domain store —
//! the legacy `events` table has raw-SQL consumers (schedule provenance, kg
//! projection guards, graph-query substrate unions) whose correctness
//! depends on finding those rows there.
//!
//! Cooperating pieces:
//!
//! - [`run_events_daemon`] — the server loop the `events-daemon` subcommand
//!   runs: binds the events socket, owns the only resident writer of the
//!   events database, and serves append/read requests through the ordinary
//!   `SqlEventStore`.
//! - [`EventsSplitClient`] — one per domain process. Plain appends ride a
//!   bounded in-memory queue drained by a background forwarder
//!   (fire-and-forget; overflow or a dead daemon drops the batch, counts it,
//!   and logs — the loss-tolerant durability class made concrete; unused by
//!   the default routing until telemetry producers opt in). Idempotent
//!   audit-batch appends and reads are synchronous framed round-trips with
//!   bounded timeouts, because their callers are background flushers or query
//!   paths, never the dispatch hot path.
//! - [`ForwardingEventStore`] — the lane-side [`EventStore`] over the socket.
//!   Preflight validation delegates to an in-memory `SqlEventStore`, so the
//!   ADR-133 audit-batch seam keeps its pre-enqueue shape check without any
//!   I/O on the dispatch path.
//! - [`SplitEventStore`] — the per-namespace handle
//!   [`crate::runtime::KhiveRuntime::events`] returns when the split is
//!   configured: routes the idempotent lane to the events store, plain
//!   appends to the legacy store, and merges reads across both.
//!
//! Domain availability never depends on events-daemon liveness: every failure
//! path here degrades (drop + count + log, or a typed storage error for the
//! synchronous lanes) instead of blocking the caller.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use async_trait::async_trait;
use khive_db::StorageBackend;
use khive_storage::event::IdempotentEventBatchResult;
use khive_storage::{
    BatchWriteSummary, Event, EventFilter, EventStore, Page, PageRequest, StorageError,
    StorageResult,
};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use uuid::Uuid;

#[cfg(unix)]
use crate::daemon::{read_frame, write_frame};

/// Bump whenever the request or response frame shape changes incompatibly.
/// The server rejects frames whose version it does not speak, so a skewed
/// client gets a typed refusal instead of a deserialization panic.
pub const EVENTS_PROTOCOL_VERSION: u32 = 1;

/// Default bound on the fire-and-forget append queue, in batches. The loss
/// window on overflow is this depth times the batch size in flight; the value
/// is deliberately generous because entries are pointers, not rows.
#[cfg(unix)]
pub const DEFAULT_APPEND_QUEUE_BATCHES: usize = 4096;

/// Timeout for one synchronous round-trip (idempotent appends, reads).
/// Covers the WHOLE attempt — connect, peer verification, write, read —
/// because each round-trip owns its connection (no shared lock to wait on
/// outside the clock).
#[cfg(unix)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff for the background forwarder after a failed connect.
#[cfg(unix)]
const FORWARDER_BACKOFF: Duration = Duration::from_secs(2);

/// Default events database file, beside the main database file.
///
/// The name is derived from the main database's full file name (`khive.db` →
/// `khive.db.events.db`), never from its stem and never a fixed name in the
/// parent directory: two independent databases that happen to share a
/// directory (`a.db`, `b.db`) — or a stem (`a.db`, `a.sqlite`) — must each
/// get their own event plane, not silently share one. The parent directory
/// is canonicalized when it resolves, so path aliases of one database
/// (relative spellings, symlinked directories) map to one sidecar instead of
/// minting a distinct events database per spelling.
pub fn events_db_path_beside(main_db: &Path) -> PathBuf {
    let mut name = main_db
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from("khive.db"));
    name.push(".events.db");
    match main_db.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => std::fs::canonicalize(dir)
            .unwrap_or_else(|_| dir.to_path_buf())
            .join(&name),
        None => PathBuf::from(name),
    }
}

/// Events daemon socket path, beside the events database it serves.
///
/// Derived from the events db path (not a process-global location, not a
/// fixed name in the parent directory) so every events database gets its own
/// daemon and socket: a shared socket would route events from any second
/// database (another seat, a test tempdir) to whichever daemon happens to
/// own it, persisting them beside the wrong main store. `khive.db.events.db`
/// yields `khive.db.events.sock`; the daemon's advisory lock is the same
/// path with a `.lock` extension.
pub fn events_socket_path_beside(events_db: &Path) -> PathBuf {
    events_db.with_extension("sock")
}

/// How a runtime reaches event storage when the split is configured.
#[derive(Debug, Clone)]
pub struct EventsSplitConfig {
    /// The events database file. The events daemon is its only writer in
    /// daemon deployments; embedded mode writes it directly.
    pub db_path: PathBuf,
    /// `Some(socket)` = forward appends to the events daemon at this socket
    /// (daemon deployments). `None` = embedded mode: open `db_path` directly
    /// in-process (one-shot CLI, tests).
    pub socket_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Process-global handles
// ---------------------------------------------------------------------------
//
// One events-split client (or one direct backend) per process, whatever the
// number of runtimes and clones — the same shape as the daemon module's other
// process-global state. Keyed by path so tests exercising two distinct paths
// in one process stay isolated.

#[cfg(unix)]
type ClientMap = std::collections::HashMap<PathBuf, Arc<EventsSplitClient>>;
/// Keyed by (path, read_only): a read-only open and a writable open of the
/// same file are different pools with different guarantees and must never be
/// handed out interchangeably.
type BackendMap = std::collections::HashMap<(PathBuf, bool), Arc<StorageBackend>>;

#[cfg(unix)]
fn client_registry() -> &'static std::sync::Mutex<ClientMap> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<ClientMap>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn direct_backend_registry() -> &'static std::sync::Mutex<BackendMap> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<BackendMap>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The process-wide client for `socket_path`, created (and its forwarder
/// spawned) on first use. Requires a tokio runtime context on first call.
#[cfg(unix)]
pub fn client_for(socket_path: &Path) -> crate::error::RuntimeResult<Arc<EventsSplitClient>> {
    let mut registry = client_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = registry.get(socket_path) {
        return Ok(Arc::clone(existing));
    }
    let client = EventsSplitClient::new(socket_path.to_path_buf())?;
    registry.insert(socket_path.to_path_buf(), Arc::clone(&client));
    Ok(client)
}

/// The process-wide direct (embedded-mode) backend for `db_path`, opened
/// read-write on first use.
pub fn direct_backend_for(db_path: &Path) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    direct_backend(db_path, false)
}

/// The process-wide direct backend for `db_path`, opened READ-ONLY on first
/// use. The file must already exist — this constructor never creates or
/// schema-initializes an events database, which is what a read-only runtime's
/// no-DB-creation contract requires of its event lane.
pub fn direct_backend_read_only_for(
    db_path: &Path,
) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    direct_backend(db_path, true)
}

fn direct_backend(
    db_path: &Path,
    read_only: bool,
) -> crate::error::RuntimeResult<Arc<StorageBackend>> {
    let mut registry = direct_backend_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (db_path.to_path_buf(), read_only);
    if let Some(existing) = registry.get(&key) {
        return Ok(Arc::clone(existing));
    }
    let backend = Arc::new(if read_only {
        StorageBackend::sqlite_read_only(db_path)?
    } else {
        StorageBackend::sqlite(db_path)?
    });
    registry.insert(key, Arc::clone(&backend));
    Ok(backend)
}

/// Forwarding metrics for the process-wide client at `socket_path`, if one
/// exists. `None` means the split never initialized in this process.
#[cfg(unix)]
pub fn forwarding_metrics(socket_path: &Path) -> Option<EventsForwardingMetrics> {
    let registry = client_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.get(socket_path).map(|client| client.metrics())
}

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// Request frame sent from a domain process to the events daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EventsRequest {
    /// Fire-and-forget append lane. The daemon replies with a summary, but the
    /// forwarder treats a failed reply as a counted drop, never an error to
    /// the original caller.
    AppendEvents {
        protocol_version: u32,
        namespace: String,
        events: Vec<Event>,
    },
    /// ADR-133 audit-batch lane: real dispositions come back.
    AppendEventsIdempotent {
        protocol_version: u32,
        namespace: String,
        events: Vec<Event>,
    },
    GetEvent {
        protocol_version: u32,
        namespace: String,
        id: Uuid,
    },
    QueryEvents {
        protocol_version: u32,
        namespace: String,
        filter: EventFilter,
        page: PageRequest,
    },
    CountEvents {
        protocol_version: u32,
        namespace: String,
        filter: EventFilter,
    },
}

impl EventsRequest {
    fn protocol_version(&self) -> u32 {
        match self {
            Self::AppendEvents {
                protocol_version, ..
            }
            | Self::AppendEventsIdempotent {
                protocol_version, ..
            }
            | Self::GetEvent {
                protocol_version, ..
            }
            | Self::QueryEvents {
                protocol_version, ..
            }
            | Self::CountEvents {
                protocol_version, ..
            } => *protocol_version,
        }
    }

    fn namespace(&self) -> &str {
        match self {
            Self::AppendEvents { namespace, .. }
            | Self::AppendEventsIdempotent { namespace, .. }
            | Self::GetEvent { namespace, .. }
            | Self::QueryEvents { namespace, .. }
            | Self::CountEvents { namespace, .. } => namespace,
        }
    }
}

/// Response frame from the events daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventsResponse {
    Appended {
        summary: BatchWriteSummary,
    },
    Idempotent {
        result: IdempotentEventBatchResult,
    },
    Event {
        event: Option<Event>,
    },
    Pageful {
        page: Page<Event>,
    },
    Count {
        count: u64,
    },
    /// Typed refusal. `retryable` distinguishes transient daemon-side
    /// conditions from contract errors (bad frame, version skew).
    Error {
        message: String,
        retryable: bool,
    },
}

// ---------------------------------------------------------------------------
// Server side
// ---------------------------------------------------------------------------

/// Advisory lock guaranteeing at most one events daemon per socket path.
/// Held for the daemon's lifetime; a second daemon exits instead of stealing
/// the socket path from the live one.
#[cfg(unix)]
pub struct EventsDaemonGuard {
    _file: std::fs::File,
}

/// Try to become the events daemon for `socket_path`. `None` = another
/// events daemon already holds the lock.
#[cfg(unix)]
pub fn try_acquire_events_daemon_guard(socket_path: &Path) -> Option<EventsDaemonGuard> {
    let lock_path = socket_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .ok()?;
    use std::os::fd::AsRawFd;
    // SAFETY: `fd` is a live descriptor owned by `file` for the duration of
    // the call; `flock` reads nothing else.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Some(EventsDaemonGuard { _file: file })
    } else {
        None
    }
}

/// Supervise the events daemon from the main daemon process: probe the
/// socket periodically and (re)spawn the daemon subcommand when unreachable.
///
/// The spawned command contract is fixed here once: the current executable
/// re-invoked as `events-daemon --db <db> --socket <socket>` — the subcommand
/// the kernel binary registers for [`run_events_daemon`]. The child holds the
/// per-socket advisory lock, so a probe/spawn race resolves to one survivor.
///
/// Lifecycle: the loop observes the process-wide daemon shutdown token, so
/// `drain()` never waits on it forever, and it retains the handle of the
/// child it spawned — reaping it with `try_wait` on every probe (no zombie
/// accumulation during a persistent startup failure) and never stacking a
/// second spawn on a still-live child. On shutdown, a child this supervisor
/// spawned is killed and reaped; a pre-existing events daemon it never
/// spawned is left alone.
///
/// The reachability probe uses the peer-verified connect: a socket answered
/// by a foreign-uid process is treated as UNREACHABLE (and logged loudly),
/// so a pre-bound spoof socket triggers a real-daemon spawn instead of being
/// reported healthy.
#[cfg(unix)]
pub async fn supervise_events_daemon(db_path: PathBuf, socket_path: PathBuf) {
    const PROBE_INTERVAL: Duration = Duration::from_secs(15);
    let shutdown = crate::daemon::daemon_shutdown_token();
    let mut child: Option<std::process::Child> = None;
    let mut respawns: u64 = 0;
    loop {
        // Reap first: a child that exited (crashed, lost the advisory lock
        // race, refused an untrusted directory) must not linger as a zombie,
        // and clearing the slot is what re-arms the spawn below.
        if let Some(c) = child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    tracing::info!(%status, "events daemon child exited");
                    child = None;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "cannot poll events daemon child; dropping handle");
                    child = None;
                }
            }
        }

        let reachable = match connect_verified(&socket_path).await {
            Ok(_stream) => true,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!(
                    socket = %socket_path.display(),
                    error = %error,
                    "events socket answered by a foreign uid; treating as unreachable"
                );
                false
            }
            Err(_) => false,
        };
        if !reachable && child.is_none() {
            match std::env::current_exe() {
                Ok(exe) => {
                    let spawned = std::process::Command::new(exe)
                        .arg("events-daemon")
                        .arg("--db")
                        .arg(&db_path)
                        .arg("--socket")
                        .arg(&socket_path)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    match spawned {
                        Ok(spawned_child) => {
                            respawns += 1;
                            tracing::info!(
                                pid = spawned_child.id(),
                                respawns,
                                socket = %socket_path.display(),
                                "spawned events daemon"
                            );
                            child = Some(spawned_child);
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "failed to spawn events daemon");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "cannot resolve current executable for events daemon spawn");
                }
            }
        }
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(PROBE_INTERVAL) => {}
        }
    }
    if let Some(mut c) = child.take() {
        // Our child, our cleanup: the events daemon holds no volatile queue
        // state (SQLite is the durability), and the next daemon host's
        // supervisor respawns it.
        let _ = c.kill();
        let _ = c.wait();
        tracing::info!("events daemon child stopped with supervisor shutdown");
    }
}

/// Serve the events daemon loop on `socket_path`, owning `db_path`.
///
/// Binds the socket (removing a stale path first), then accepts connections
/// for the process lifetime. Each connection is served sequentially:
/// same-uid admission, then a read-frame → dispatch → write-frame loop until
/// the peer disconnects. All storage goes through `SqlEventStore` on a
/// backend opened read-write against `db_path`; the events schema is ensured
/// once at boot.
///
/// Bind-path trust mirrors the main daemon socket: the socket directory must
/// pass the same ownership/mode/swap-resistance validation, and the bound
/// socket entry is chmod'd 0600 fail-closed. Pathname reachability is not
/// identity — clients additionally verify the peer uid on every connect —
/// but a hardened bind path is what keeps the *bind* itself out of another
/// user's hands.
#[cfg(unix)]
pub async fn run_events_daemon(db_path: &Path, socket_path: &Path) -> anyhow::Result<()> {
    let Some(_guard) = try_acquire_events_daemon_guard(socket_path) else {
        tracing::info!(
            socket = %socket_path.display(),
            "another events daemon holds the lock; exiting"
        );
        return Ok(());
    };
    let backend = Arc::new(StorageBackend::sqlite(db_path)?);
    // Ensure the schema once, loudly, before accepting traffic.
    backend.events()?;

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::daemon::ensure_socket_dir_is_trusted(parent)?;
    }
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        // Fail closed, same contract as the main daemon socket: if the entry
        // cannot be made owner-only, drop the listener and remove it rather
        // than serve a world-reachable socket the design never covered.
        if let Err(e) =
            std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        {
            drop(listener);
            let _ = std::fs::remove_file(socket_path);
            anyhow::bail!(
                "refusing to serve events: cannot chmod 0600 {}: {e}. The events socket must \
                 be owner-only.",
                socket_path.display()
            );
        }
    }
    let daemon_euid = unsafe { libc::geteuid() };
    tracing::info!(
        socket = %socket_path.display(),
        db = %db_path.display(),
        "events daemon listening"
    );

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(error = %error, "events daemon accept failed");
                continue;
            }
        };
        match crate::daemon::peer_uid(&stream) {
            Ok(uid) if crate::daemon::uid_is_permitted(uid, daemon_euid) => {}
            Ok(uid) => {
                tracing::warn!(peer_uid = uid, "events daemon rejected foreign-uid peer");
                continue;
            }
            Err(error) => {
                tracing::warn!(error = %error, "events daemon could not read peer credentials");
                continue;
            }
        }
        let backend = Arc::clone(&backend);
        crate::daemon::spawn_tracked_task(async move {
            serve_events_conn(stream, backend).await;
        });
    }
}

#[cfg(unix)]
async fn serve_events_conn(mut stream: UnixStream, backend: Arc<StorageBackend>) {
    loop {
        let payload = match read_frame(&mut stream).await {
            Ok(bytes) => bytes,
            // Includes clean EOF on peer disconnect.
            Err(_) => return,
        };
        let response = match serde_json::from_slice::<EventsRequest>(&payload) {
            Ok(request) => dispatch_events_request(request, &backend).await,
            Err(error) => EventsResponse::Error {
                message: format!("events daemon could not parse request frame: {error}"),
                retryable: false,
            },
        };
        let bytes = match serde_json::to_vec(&response) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "events daemon response serialization failed");
                return;
            }
        };
        if write_frame(&mut stream, &bytes).await.is_err() {
            return;
        }
    }
}

#[cfg(unix)]
async fn dispatch_events_request(
    request: EventsRequest,
    backend: &StorageBackend,
) -> EventsResponse {
    if request.protocol_version() != EVENTS_PROTOCOL_VERSION {
        return EventsResponse::Error {
            message: format!(
                "events protocol version mismatch: daemon speaks {}, client sent {}",
                EVENTS_PROTOCOL_VERSION,
                request.protocol_version()
            ),
            retryable: false,
        };
    }
    let store = match backend.events_for_namespace(request.namespace()) {
        Ok(store) => store,
        Err(error) => {
            return EventsResponse::Error {
                message: format!("events store unavailable: {error}"),
                retryable: true,
            };
        }
    };
    match request {
        EventsRequest::AppendEvents { events, .. } => match store.append_events(events).await {
            Ok(summary) => EventsResponse::Appended { summary },
            Err(error) => storage_error_response(&error),
        },
        EventsRequest::AppendEventsIdempotent { events, .. } => {
            match store.append_events_idempotent(events).await {
                Ok(result) => EventsResponse::Idempotent { result },
                Err(error) => storage_error_response(&error),
            }
        }
        EventsRequest::GetEvent { id, .. } => match store.get_event(id).await {
            Ok(event) => EventsResponse::Event { event },
            Err(error) => storage_error_response(&error),
        },
        EventsRequest::QueryEvents { filter, page, .. } => {
            match store.query_events(filter, page).await {
                Ok(page) => EventsResponse::Pageful { page },
                Err(error) => storage_error_response(&error),
            }
        }
        EventsRequest::CountEvents { filter, .. } => match store.count_events(filter).await {
            Ok(count) => EventsResponse::Count { count },
            Err(error) => storage_error_response(&error),
        },
    }
}

#[cfg(unix)]
fn storage_error_response(error: &StorageError) -> EventsResponse {
    EventsResponse::Error {
        message: error.to_string(),
        // Defer to the storage layer's own transience classifier rather than
        // re-enumerating variants here: a hand-rolled subset silently turns
        // transient writer contention (`WriterTaskBusy`, `Transaction`) into
        // a terminal error on the client side of the socket.
        retryable: error.is_retryable(),
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Connect to the events socket and verify who answered. Pathname
/// reachability is not identity: any process that can write the directory can
/// pre-bind the path. The kernel-reported peer uid is the one identity the
/// far side cannot choose, so every client connect — round-trips, the
/// forwarder, the supervisor probe — refuses a socket answered by a foreign
/// uid before a single frame is written.
#[cfg(unix)]
async fn connect_verified(socket_path: &Path) -> std::io::Result<UnixStream> {
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
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct ForwardingCounters {
    forwarded_batches: AtomicU64,
    forwarded_events: AtomicU64,
    dropped_batches: AtomicU64,
    dropped_events: AtomicU64,
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
    socket_path: PathBuf,
    append_tx: tokio::sync::mpsc::Sender<(String, Vec<Event>)>,
    counters: Arc<ForwardingCounters>,
    /// Flipped by the forwarder while the daemon is unreachable so the drop
    /// log fires once per outage, not once per batch.
    outage_logged: Arc<AtomicBool>,
    /// In-memory validator backing `preflight_event` without I/O.
    preflight_store: Arc<dyn EventStore>,
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
        let preflight_backend = StorageBackend::memory()?;
        let preflight_store = preflight_backend.events()?;
        // The in-memory backend must outlive the store handle; the store holds
        // the pool Arc internally, so dropping the backend wrapper here is fine.

        let (append_tx, append_rx) =
            tokio::sync::mpsc::channel::<(String, Vec<Event>)>(queue_depth.max(1));
        let counters = Arc::new(ForwardingCounters::default());
        let outage_logged = Arc::new(AtomicBool::new(false));

        let client = Arc::new(Self {
            socket_path: socket_path.clone(),
            append_tx,
            counters: Arc::clone(&counters),
            outage_logged: Arc::clone(&outage_logged),
            preflight_store,
        });

        crate::daemon::spawn_tracked_task(run_forwarder(
            socket_path,
            append_rx,
            counters,
            outage_logged,
        ));
        Ok(client)
    }

    /// Snapshot of the fire-and-forget lane's health.
    pub fn metrics(&self) -> EventsForwardingMetrics {
        EventsForwardingMetrics {
            forwarded_batches: self.counters.forwarded_batches.load(Ordering::Relaxed),
            forwarded_events: self.counters.forwarded_events.load(Ordering::Relaxed),
            dropped_batches: self.counters.dropped_batches.load(Ordering::Relaxed),
            dropped_events: self.counters.dropped_events.load(Ordering::Relaxed),
        }
    }

    /// Enqueue a batch on the fire-and-forget lane. Never blocks; a full
    /// queue is a counted, logged drop.
    fn enqueue(&self, namespace: &str, events: Vec<Event>) {
        let count = events.len() as u64;
        match self.append_tx.try_send((namespace.to_string(), events)) {
            Ok(()) => {}
            Err(_) => {
                self.counters
                    .dropped_batches
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .dropped_events
                    .fetch_add(count, Ordering::Relaxed);
                if !self.outage_logged.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        dropped_events = count,
                        "events append queue full; dropping loss-tolerant events until it drains"
                    );
                }
            }
        }
    }

    /// One synchronous framed round-trip with a bounded timeout, on its own
    /// fresh connection. No connection is shared between concurrent callers,
    /// so no caller ever waits on another's stall, and `REQUEST_TIMEOUT`
    /// bounds the whole attempt — connect, peer verification, write, read.
    async fn round_trip(&self, request: &EventsRequest) -> StorageResult<EventsResponse> {
        let op = "events daemon round-trip";
        let payload = serde_json::to_vec(request).map_err(|error| StorageError::Serialization {
            capability: khive_storage::StorageCapability::Events,
            message: format!("events request serialization failed: {error}"),
        })?;

        let attempt = async {
            let mut stream = connect_verified(&self.socket_path).await?;
            write_frame(&mut stream, &payload).await?;
            let bytes = read_frame(&mut stream).await?;
            std::io::Result::Ok(bytes)
        };
        let bytes = match tokio::time::timeout(REQUEST_TIMEOUT, attempt).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                return Err(StorageError::Pool {
                    operation: op.into(),
                    message: format!(
                        "events daemon unreachable at {}: {error}",
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

        serde_json::from_slice::<EventsResponse>(&bytes).map_err(|error| {
            StorageError::Serialization {
                capability: khive_storage::StorageCapability::Events,
                message: format!("events response deserialization failed: {error}"),
            }
        })
    }
}

/// The background fire-and-forget forwarder: drains the bounded queue into
/// framed appends on its own connection, reconnecting with backoff. A batch
/// that cannot be delivered is dropped and counted — never retried, never
/// blocking the queue behind it.
#[cfg(unix)]
async fn run_forwarder(
    socket_path: PathBuf,
    mut rx: tokio::sync::mpsc::Receiver<(String, Vec<Event>)>,
    counters: Arc<ForwardingCounters>,
    outage_logged: Arc<AtomicBool>,
) {
    let mut conn: Option<UnixStream> = None;
    while let Some((namespace, events)) = rx.recv().await {
        let count = events.len() as u64;
        let request = EventsRequest::AppendEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace,
            events,
        };
        let payload = match serde_json::to_vec(&request) {
            Ok(payload) => payload,
            Err(error) => {
                counters.dropped_batches.fetch_add(1, Ordering::Relaxed);
                counters.dropped_events.fetch_add(count, Ordering::Relaxed);
                tracing::error!(error = %error, "events forwarder serialization failed; batch dropped");
                continue;
            }
        };

        let delivered = deliver_batch(&socket_path, &mut conn, &payload).await;
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
            tokio::time::sleep(FORWARDER_BACKOFF).await;
        }
    }
}

/// Try to deliver one framed append over the forwarder connection,
/// (re)connecting at most once. Returns whether the daemon acknowledged.
/// Connections are peer-verified: a socket answered by a foreign uid is a
/// failed connect, never a delivery target.
#[cfg(unix)]
async fn deliver_batch(socket_path: &Path, conn: &mut Option<UnixStream>, payload: &[u8]) -> bool {
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

// ---------------------------------------------------------------------------
// The per-namespace EventStore handle
// ---------------------------------------------------------------------------

/// [`EventStore`] implementation the runtime hands out when the events split
/// runs in daemon mode. Appends are fire-and-forget through the client's
/// bounded queue; the ADR-133 idempotent lane and all reads are synchronous
/// round-trips to the events daemon.
#[cfg(unix)]
#[derive(Debug)]
pub struct ForwardingEventStore {
    namespace: String,
    client: Arc<EventsSplitClient>,
}

#[cfg(unix)]
impl ForwardingEventStore {
    pub fn new(namespace: impl Into<String>, client: Arc<EventsSplitClient>) -> Self {
        Self {
            namespace: namespace.into(),
            client,
        }
    }

    fn unexpected(&self, op: &'static str, response: EventsResponse) -> StorageError {
        match response {
            EventsResponse::Error { message, retryable } => {
                if retryable {
                    StorageError::Pool {
                        operation: op.into(),
                        message,
                    }
                } else {
                    StorageError::InvalidInput {
                        capability: khive_storage::StorageCapability::Events,
                        operation: op.into(),
                        message,
                    }
                }
            }
            other => StorageError::Serialization {
                capability: khive_storage::StorageCapability::Events,
                message: format!("events daemon returned mismatched response for {op}: {other:?}"),
            },
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl EventStore for ForwardingEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.client.enqueue(&self.namespace, vec![event]);
        Ok(())
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        let attempted = events.len() as u64;
        self.client.enqueue(&self.namespace, events);
        // Fire-and-forget: the hand-off succeeded or was counted as a drop;
        // either way the caller's contract is "accepted for forwarding".
        Ok(BatchWriteSummary {
            attempted,
            affected: attempted,
            failed: 0,
            first_error: String::new(),
        })
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        let request = EventsRequest::GetEvent {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            id,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Event { event } => Ok(event),
            other => Err(self.unexpected("get_event", other)),
        }
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        let request = EventsRequest::QueryEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            filter,
            page,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Pageful { page } => Ok(page),
            other => Err(self.unexpected("query_events", other)),
        }
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        let request = EventsRequest::CountEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            filter,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Count { count } => Ok(count),
            other => Err(self.unexpected("count_events", other)),
        }
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        // Same validation code path as the daemon-side store, zero I/O.
        self.client.preflight_store.preflight_event(event)
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        let request = EventsRequest::AppendEventsIdempotent {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: self.namespace.clone(),
            events,
        };
        match self.client.round_trip(&request).await? {
            EventsResponse::Idempotent { result } => Ok(result),
            other => Err(self.unexpected("append_events_idempotent", other)),
        }
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        true
    }
}

/// The store the runtime hands out when the events split is configured:
/// routes by APPEND CLASS rather than moving the whole event plane.
///
/// - The ADR-133 idempotent audit-batch lane — the measured bulk of event
///   write volume (verb-dispatch audit plus the config-lock rows that ride
///   the same flusher) — goes to the events lane (the events database, forwarded or
///   direct).
/// - Plain appends stay on the legacy store, because the legacy `events`
///   table has raw-SQL consumers whose correctness depends on finding those
///   rows there: the schedule drain's creator-provenance fence, the kg
///   projection worker's guarded event INSERT (transactional with main-db
///   state), and GraphQuery's cross-substrate UNION. Those events are
///   low-volume domain facts; the split's contention relief does not need
///   them moved, and moving them breaks the consumers by construction.
/// - Reads merge both stores so trait-level consumers
///   (`brain.event_counts`, event getters) observe one event plane.
pub struct SplitEventStore {
    legacy: Arc<dyn EventStore>,
    lane: Arc<dyn EventStore>,
}

impl std::fmt::Debug for SplitEventStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SplitEventStore").finish_non_exhaustive()
    }
}

impl SplitEventStore {
    pub fn new(legacy: Arc<dyn EventStore>, lane: Arc<dyn EventStore>) -> Self {
        Self { legacy, lane }
    }
}

#[async_trait]
impl EventStore for SplitEventStore {
    async fn append_event(&self, event: Event) -> StorageResult<()> {
        self.legacy.append_event(event).await
    }

    async fn append_events(&self, events: Vec<Event>) -> StorageResult<BatchWriteSummary> {
        self.legacy.append_events(events).await
    }

    async fn get_event(&self, id: Uuid) -> StorageResult<Option<Event>> {
        // Domain events (legacy) are the likelier and cheaper hit; fall back
        // to the lane for audit-batch rows.
        if let Some(event) = self.legacy.get_event(id).await? {
            return Ok(Some(event));
        }
        self.lane.get_event(id).await
    }

    async fn query_events(
        &self,
        filter: EventFilter,
        page: PageRequest,
    ) -> StorageResult<Page<Event>> {
        // Offset pagination cannot be split across two stores: fetch each
        // store's prefix covering the requested window, merge in the stores'
        // shared order (created_at DESC, id DESC), then window in memory.
        let prefix = PageRequest {
            offset: 0,
            limit: page
                .offset
                .saturating_add(u64::from(page.limit))
                .min(u64::from(u32::MAX)) as u32,
        };
        let legacy = self
            .legacy
            .query_events(filter.clone(), prefix.clone())
            .await?;
        let lane = self.lane.query_events(filter, prefix).await?;
        let total = match (legacy.total, lane.total) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        let mut items = legacy.items;
        items.extend(lane.items);
        items.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        let items = items
            .into_iter()
            .skip(page.offset as usize)
            .take(page.limit as usize)
            .collect();
        Ok(Page { items, total })
    }

    async fn count_events(&self, filter: EventFilter) -> StorageResult<u64> {
        let legacy = self.legacy.count_events(filter.clone()).await?;
        let lane = self.lane.count_events(filter).await?;
        Ok(legacy + lane)
    }

    fn preflight_event(&self, event: &Event) -> StorageResult<()> {
        // Validation is store-independent; the lane's implementation is the
        // local zero-I/O validator in forwarding mode.
        self.lane.preflight_event(event)
    }

    async fn append_events_idempotent(
        &self,
        events: Vec<Event>,
    ) -> StorageResult<IdempotentEventBatchResult> {
        self.lane.append_events_idempotent(events).await
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        self.lane.supports_idempotent_audit_batch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The full daemon/forwarding test suite for this module lands with the
    // final slice of this series; these tests cover the naming and
    // classification contracts the module itself defines.

    #[test]
    fn sidecar_name_derives_from_the_full_file_name() {
        let path = events_db_path_beside(Path::new("/data/khive.db"));
        assert!(path.ends_with("khive.db.events.db"), "got {path:?}");
    }

    #[test]
    fn databases_sharing_a_stem_get_distinct_sidecars() {
        let a = events_db_path_beside(Path::new("/data/a.db"));
        let b = events_db_path_beside(Path::new("/data/a.sqlite"));
        assert_ne!(a, b, "a.db and a.sqlite must not share an event plane");
        assert!(a.ends_with("a.db.events.db"), "got {a:?}");
        assert!(b.ends_with("a.sqlite.events.db"), "got {b:?}");
    }

    #[cfg(unix)]
    #[test]
    fn directory_aliases_resolve_to_one_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert_eq!(
            events_db_path_beside(&real.join("khive.db")),
            events_db_path_beside(&alias.join("khive.db")),
            "a symlinked spelling of one directory must not mint a second sidecar"
        );
    }

    #[test]
    fn socket_derives_beside_the_sidecar() {
        let db = events_db_path_beside(Path::new("/data/khive.db"));
        let socket = events_socket_path_beside(&db);
        assert!(socket.ends_with("khive.db.events.sock"), "got {socket:?}");
    }

    #[cfg(unix)]
    #[test]
    fn wire_retryability_follows_the_storage_classifier() {
        let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
        assert!(
            matches!(
                busy,
                EventsResponse::Error {
                    retryable: true,
                    ..
                }
            ),
            "transient writer contention must stay retryable across the socket"
        );
        // A terminated writer task is not retryable per the storage layer's
        // own classifier; the wire response must agree rather than widen it.
        let terminated = storage_error_response(&StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        });
        assert!(
            matches!(
                terminated,
                EventsResponse::Error {
                    retryable: false,
                    ..
                }
            ),
            "a terminated writer must not be reported transient"
        );
    }
}
