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

/// Cap on concurrently served daemon connections. Admission control, not a
/// correctness bound: every permitted-uid process on the machine shares the
/// daemon, and without a cap each accepted connection holds a task and a file
/// descriptor for as long as the peer keeps the socket open — a stalled or
/// hostile same-uid peer could exhaust both. Excess connects are dropped
/// (peer sees EOF) and retried by the clients' own reconnect paths.
#[cfg(unix)]
const MAX_EVENTS_CONNECTIONS: usize = 128;

/// Per-frame I/O deadline on a served connection: the longest the daemon
/// waits for one request frame to finish arriving, or one response frame to
/// finish sending. A peer that sends a partial frame and stalls is closed
/// instead of holding its task and descriptor forever. Idle well-behaved
/// clients are closed too — that is fine by construction: round-trips open a
/// fresh connection per call, and the forwarder's delivery path retries once
/// on a stale connection.
#[cfg(unix)]
const CONN_IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap on the per-namespace store cache. Stores are cheap pool handles, but
/// the namespace string is client-supplied, so an unbounded map is
/// attacker-controlled memory growth. Beyond the cap, stores are built
/// per-request instead of cached — slower, never wrong.
#[cfg(unix)]
const MAX_CACHED_NAMESPACE_STORES: usize = 1024;

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
    let path = match main_db.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => std::fs::canonicalize(dir)
            .unwrap_or_else(|_| dir.to_path_buf())
            .join(&name),
        None => PathBuf::from(name),
    };
    absolutize(&path)
}

/// Anchor a relative path to the current working directory. A bare relative
/// spelling (`khive.db`) yields `Some("")` from `Path::parent`, and every
/// consumer downstream — lock-file parenting, socket-directory trust
/// validation, the daemon spawn contract — needs a real directory to stat,
/// not an empty string. Absolute paths pass through untouched.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
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
    // Embedded writable mode holds the events database to the daemon's own
    // contract: owner-only from the first byte, never at the process umask,
    // and — because event rows carry the same audit payloads either way — a
    // pre-existing database or -wal/-shm sidecar is tightened to 0600 too,
    // fail-closed. A create race just means SQLite finds the file present.
    #[cfg(unix)]
    if !read_only {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(db_path);
        harden_events_db_sidecars(db_path)
            .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
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
    /// `writer_task_state` preserves the ADR-133 dispositions the retryable
    /// bit cannot carry: the daemon-side writer terminated in a state the
    /// audit-batch retry classifier keys on per-variant — `NotStarted` and
    /// `TransactionRolledBack` are safe replays, `SideEffectsUnknown` gates
    /// double-send decisions. Flattening any of them into a generic refusal
    /// turns a mandated retry into a terminal failure on the client side.
    /// `side_effects_unknown` is the older single-state spelling, kept so
    /// frames from a daemon one protocol build older still carry that one
    /// state; `#[serde(default)]` keeps both directions skew-tolerant.
    Error {
        message: String,
        retryable: bool,
        #[serde(default)]
        side_effects_unknown: bool,
        #[serde(default)]
        writer_task_state: Option<WireWriterTaskState>,
    },
}

/// Wire mirror of [`khive_storage::WriterTaskRequestState`]. A separate type
/// because the storage enum is not serializable and the wire shape must stay
/// under this module's protocol-version control, not the storage crate's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireWriterTaskState {
    NotStarted,
    TransactionRolledBack,
    SideEffectsUnknown,
}

impl From<khive_storage::WriterTaskRequestState> for WireWriterTaskState {
    fn from(state: khive_storage::WriterTaskRequestState) -> Self {
        use khive_storage::WriterTaskRequestState as S;
        match state {
            S::NotStarted => Self::NotStarted,
            S::TransactionRolledBack => Self::TransactionRolledBack,
            S::SideEffectsUnknown => Self::SideEffectsUnknown,
        }
    }
}

impl From<WireWriterTaskState> for khive_storage::WriterTaskRequestState {
    fn from(state: WireWriterTaskState) -> Self {
        use khive_storage::WriterTaskRequestState as S;
        match state {
            WireWriterTaskState::NotStarted => S::NotStarted,
            WireWriterTaskState::TransactionRolledBack => S::TransactionRolledBack,
            WireWriterTaskState::SideEffectsUnknown => S::SideEffectsUnknown,
        }
    }
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
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .ok()?;
    // `mode` applies only at creation; tighten a pre-existing lock file too.
    // Best-effort: the file carries no data (it exists to be flocked), so a
    // failed chmod is not grounds to refuse the lock.
    let _ = std::fs::set_permissions(
        &lock_path,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    );
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

/// Create the events database file owner-only if it does not exist yet.
/// The 0600 socket and peer-uid admission bound who can *talk to* the
/// daemon; they bound nothing if the database file itself is readable by
/// other local users, so the daemon creates it 0600 before SQLite ever
/// opens it (SQLite would otherwise create it at the process umask).
#[cfg(unix)]
fn ensure_events_db_owner_only(db_path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(db_path)
    {
        // A zero-byte file is a valid empty SQLite database.
        Ok(_created) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "refusing to serve events: cannot create {} owner-only: {e}",
            db_path.display()
        )),
    }
}

/// Tighten the events database and its SQLite sidecars to owner-only.
/// Fail closed, same contract as the socket chmod: a daemon that cannot
/// keep its database owner-only must not serve it.
#[cfg(unix)]
fn harden_events_db_sidecars(db_path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut targets = vec![db_path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        targets.push(PathBuf::from(name));
    }
    for path in targets {
        if !path.exists() {
            continue;
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
            anyhow::anyhow!(
                "refusing to serve events: cannot chmod 0600 {}: {e}. The events database \
                 and its sidecars must be owner-only.",
                path.display()
            )
        })?;
    }
    Ok(())
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
    // The subcommand's `--db`/`--socket` arrive from argv and may be
    // relative; anchor them before anything derives a parent from them.
    let db_path = &absolutize(db_path);
    let socket_path = &absolutize(socket_path);
    let Some(_guard) = try_acquire_events_daemon_guard(socket_path) else {
        tracing::info!(
            socket = %socket_path.display(),
            "another events daemon holds the lock; exiting"
        );
        return Ok(());
    };
    ensure_events_db_owner_only(db_path)?;
    let backend = Arc::new(StorageBackend::sqlite(db_path)?);
    // Ensure the schema once, loudly, before accepting traffic.
    backend.events()?;
    // The `-wal`/`-shm` sidecars inherit the database file's mode at
    // creation; tighten any that already exist from before the hardening.
    harden_events_db_sidecars(db_path)?;

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
    // Per-namespace store cache: `events_for_namespace` takes a writer-lane
    // checkout and re-runs the schema DDL on every call, so paying it once
    // per namespace instead of once per request keeps the writer lane for
    // actual writes. The namespace is client-supplied, so the cache is
    // bounded (`MAX_CACHED_NAMESPACE_STORES`) rather than trusted small.
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_EVENTS_CONNECTIONS));
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
        let permit = match Arc::clone(&connections).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // At the cap: drop the stream (peer sees EOF and retries via
                // its own reconnect path) instead of queueing unbounded work.
                tracing::warn!(
                    cap = MAX_EVENTS_CONNECTIONS,
                    "events daemon at connection cap; dropping new connection"
                );
                continue;
            }
        };
        let backend = Arc::clone(&backend);
        let stores = Arc::clone(&stores);
        crate::daemon::spawn_tracked_task(async move {
            serve_events_conn(stream, backend, stores).await;
            drop(permit);
        });
    }
}

#[cfg(unix)]
type NamespaceStores =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<dyn EventStore>>>>;

/// The cached per-namespace store, constructing (and caching) it on first
/// use. Construction failures are not cached — the next request retries.
///
/// The cache key is the trimmed namespace, matching the backend's own
/// normalization, so spellings that resolve to one store share one handle.
/// The map is bounded: at `MAX_CACHED_NAMESPACE_STORES` entries a new
/// namespace gets an uncached per-request store instead of growing the map —
/// the namespace string is client-supplied, and an unbounded cache would be
/// attacker-controlled memory in a process that lives for months.
#[cfg(unix)]
fn namespace_store(
    backend: &StorageBackend,
    stores: &NamespaceStores,
    namespace: &str,
) -> Result<Arc<dyn EventStore>, khive_db::SqliteError> {
    namespace_store_with_cap(backend, stores, namespace, MAX_CACHED_NAMESPACE_STORES)
}

#[cfg(unix)]
fn namespace_store_with_cap(
    backend: &StorageBackend,
    stores: &NamespaceStores,
    namespace: &str,
    cap: usize,
) -> Result<Arc<dyn EventStore>, khive_db::SqliteError> {
    let key = namespace.trim();
    if let Some(store) = stores
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(key)
    {
        return Ok(Arc::clone(store));
    }
    let store = backend.events_for_namespace(namespace)?;
    let mut map = stores
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map.len() < cap || map.contains_key(key) {
        map.insert(key.to_string(), Arc::clone(&store));
    }
    Ok(store)
}

#[cfg(unix)]
async fn serve_events_conn(
    mut stream: UnixStream,
    backend: Arc<StorageBackend>,
    stores: NamespaceStores,
) {
    loop {
        // Deadline on the whole frame read: a peer that opens a connection
        // and stalls — before or mid-frame — is closed instead of holding
        // this task and its descriptor indefinitely.
        let payload = match tokio::time::timeout(CONN_IO_TIMEOUT, read_frame(&mut stream)).await {
            Ok(Ok(bytes)) => bytes,
            // Includes clean EOF on peer disconnect, and the expired deadline.
            Ok(Err(_)) | Err(_) => return,
        };
        let response = match serde_json::from_slice::<EventsRequest>(&payload) {
            Ok(request) => dispatch_events_request(request, &backend, &stores).await,
            Err(error) => EventsResponse::Error {
                message: format!("events daemon could not parse request frame: {error}"),
                retryable: false,
                side_effects_unknown: false,
                writer_task_state: None,
            },
        };
        let bytes = match serde_json::to_vec(&response) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::error!(error = %error, "events daemon response serialization failed");
                return;
            }
        };
        // Same deadline on the response write: a peer that stops reading
        // would otherwise park this task in a full socket buffer.
        match tokio::time::timeout(CONN_IO_TIMEOUT, write_frame(&mut stream, &bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return,
        }
    }
}

#[cfg(unix)]
async fn dispatch_events_request(
    request: EventsRequest,
    backend: &StorageBackend,
    stores: &NamespaceStores,
) -> EventsResponse {
    if request.protocol_version() != EVENTS_PROTOCOL_VERSION {
        return EventsResponse::Error {
            message: format!(
                "events protocol version mismatch: daemon speaks {}, client sent {}",
                EVENTS_PROTOCOL_VERSION,
                request.protocol_version()
            ),
            retryable: false,
            side_effects_unknown: false,
            writer_task_state: None,
        };
    }
    let store = match namespace_store(backend, stores, request.namespace()) {
        Ok(store) => store,
        Err(error) => {
            return EventsResponse::Error {
                message: format!("events store unavailable: {error}"),
                retryable: true,
                side_effects_unknown: false,
                writer_task_state: None,
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
    // Writer-task terminations carry their request state verbatim: the
    // ADR-133 retry classifier decides per-variant (`NotStarted` and
    // `TransactionRolledBack` are mandated retries), so the state must
    // survive the socket even though `is_retryable()` reports the family
    // non-transient.
    let writer_task_state = match error {
        StorageError::WriterTaskTerminated { request_state } => {
            Some(WireWriterTaskState::from(*request_state))
        }
        _ => None,
    };
    EventsResponse::Error {
        message: error.to_string(),
        // Defer to the storage layer's own transience classifier rather than
        // re-enumerating variants here: a hand-rolled subset silently turns
        // transient writer contention (`WriterTaskBusy`, `Transaction`) into
        // a terminal error on the client side of the socket.
        retryable: error.is_retryable(),
        side_effects_unknown: matches!(
            writer_task_state,
            Some(WireWriterTaskState::SideEffectsUnknown)
        ),
        writer_task_state,
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
        Self::new_with_queue_depth_and_delivery_timeout(socket_path, queue_depth, REQUEST_TIMEOUT)
    }

    /// [`Self::new_with_queue_depth`] with an explicit per-batch delivery
    /// timeout, so tests can prove the forwarder abandons a hung peer without
    /// waiting out the production clock.
    fn new_with_queue_depth_and_delivery_timeout(
        socket_path: PathBuf,
        queue_depth: usize,
        delivery_timeout: Duration,
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
            delivery_timeout,
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
    delivery_timeout: Duration,
) {
    // The client lives in a process-global registry, so its sender is never
    // dropped and `rx.recv()` alone would keep this tracked task alive
    // through daemon shutdown, forcing `drain()` to its full timeout.
    // Observe the shutdown token directly: queued batches at shutdown are a
    // counted drop, exactly the lane's loss-tolerant contract.
    let shutdown = crate::daemon::daemon_shutdown_token();
    let mut conn: Option<UnixStream> = None;
    loop {
        let (namespace, events) = tokio::select! {
            _ = shutdown.cancelled() => break,
            received = rx.recv() => match received {
                Some(batch) => batch,
                None => break,
            },
        };
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

        // The delivery itself must stay both cancellable and bounded: a
        // connected but non-responding daemon would otherwise park this
        // tracked task inside write_frame/read_frame, beyond the reach of the
        // recv-side shutdown select — and daemon drain would wait its full
        // timeout on it. Shutdown mid-delivery abandons the batch (the
        // lane's loss-tolerant contract); a timeout poisons the connection,
        // since the peer may answer the abandoned frame later.
        let delivered = tokio::select! {
            _ = shutdown.cancelled() => break,
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
                _ = shutdown.cancelled() => break,
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
            EventsResponse::Error {
                message,
                retryable,
                side_effects_unknown,
                writer_task_state,
            } => {
                if let Some(state) = writer_task_state {
                    // Reconstruct the exact writer-task variant: the ADR-133
                    // retry classifier decides per-state (`NotStarted` and
                    // `TransactionRolledBack` are mandated retries,
                    // `SideEffectsUnknown` gates double-send decisions), so
                    // flattening any of them into a generic refusal breaks
                    // the retry contract on the client side of the socket.
                    StorageError::WriterTaskTerminated {
                        request_state: state.into(),
                    }
                } else if side_effects_unknown {
                    // Frame from a daemon one protocol build older, which
                    // spelled only this one state.
                    StorageError::WriterTaskTerminated {
                        request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                    }
                } else if retryable {
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
    /// Bound on `offset + limit` for a merged `query_events` window. Offset
    /// pagination over two stores materializes the whole prefix in memory
    /// (see `query_events`), so an unbounded offset would let a single
    /// request buffer both stores wholesale. The bound comfortably admits
    /// the largest legitimate bounded window in the tree
    /// (`brain.event_counts`' 50k page); deep walks page with a `before`
    /// cursor at `offset: 0`, which never grows the materialized prefix.
    pub const MAX_MERGED_WINDOW_ROWS: u64 = 100_000;

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
        // Cost is O(offset + limit) rows materialized from each store — the
        // floor for offset semantics over two sources, since the split point
        // is unknowable without both prefixes. The sort below is a single
        // merge pass in practice (std's stable sort is adaptive on the
        // concatenation of two sorted runs). That materialization is why the
        // window is bounded below: without a bound, one authenticated
        // request with a pathological offset would make both stores buffer
        // and sort every matching row. Deep walks page with a `before`
        // cursor (strict `created_at <` bound in `EventFilter`) at
        // `offset: 0`, which stays inside the bound at any depth.
        let window = page.offset.saturating_add(u64::from(page.limit));
        if window > Self::MAX_MERGED_WINDOW_ROWS {
            return Err(StorageError::InvalidInput {
                capability: khive_storage::StorageCapability::Events,
                operation: "query_events".into(),
                message: format!(
                    "offset+limit ({window}) exceeds the merged event plane's window bound \
                     of {}; page deep windows with a `before` cursor at offset 0, or narrow \
                     the filter",
                    Self::MAX_MERGED_WINDOW_ROWS
                ),
            });
        }
        let prefix = PageRequest {
            offset: 0,
            limit: window.min(u64::from(u32::MAX)) as u32,
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
        // A retried batch may have landed — fully or partially — on the
        // legacy store before the split cutover, and merged reads stay
        // duplicate-free only while an id lives in exactly one store. Probe
        // the legacy store for the batch's ids and route resident rows back
        // through the legacy store's own idempotent machinery, which compares
        // every persisted column without re-inserting; only genuinely new
        // rows reach the lane. Without the probe, a legacy-resident id would
        // be inserted a second time into the lane and every merged query and
        // count would double-count it.
        if events.is_empty() {
            return self.lane.append_events_idempotent(events).await;
        }
        let ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
        let probe_limit = u32::try_from(ids.len()).map_err(|_| StorageError::InvalidInput {
            capability: khive_storage::StorageCapability::Events,
            operation: "append_events_idempotent".into(),
            message: format!(
                "batch of {} rows exceeds the legacy probe window",
                ids.len()
            ),
        })?;
        let existing = self
            .legacy
            .query_events(
                EventFilter {
                    ids,
                    ..EventFilter::default()
                },
                PageRequest {
                    offset: 0,
                    limit: probe_limit,
                },
            )
            .await?;
        let legacy_resident: std::collections::HashSet<Uuid> =
            existing.items.iter().map(|event| event.id).collect();
        if legacy_resident.is_empty() {
            return self.lane.append_events_idempotent(events).await;
        }
        let mut legacy_rows = Vec::new();
        let mut lane_rows = Vec::new();
        let mut routed_to_legacy = Vec::with_capacity(events.len());
        for event in events {
            if legacy_resident.contains(&event.id) {
                routed_to_legacy.push(true);
                legacy_rows.push(event);
            } else {
                routed_to_legacy.push(false);
                lane_rows.push(event);
            }
        }
        let legacy_expected = legacy_rows.len();
        let lane_expected = lane_rows.len();
        let legacy_result = self.legacy.append_events_idempotent(legacy_rows).await?;
        let lane_result = if lane_expected == 0 {
            IdempotentEventBatchResult { rows: Vec::new() }
        } else {
            self.lane.append_events_idempotent(lane_rows).await?
        };
        if legacy_result.rows.len() != legacy_expected || lane_result.rows.len() != lane_expected {
            return Err(StorageError::Driver {
                capability: khive_storage::StorageCapability::Events,
                operation: "append_events_idempotent".into(),
                source: format!(
                    "idempotent sub-batch result length mismatch: legacy {}/{}, lane {}/{}",
                    legacy_result.rows.len(),
                    legacy_expected,
                    lane_result.rows.len(),
                    lane_expected
                )
                .into(),
            });
        }
        let mut legacy_iter = legacy_result.rows.into_iter();
        let mut lane_iter = lane_result.rows.into_iter();
        let rows = routed_to_legacy
            .into_iter()
            .map(|to_legacy| {
                if to_legacy {
                    legacy_iter.next().expect("length checked above")
                } else {
                    lane_iter.next().expect("length checked above")
                }
            })
            .collect();
        Ok(IdempotentEventBatchResult { rows })
    }

    fn supports_idempotent_audit_batch(&self) -> bool {
        // The legacy-resident probe above routes retried rows through the
        // legacy store's idempotent path, so the split supports the audit
        // batch only when BOTH stores do.
        self.lane.supports_idempotent_audit_batch() && self.legacy.supports_idempotent_audit_batch()
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

    #[test]
    fn bare_relative_main_db_yields_an_absolute_sidecar() {
        // A bare file name has `Some("")` for a parent; every downstream
        // consumer (lock parenting, socket-dir trust validation) needs a
        // real directory, so the sidecar path must come back anchored.
        let path = events_db_path_beside(Path::new("khive.db"));
        assert!(path.is_absolute(), "got relative {path:?}");
        assert!(path.ends_with("khive.db.events.db"), "got {path:?}");
    }

    #[cfg(unix)]
    #[test]
    fn side_effects_unknown_crosses_the_wire() {
        let response = storage_error_response(&StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        });
        assert!(
            matches!(
                response,
                EventsResponse::Error {
                    side_effects_unknown: true,
                    ..
                }
            ),
            "an unknown-commit-state termination must be marked on the wire"
        );
        // Any other refusal must NOT carry the marker.
        let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
        assert!(matches!(
            busy,
            EventsResponse::Error {
                side_effects_unknown: false,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn client_reconstructs_side_effects_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let client =
            EventsSplitClient::new(dir.path().join("never-bound.sock")).expect("client builds");
        let store = ForwardingEventStore::new("test", client);
        // Older-daemon frame shape: only the single-state marker, no
        // `writer_task_state` field. The client must still reconstruct it.
        let err = store.unexpected(
            "append_events_idempotent",
            EventsResponse::Error {
                message: "writer terminated mid-flush".into(),
                retryable: false,
                side_effects_unknown: true,
                writer_task_state: None,
            },
        );
        assert!(
            matches!(
                err,
                StorageError::WriterTaskTerminated {
                    request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn error_frames_from_an_older_daemon_still_parse() {
        // A daemon built before `side_effects_unknown` emits Error frames
        // without the field; the client must read them as `false`, not fail.
        let bytes = br#"{"kind":"error","message":"boom","retryable":true}"#;
        let parsed: EventsResponse = serde_json::from_slice(bytes).expect("older frame parses");
        assert!(matches!(
            parsed,
            EventsResponse::Error {
                retryable: true,
                side_effects_unknown: false,
                ..
            }
        ));
    }

    fn split_retry_event(verb: &str) -> Event {
        Event::new(
            "test",
            verb,
            khive_types::EventKind::RecallExecuted,
            khive_types::SubstrateKind::Note,
            "agent:test",
        )
    }

    fn store_pair(dir: &Path) -> (Arc<dyn EventStore>, Arc<dyn EventStore>) {
        let legacy = direct_backend_for(&dir.join("legacy.db"))
            .expect("legacy backend")
            .events_for_namespace("test")
            .expect("legacy store");
        let lane = direct_backend_for(&dir.join("lane.db"))
            .expect("lane backend")
            .events_for_namespace("test")
            .expect("lane store");
        (legacy, lane)
    }

    /// A retried audit batch whose first attempt landed on the legacy store
    /// before the cutover must be answered by the legacy store's comparison,
    /// never re-inserted into the lane — otherwise merged reads and counts
    /// double-count the id.
    #[tokio::test]
    async fn idempotent_retry_of_legacy_resident_rows_does_not_duplicate() {
        use khive_storage::event::EventAppendDisposition;

        let dir = tempfile::tempdir().unwrap();
        let (legacy, lane) = store_pair(dir.path());

        let resident = split_retry_event("recall");
        legacy
            .append_events_idempotent(vec![resident.clone()])
            .await
            .expect("pre-cutover landing");

        let split = SplitEventStore::new(Arc::clone(&legacy), Arc::clone(&lane));
        let fresh = split_retry_event("search");
        let result = split
            .append_events_idempotent(vec![resident.clone(), fresh.clone()])
            .await
            .expect("mixed retry batch");
        assert_eq!(
            result.rows,
            vec![
                EventAppendDisposition::AlreadyPresentIdentical,
                EventAppendDisposition::Inserted,
            ],
            "input order must be preserved across the two sub-batches"
        );
        assert_eq!(
            lane.count_events(EventFilter::default()).await.unwrap(),
            1,
            "the legacy-resident row must not reach the lane"
        );
        assert_eq!(
            split.count_events(EventFilter::default()).await.unwrap(),
            2,
            "merged count must not double-count the retried id"
        );

        // A conflicting retry of the resident row reports the conflict from
        // the legacy comparison and inserts nowhere.
        let mut mutated = resident.clone();
        mutated.verb = "other".to_string();
        let conflict = split
            .append_events_idempotent(vec![mutated])
            .await
            .expect("conflicting retry");
        assert_eq!(
            conflict.rows,
            vec![EventAppendDisposition::IdentityConflict]
        );
        assert_eq!(split.count_events(EventFilter::default()).await.unwrap(), 2);
    }

    /// The merged plane refuses to materialize an unbounded offset prefix;
    /// the refusal names the cursor remedy, and the bound itself is usable.
    #[tokio::test]
    async fn merged_offset_window_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let (legacy, lane) = store_pair(dir.path());
        let split = SplitEventStore::new(legacy, lane);

        let err = split
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: SplitEventStore::MAX_MERGED_WINDOW_ROWS,
                    limit: 1,
                },
            )
            .await
            .expect_err("a window past the bound must be refused");
        assert!(
            matches!(err, StorageError::InvalidInput { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("before"),
            "the refusal must name the cursor remedy: {err}"
        );

        let at_bound = split
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: SplitEventStore::MAX_MERGED_WINDOW_ROWS - 1,
                    limit: 1,
                },
            )
            .await
            .expect("a window at the bound is admitted");
        assert!(at_bound.items.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn writer_task_states_cross_the_wire_verbatim() {
        // The ADR-133 retry classifier decides per-state, so every
        // WriterTaskTerminated state must survive server → wire → client
        // reconstruction exactly. Before this field existed, NotStarted and
        // TransactionRolledBack crossed as a generic non-retryable refusal
        // and came back as terminal InvalidInput — a broken retry contract.
        use khive_storage::WriterTaskRequestState as S;
        let dir = tempfile::tempdir().unwrap();
        let client =
            EventsSplitClient::new(dir.path().join("never-bound.sock")).expect("client builds");
        let store = ForwardingEventStore::new("test", client);
        for state in [
            S::NotStarted,
            S::TransactionRolledBack,
            S::SideEffectsUnknown,
        ] {
            let response = storage_error_response(&StorageError::WriterTaskTerminated {
                request_state: state,
            });
            // Round-trip through serde like the socket does.
            let bytes = serde_json::to_vec(&response).unwrap();
            let parsed: EventsResponse = serde_json::from_slice(&bytes).unwrap();
            let err = store.unexpected("append_events_idempotent", parsed);
            assert!(
                matches!(
                    err,
                    StorageError::WriterTaskTerminated { request_state } if request_state == state
                ),
                "state {state:?} did not survive the socket: got {err:?}"
            );
        }
        // Control: a non-writer-task refusal must NOT carry the field.
        let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
        assert!(matches!(
            busy,
            EventsResponse::Error {
                writer_task_state: None,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn direct_backend_hardens_preexisting_db_and_sidecars() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("pre-existing.events.db");
        let wal = dir.path().join("pre-existing.events.db-wal");
        std::fs::write(&db, b"").unwrap();
        std::fs::write(&wal, b"").unwrap();
        for path in [&db, &wal] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        // Control: the loose mode is really in place before the open.
        assert_eq!(
            std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o644
        );
        direct_backend_for(&db).expect("writable open succeeds");
        for path in [&db, &wal] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600,
                "pre-existing {} must be tightened to owner-only",
                path.display()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn namespace_store_cache_is_bounded_and_trim_normalized() {
        let backend = StorageBackend::memory().unwrap();
        let stores: NamespaceStores =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        // cap=1: the first namespace caches, the second still gets a working
        // store but must not grow the map.
        namespace_store_with_cap(&backend, &stores, "alpha", 1).expect("first store");
        namespace_store_with_cap(&backend, &stores, "beta", 1).expect("uncached store still works");
        let len = stores.lock().unwrap().len();
        assert_eq!(len, 1, "cache must not grow past its cap");
        // Trimmed spelling of a cached namespace hits the same entry rather
        // than minting a duplicate handle (the backend trims namespaces).
        namespace_store_with_cap(&backend, &stores, "  alpha  ", 1).expect("trimmed spelling");
        let len = stores.lock().unwrap().len();
        assert_eq!(len, 1, "spellings of one namespace must share one entry");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forwarder_abandons_hung_delivery() {
        // A daemon that accepts the connection and then never responds must
        // not park the forwarder forever: the delivery deadline fires, the
        // batch is dropped and counted, and the task stays live.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hung.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // Accept and hold connections open without ever reading or replying.
        let _server = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((stream, _)) = listener.accept().await {
                    held.push(stream);
                }
            }
        });
        let client = EventsSplitClient::new_with_queue_depth_and_delivery_timeout(
            socket.clone(),
            4,
            Duration::from_millis(100),
        )
        .expect("client builds");
        let event = Event::new(
            "test",
            "noop",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "tester",
        );
        client.enqueue("test", vec![event]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if client.metrics().dropped_batches >= 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "forwarder never abandoned the hung delivery: {:?}",
                client.metrics()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
