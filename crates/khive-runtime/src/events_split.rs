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
///
/// Version 2 was the first version any release shipped: version 1 carried a
/// `side_effects_unknown` error field that was replaced by the
/// `writer_task_state` carrier before this module reached any released ref,
/// so v1 speakers existed only on unreleased development heads. The bump
/// exists so that even such a process gets the version refusal above rather
/// than having its retryable writer states silently mapped to terminal
/// `InvalidInput`. Version 3 replaces that state-only carrier with a failure
/// disposition so a proven rollback can cross the socket without falsely
/// claiming the remote writer task terminated.
pub const EVENTS_PROTOCOL_VERSION: u32 = 3;

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

/// Aggregate cap on request-frame bytes buffered across all served
/// connections at once. The per-frame cap bounds one buffer and the
/// connection cap bounds the task count, but their product is the real
/// allocation exposure (128 connections × 8 MiB declared frames ≈ 1 GiB).
/// Body buffers are admitted against this shared byte budget before they are
/// allocated; a connection whose frame cannot be admitted waits inside its
/// own I/O deadline, so exhaustion degrades into per-connection timeouts,
/// never into daemon memory growth. Must be at least `MAX_FRAME_BYTES`, or a
/// maximum-size frame could never be admitted.
#[cfg(unix)]
const MAX_INFLIGHT_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Cap on the per-namespace store cache. Stores are cheap pool handles, but
/// the namespace string is client-supplied, so an unbounded map is
/// attacker-controlled memory growth. Beyond the cap, stores are built
/// per-request instead of cached — slower, never wrong.
#[cfg(unix)]
const MAX_CACHED_NAMESPACE_STORES: usize = 1024;

/// Symlink-chain hop ceiling for events-path resolution. Matches the bound
/// the daemon's socket-path traversal guard enforces, which is itself the
/// kernel's total-links ceiling on Linux (40): both resolvers must agree,
/// or a chain one accepts derives a different identity than the other.
const EVENTS_SYMLINK_HOP_BOUND: u32 = 40;

/// Cap on `QueryEvents` page size, in rows. The wire `PageRequest.limit` is
/// client-supplied `u32`, and the daemon materializes the full page as a
/// `Vec<Event>` and serializes it into one response frame — so an unbounded
/// limit is attacker-controlled memory and serialization work in a process
/// that lives for months. Over-cap requests get a typed refusal naming the
/// cap, never a silently clamped page: a caller that asked for more rows
/// than it got would otherwise read the short page as the end of the data.
/// The split client's merged read requests a prefix of `offset + limit`
/// rows, so this cap also bounds the deep-offset window a socket client can
/// demand in one request.
///
/// Public (and defined on every platform) because in-tree consumers that
/// read events through a possibly-split store must size their page requests
/// under it — a deep read is a `before`-cursor walk at `offset: 0` in pages
/// of at most this many rows, never one wide page.
pub const MAX_QUERY_EVENTS_PAGE_ROWS: u32 = 4096;

/// Default events database file, beside the main database file.
///
/// The name is derived from the main database's full file name (`khive.db` →
/// `khive.db.events.db`), never from its stem and never a fixed name in the
/// parent directory: two independent databases that happen to share a
/// directory (`a.db`, `b.db`) — or a stem (`a.db`, `a.sqlite`) — must each
/// get their own event plane, not silently share one. The whole path is
/// canonicalized when the database file exists — final-component symlink
/// aliases of one database must derive the same sidecar and socket as the
/// target spelling, because backend identity already treats those aliases as
/// one database. When the file does not exist yet, the parent directory alone
/// is canonicalized when it resolves, so path aliases of a fresh database
/// (relative spellings, symlinked directories) still map to one sidecar
/// instead of minting a distinct events database per spelling.
pub fn events_db_path_beside(main_db: &Path) -> PathBuf {
    // When the database file does not exist yet, the path can still be a
    // symlink — a dangling alias whose target the first open will create.
    // `canonicalize` refuses dangling links, so resolve final-component
    // links by hand on that arm: alias-first and target-first cold starts
    // must derive the same sidecar, or the first process to open each
    // spelling writes audit records the other never reads.
    let resolved = std::fs::canonicalize(main_db)
        .unwrap_or_else(|_| resolve_dangling_final_component(main_db));
    let mut name = resolved
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| std::ffi::OsString::from("khive.db"));
    name.push(".events.db");
    let path = match resolved.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        Some(dir) => std::fs::canonicalize(dir)
            .unwrap_or_else(|_| dir.to_path_buf())
            .join(&name),
        None => PathBuf::from(name),
    };
    absolutize(&path)
}

/// Follow final-component symlinks whose eventual target need not exist.
/// The identity being recovered is the target's NAME — the file the first
/// open through the alias will actually create — so a chain of links is
/// walked (relative targets anchored at each link's parent) up to the same
/// bound kernels use; a cycle or over-long chain stops at the last spelling,
/// which `open()` will refuse anyway.
fn resolve_dangling_final_component(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    // Same hop bound as the daemon's socket-path traversal guard (and the
    // kernel's own total-links ceiling on Linux, 40): a chain the kernel
    // would resolve must derive the target's sidecar, or a 33-hop alias
    // opens fine yet splits its audit records from the target spelling.
    for _ in 0..EVENTS_SYMLINK_HOP_BOUND {
        match std::fs::read_link(&current) {
            Ok(target) => {
                current = if target.is_absolute() {
                    target
                } else {
                    match current.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                        Some(dir) => dir.join(target),
                        None => target,
                    }
                };
            }
            Err(_) => break,
        }
    }
    current
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
    refuse_events_db_symlinks(db_path)
        .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
    #[cfg(unix)]
    if !read_only {
        if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    // Directory trust comes after creation (a fresh parent in a trusted
    // ancestor must be statable) and before any open: SQLite's open is
    // path-based, so the only defense against a component swapped between
    // validation and open is that no untrusted local user can write the
    // directories the path traverses.
    #[cfg(unix)]
    ensure_events_db_parent_trusted(db_path)
        .map_err(|e| crate::error::RuntimeError::Internal(e.to_string()))?;
    // Embedded writable mode holds the events database to the daemon's own
    // contract: owner-only from the first byte, never at the process umask,
    // and — because event rows carry the same audit payloads either way — a
    // pre-existing database or -wal/-shm sidecar is tightened to 0600 too,
    // fail-closed. A create race just means SQLite finds the file present.
    #[cfg(unix)]
    if !read_only {
        use std::os::unix::fs::OpenOptionsExt;
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
    /// `writer_task_failure` carries both request finality and whether the
    /// remote writer seam terminated. The retryable bit cannot express that
    /// distinction: an ordinary operation or COMMIT error may be transient or
    /// permanent while its transaction is independently proven rolled back.
    /// `None` means the error is not from a writer request. Cross-version
    /// frames never reach this mapping because `dispatch_events_request`
    /// refuses a mismatched version before any write executes.
    Error {
        message: String,
        retryable: bool,
        #[serde(default)]
        writer_task_failure: Option<WireWriterTaskFailure>,
    },
}

/// Whether a writer-state error belongs to one failed request on a still-live
/// seam or to a permanently retired writer task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireWriterTaskFailure {
    RequestFailed { request_state: WireWriterTaskState },
    TaskTerminated { request_state: WireWriterTaskState },
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

/// Try to become the events daemon for `socket_path`. `None` = the lock
/// could not be safely acquired — either another events daemon holds it, or
/// a hardening step refused (symlinked lock entry, failed chmod). Both mean
/// the caller must not serve; callers that need the socket directory
/// validated must run `ensure_socket_dir_is_trusted` on the parent BEFORE
/// calling this, so no lock-path operation happens in an untrusted
/// directory.
#[cfg(unix)]
pub fn try_acquire_events_daemon_guard(socket_path: &Path) -> Option<EventsDaemonGuard> {
    let lock_path = socket_path.with_extension("lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    // `O_NOFOLLOW` pins the open to the final component: a symlink planted
    // at the lock name is refused instead of redirecting the open (and the
    // chmod below) to an attacker-selected target.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .ok()?;
    // `mode` applies only at creation; tighten a pre-existing lock file too.
    // Descriptor-based (`fchmod` on the handle just opened), never a second
    // path lookup — and fail closed: with the inode pinned, a failed chmod
    // is abnormal, and serving behind a lock file another user can open is
    // exactly what the hardening exists to refuse.
    file.set_permissions(
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
    )
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

/// Refuse to serve an events database whose path — or whose `-wal`/`-shm`
/// sidecar path — is a pre-existing symlink. These paths are derived, never
/// user-chosen (`events_db_path_beside` canonicalizes the main database
/// spelling first), so a link here is a planted redirect, not an alias:
/// permission hardening and SQLite would otherwise follow it and tighten or
/// write event rows through to whatever file the link's author chose
/// (CWE-59). Runs before any open on both the embedded and daemon arms; a
/// link planted after admission is bounded by the daemon's trusted-directory
/// contract on the socket parent.
fn refuse_events_db_symlinks(db_path: &Path) -> anyhow::Result<()> {
    let mut targets = vec![db_path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        targets.push(PathBuf::from(name));
    }
    for path in targets {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => anyhow::bail!(
                "refusing to serve events: {} is a symlink; the events database and its \
                 sidecars must be regular files",
                path.display()
            ),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!(
                "refusing to serve events: cannot inspect {}: {e}",
                path.display()
            ),
        }
    }
    Ok(())
}

/// Require the directory holding the events database to be trusted before
/// anything opens a path inside it. The symlink pre-checks and the fd-pinned
/// chmod close the races they can see, but SQLite's own open is path-based
/// and cannot be inode-pinned from here — so the remaining defense is the
/// same one the daemon socket uses: no untrusted local user may be able to
/// write (or swap components of) the directory the path traverses. Delegates
/// to the socket-path walk, which validates every ancestor the kernel will
/// visit, with the same ownership and sticky-bit rules.
#[cfg(unix)]
fn ensure_events_db_parent_trusted(db_path: &Path) -> anyhow::Result<()> {
    let parent = absolutize(db_path);
    let parent = parent.parent().filter(|p| !p.as_os_str().is_empty());
    match parent {
        Some(dir) => crate::daemon::ensure_socket_dir_is_trusted(dir).map_err(|e| {
            anyhow::anyhow!("refusing to serve events from an untrusted directory: {e}")
        }),
        None => Ok(()),
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
    refuse_events_db_symlinks(db_path)?;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // After creation so a fresh parent can be validated; a directory this
    // process just created in a trusted ancestor passes by construction.
    ensure_events_db_parent_trusted(db_path)?;
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
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut targets = vec![db_path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_os_string();
        name.push(suffix);
        targets.push(PathBuf::from(name));
    }
    for path in targets {
        // Pin the inode before touching it: `O_NOFOLLOW` makes the open
        // itself refuse a symlink at the final component, and the chmod is
        // then issued on the returned handle (fchmod), so no path re-lookup
        // exists between validation and the permission change for a swapped
        // entry to exploit. A path-based lstat-then-chmod pair here would
        // re-open the exact race it is defending against.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                anyhow::bail!(
                    "refusing to serve events: cannot open {} without following symlinks: \
                     {e}. The events database and its sidecars must be regular files.",
                    path.display()
                )
            }
        };
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| {
                anyhow::anyhow!(
                    "refusing to serve events: cannot chmod 0600 {}: {e}. The events \
                     database and its sidecars must be owner-only.",
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
    // Directory trust comes FIRST: the lock guard below opens and chmods a
    // path in this directory, and validating only before the later bind
    // would let those operations run in a directory another user controls.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
        crate::daemon::ensure_socket_dir_is_trusted(parent)?;
    }
    let Some(_guard) = try_acquire_events_daemon_guard(socket_path) else {
        tracing::info!(
            socket = %socket_path.display(),
            "events daemon lock unavailable (held by another daemon, or hardening refused); exiting"
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
    let frame_budget = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_REQUEST_BYTES));
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
        let frame_budget = Arc::clone(&frame_budget);
        crate::daemon::spawn_tracked_task(async move {
            serve_events_conn(stream, backend, stores, frame_budget).await;
            drop(permit);
        });
    }
}

/// Read one length-prefixed request frame, admitting the body buffer against
/// the shared byte budget before allocating it. The returned permit holds
/// the admitted bytes for as long as the buffer may be alive — the caller
/// drops it after the response is written. The per-frame cap is checked
/// before admission, so a single frame is always satisfiable against the
/// full budget (`MAX_INFLIGHT_REQUEST_BYTES >= MAX_FRAME_BYTES`) and a
/// waiter can never deadlock on an impossible request.
#[cfg(unix)]
async fn read_frame_budgeted(
    stream: &mut UnixStream,
    budget: &Arc<tokio::sync::Semaphore>,
) -> std::io::Result<(Vec<u8>, tokio::sync::OwnedSemaphorePermit)> {
    use tokio::io::AsyncReadExt;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > crate::daemon::MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "daemon frame of {len} bytes exceeds {} cap",
                crate::daemon::MAX_FRAME_BYTES
            ),
        ));
    }
    let permit = Arc::clone(budget)
        .acquire_many_owned(len as u32)
        .await
        .map_err(|_| std::io::Error::other("frame budget closed"))?;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok((buf, permit))
}

#[cfg(unix)]
type NamespaceStores =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<dyn EventStore>>>>;

/// The cached per-namespace store, constructing (and caching) it on first
/// use. Construction failures are not cached — the next request retries.
///
/// The cache key is the trimmed namespace, matching the backend's own
/// normalization, so spellings that resolve to one store share one handle.
/// The map is bounded: at `MAX_CACHED_NAMESPACE_STORES` entries an arbitrary
/// existing entry is evicted to admit the new namespace, so the map never
/// grows past the cap and no namespace is permanently condemned to
/// per-request store rebuilds. Keys reaching this cache have already passed
/// `Namespace` validation at dispatch (charset + length bound), so cap ×
/// bounded key is the worst-case retained memory.
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
    if map.len() >= cap && !map.contains_key(key) {
        // At the cap, evict an arbitrary entry instead of refusing
        // admission: stores are cheap pool handles, so eviction costs one
        // rebuild on the victim's next request, while refusing admission
        // would make every request beyond the cap rebuild its store and
        // rerun schema init on the SQLite writer forever. Keys are
        // validated `Namespace` values (bounded length), so the map's
        // worst-case memory is cap × small key + cap handles.
        if let Some(victim) = map.keys().next().cloned() {
            map.remove(&victim);
        }
    }
    map.insert(key.to_string(), Arc::clone(&store));
    Ok(store)
}

#[cfg(unix)]
async fn serve_events_conn(
    mut stream: UnixStream,
    backend: Arc<StorageBackend>,
    stores: NamespaceStores,
    frame_budget: Arc<tokio::sync::Semaphore>,
) {
    loop {
        // Deadline on the whole frame read — budget admission included: a
        // peer that opens a connection and stalls, or that cannot be
        // admitted because other connections hold the byte budget, is
        // closed instead of holding this task and its descriptor
        // indefinitely.
        let (payload, budget_permit) = match tokio::time::timeout(
            CONN_IO_TIMEOUT,
            read_frame_budgeted(&mut stream, &frame_budget),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            // Includes clean EOF on peer disconnect, and the expired deadline.
            Ok(Err(_)) | Err(_) => return,
        };
        let response = match serde_json::from_slice::<EventsRequest>(&payload) {
            Ok(request) => dispatch_events_request(request, &backend, &stores).await,
            Err(error) => EventsResponse::Error {
                message: format!("events daemon could not parse request frame: {error}"),
                retryable: false,
                writer_task_failure: None,
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
        // The request buffer is long dropped; release its byte budget only
        // now, after the whole request lifecycle, so admission tracks live
        // work rather than just live buffers.
        drop(budget_permit);
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
            writer_task_failure: None,
        };
    }
    // The wire namespace is an unrestricted client-supplied String until this
    // point. Validate it as a real `Namespace` (charset plus the 256-byte
    // length bound) before it can become a cache key, a store, or a database
    // row — without this, near-frame-sized strings are attacker-controlled
    // retained memory in a process that lives for months.
    if let Err(error) = khive_types::Namespace::parse(request.namespace().trim()) {
        return EventsResponse::Error {
            message: format!("events request namespace rejected: {error}"),
            retryable: false,
            writer_task_failure: None,
        };
    }
    let store = match namespace_store(backend, stores, request.namespace()) {
        Ok(store) => store,
        Err(error) => {
            return EventsResponse::Error {
                message: format!("events store unavailable: {error}"),
                retryable: true,
                writer_task_failure: None,
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
            if page.limit > MAX_QUERY_EVENTS_PAGE_ROWS {
                return EventsResponse::Error {
                    message: format!(
                        "events query page limit {} exceeds the daemon cap of {} rows; \
                         request narrower pages",
                        page.limit, MAX_QUERY_EVENTS_PAGE_ROWS
                    ),
                    retryable: false,
                    writer_task_failure: None,
                };
            }
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
    let (message, writer_task_failure) = match error {
        StorageError::WriterTaskRequestFailed {
            request_state,
            source,
        } => (
            source.to_string(),
            Some(WireWriterTaskFailure::RequestFailed {
                request_state: (*request_state).into(),
            }),
        ),
        StorageError::WriterTaskTerminated { request_state } => (
            error.to_string(),
            Some(WireWriterTaskFailure::TaskTerminated {
                request_state: (*request_state).into(),
            }),
        ),
        _ => (error.to_string(), None),
    };
    EventsResponse::Error {
        message,
        // Defer to the storage layer's own transience classifier rather than
        // re-enumerating variants here: a hand-rolled subset silently turns
        // transient writer contention (`WriterTaskBusy`, `Transaction`) into
        // a terminal error on the client side of the socket.
        retryable: error.is_retryable(),
        writer_task_failure,
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
            crate::daemon::daemon_shutdown_token(),
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

/// Drain every batch currently sitting in the fire-and-forget queue,
/// counting each as a dropped batch/events and logging one summary line
/// (never one line per batch — a full queue at shutdown is exactly the
/// bursty case a per-batch log would flood). Non-blocking: `try_recv` only
/// consumes what is already queued, so it terminates as soon as the queue
/// (temporarily) empties even though the sender half is still live.
#[cfg(unix)]
fn drain_dropped_queue(
    rx: &mut tokio::sync::mpsc::Receiver<(String, Vec<Event>)>,
    counters: &ForwardingCounters,
) {
    let mut dropped_batches = 0u64;
    let mut dropped_events = 0u64;
    while let Ok((_, events)) = rx.try_recv() {
        dropped_batches += 1;
        dropped_events += events.len() as u64;
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
async fn run_forwarder(
    socket_path: PathBuf,
    mut rx: tokio::sync::mpsc::Receiver<(String, Vec<Event>)>,
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
        let (namespace, events) = tokio::select! {
            _ = shutdown.cancelled() => {
                drain_dropped_queue(&mut rx, &counters);
                break;
            },
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
                writer_task_failure,
            } => {
                if let Some(failure) = writer_task_failure {
                    match failure {
                        WireWriterTaskFailure::RequestFailed { request_state } => {
                            let source = if retryable {
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
                            };
                            StorageError::WriterTaskRequestFailed {
                                request_state: request_state.into(),
                                source: Box::new(source),
                            }
                        }
                        WireWriterTaskFailure::TaskTerminated { request_state } => {
                            StorageError::WriterTaskTerminated {
                                request_state: request_state.into(),
                            }
                        }
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

    #[cfg(unix)]
    #[test]
    fn final_component_aliases_of_one_database_share_one_sidecar() {
        // Backend identity canonicalizes the whole database path, so a
        // final-component symlink alias is the same database; its sidecar
        // and socket must be the same too, or a process opening one spelling
        // writes event rows a process opening the other never reads.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        std::fs::write(&real, b"").unwrap();
        let alias = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let real_sidecar = events_db_path_beside(&real);
        let alias_sidecar = events_db_path_beside(&alias);
        assert_eq!(
            real_sidecar, alias_sidecar,
            "a symlink alias of one database file must not mint a second event store"
        );
        assert_eq!(
            events_socket_path_beside(&real_sidecar),
            events_socket_path_beside(&alias_sidecar),
        );
        // Control: a genuinely distinct database in the same directory keeps
        // its own sidecar — resolution must not collapse different files.
        let other = dir.path().join("other.db");
        std::fs::write(&other, b"").unwrap();
        assert_ne!(events_db_path_beside(&other), real_sidecar);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_alias_derives_the_target_sidecar_on_cold_start() {
        // Event-path derivation runs before backend creation, so the alias
        // can be consulted while its target does not exist yet — the first
        // open through the alias is what creates the target. A dangling
        // link must therefore already derive the TARGET's sidecar, or an
        // alias-first cold start and a later target-spelled process split
        // the event store between them.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        let alias = dir.path().join("alias.db");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        // Neither real.db nor its sidecar exists at this point.
        assert_eq!(
            events_db_path_beside(&alias),
            events_db_path_beside(&real),
            "a dangling alias must derive the same sidecar its target will use"
        );
        // Chain: a link to a link resolves all the way down.
        let chain = dir.path().join("chain.db");
        std::os::unix::fs::symlink(&alias, &chain).unwrap();
        assert_eq!(events_db_path_beside(&chain), events_db_path_beside(&real));
        // Control: a distinct nonexistent file still gets its own sidecar.
        assert_ne!(
            events_db_path_beside(&dir.path().join("unrelated.db")),
            events_db_path_beside(&real)
        );
    }

    #[cfg(unix)]
    #[test]
    fn planted_symlink_at_the_sidecar_path_is_refused() {
        // The sidecar path is derived, never user-chosen, so a pre-existing
        // symlink there is a planted redirect: following it would tighten
        // permissions on and write event rows into the link's target.
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"victim-bytes").unwrap();
        let mode_before = victim.metadata().unwrap().permissions();
        let sidecar = dir.path().join("khive.db.events.db");
        std::os::unix::fs::symlink(&victim, &sidecar).unwrap();
        let err = match direct_backend_for(&sidecar) {
            Ok(_) => panic!("a planted symlink at the sidecar path must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("symlink"), "got: {err}");
        // The link's target is untouched: content intact, mode not tightened.
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim-bytes");
        assert_eq!(victim.metadata().unwrap().permissions(), mode_before);
        // A dangling link is refused too, and nothing is created at its
        // target — without the refusal, the open itself would mint the
        // redirect target.
        let ghost_target = dir.path().join("ghost.db");
        let dangling = dir.path().join("other.db.events.db");
        std::os::unix::fs::symlink(&ghost_target, &dangling).unwrap();
        assert!(direct_backend_for(&dangling).is_err());
        assert!(
            !ghost_target.exists(),
            "refusal must not create the redirect target"
        );
        // Control: a regular path proceeds and the backend opens.
        let regular = dir.path().join("plain.db.events.db");
        direct_backend_for(&regular).expect("regular sidecar path must open");
        assert!(regular.exists());
    }

    #[cfg(unix)]
    #[test]
    fn deep_symlink_chains_within_the_kernel_bound_derive_the_target_sidecar() {
        // Linux resolves up to 40 links per lookup; a 35-hop dangling chain
        // therefore opens fine, so it must ALSO derive the target's sidecar
        // — a resolver stopping short would split audit records between the
        // alias and target spellings of one database.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.db");
        let mut prev = real.clone();
        for i in 0..35 {
            let link = dir.path().join(format!("hop{i}.db"));
            std::os::unix::fs::symlink(&prev, &link).unwrap();
            prev = link;
        }
        assert_eq!(
            events_db_path_beside(&prev),
            events_db_path_beside(&real),
            "a 35-hop dangling chain must derive the target's sidecar"
        );
        // Control: an unrelated dangling path still derives its own name.
        assert_ne!(
            events_db_path_beside(&dir.path().join("unrelated.db")),
            events_db_path_beside(&real)
        );
    }

    #[cfg(unix)]
    #[test]
    fn untrusted_parent_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        // SQLite's open is path-based: the only sound defense against a
        // component swapped after validation is refusing directories other
        // local users can write. A group/other-writable parent is refused
        // before any open; an owner-only parent proceeds.
        let dir = tempfile::tempdir().unwrap();
        let open_dir = dir.path().join("shared");
        std::fs::create_dir(&open_dir).unwrap();
        std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let refused = direct_backend_for(&open_dir.join("khive.db.events.db"));
        match refused {
            Ok(_) => panic!("a world-writable events directory must be refused"),
            Err(e) => assert!(
                e.to_string().contains("untrusted directory"),
                "refusal must name the directory trust rule: {e}"
            ),
        }
        // Control: an owner-only sibling directory passes the same gate.
        let safe_dir = dir.path().join("owned");
        std::fs::create_dir(&safe_dir).unwrap();
        std::fs::set_permissions(&safe_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        direct_backend_for(&safe_dir.join("khive.db.events.db"))
            .expect("an owner-only events directory must open");
    }

    #[cfg(unix)]
    #[test]
    fn harden_refuses_a_sidecar_symlink_at_use_time() {
        use std::os::unix::fs::PermissionsExt;
        // The hardening step must be coupled to its validation: it opens
        // each target with O_NOFOLLOW and chmods the returned handle, so a
        // symlink present AT USE TIME is refused by the open itself — no
        // lstat-then-chmod window — and the link's target keeps its mode.
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"v").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        let db = dir.path().join("db.events.db");
        std::fs::write(&db, b"").unwrap();
        let mut wal = db.as_os_str().to_os_string();
        wal.push("-wal");
        std::os::unix::fs::symlink(&victim, PathBuf::from(&wal)).unwrap();
        let result = harden_events_db_sidecars(&db);
        assert!(
            result.is_err(),
            "a symlinked -wal must be refused at hardening time"
        );
        let mode = victim.metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644, "the link's target must keep its mode");
    }

    #[cfg(unix)]
    #[test]
    fn lock_symlink_is_refused_and_target_untouched() {
        use std::os::unix::fs::PermissionsExt;
        // The daemon lock open is pinned with O_NOFOLLOW and chmods its own
        // handle: a symlink planted at the lock name must refuse the guard,
        // and the link's target must keep its inode content and mode. A
        // plain lock in the same directory must still acquire — the control
        // that proves the refusal is the symlink, not a broken guard.
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"v").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        let socket = dir.path().join("events.sock");
        std::os::unix::fs::symlink(&victim, socket.with_extension("lock")).unwrap();
        assert!(
            try_acquire_events_daemon_guard(&socket).is_none(),
            "a symlinked lock entry must refuse the guard"
        );
        let mode = victim.metadata().unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644, "the symlink's target must keep its mode");
        assert_eq!(std::fs::read(&victim).unwrap(), b"v");
        let clean = dir.path().join("clean.sock");
        assert!(
            try_acquire_events_daemon_guard(&clean).is_some(),
            "a plain lock path in the same directory must still acquire"
        );
    }

    #[cfg(unix)]
    #[test]
    fn planted_wal_symlink_is_refused_before_open() {
        // SQLite creates `-wal` without O_EXCL, so a planted `-wal` symlink
        // would redirect WAL writes; admission checks the sidecar suffixes
        // before the database is ever created or opened.
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, b"w").unwrap();
        let sidecar = dir.path().join("db.events.db");
        let mut wal = sidecar.as_os_str().to_os_string();
        wal.push("-wal");
        std::os::unix::fs::symlink(&victim, PathBuf::from(&wal)).unwrap();
        assert!(direct_backend_for(&sidecar).is_err());
        assert!(
            !sidecar.exists(),
            "refusal must precede creation of the events database"
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
    #[tokio::test]
    async fn over_cap_query_page_limit_is_refused_before_materialization() {
        // `PageRequest.limit` arrives client-controlled off the wire; the
        // daemon must refuse an over-cap page with a typed, actionable error
        // instead of materializing and serializing it — and must never
        // silently clamp, or the short page reads as end-of-data.
        let backend = Arc::new(StorageBackend::memory().unwrap());
        let stores: NamespaceStores =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let request = |limit: u32| EventsRequest::QueryEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION,
            namespace: "local".to_string(),
            filter: EventFilter::default(),
            page: PageRequest { offset: 0, limit },
        };
        let refused =
            dispatch_events_request(request(MAX_QUERY_EVENTS_PAGE_ROWS + 1), &backend, &stores)
                .await;
        match refused {
            EventsResponse::Error {
                message,
                retryable,
                writer_task_failure,
            } => {
                assert!(
                    message.contains(&MAX_QUERY_EVENTS_PAGE_ROWS.to_string()),
                    "refusal must name the cap: {message}"
                );
                assert!(!retryable, "an over-cap page is not transient");
                assert!(writer_task_failure.is_none());
            }
            other => panic!("over-cap query must be refused, got {other:?}"),
        }
        // Control: a request AT the cap passes admission and reaches the
        // store — an empty page, not a refusal.
        let at_cap =
            dispatch_events_request(request(MAX_QUERY_EVENTS_PAGE_ROWS), &backend, &stores).await;
        assert!(
            matches!(at_cap, EventsResponse::Pageful { .. }),
            "at-cap query must reach the store, got {at_cap:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn side_effects_unknown_state_crosses_the_wire_as_terminal_failure() {
        let response = storage_error_response(&StorageError::WriterTaskTerminated {
            request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
        });
        assert!(
            matches!(
                response,
                EventsResponse::Error {
                    writer_task_failure: Some(WireWriterTaskFailure::TaskTerminated {
                        request_state: WireWriterTaskState::SideEffectsUnknown,
                    }),
                    ..
                }
            ),
            "an unknown-commit-state termination must carry its state on the wire"
        );
        // Any other refusal must NOT carry a writer state.
        let busy = storage_error_response(&StorageError::WriterTaskBusy { timeout_ms: 5 });
        assert!(matches!(
            busy,
            EventsResponse::Error {
                writer_task_failure: None,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn proven_rollback_crosses_the_wire_without_claiming_task_termination() {
        let response = storage_error_response(&StorageError::WriterTaskRequestFailed {
            request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
            source: Box::new(StorageError::Pool {
                operation: "writer_task_commit".into(),
                message: "commit refused".into(),
            }),
        });
        assert!(matches!(
            response,
            EventsResponse::Error {
                writer_task_failure: Some(WireWriterTaskFailure::RequestFailed {
                    request_state: WireWriterTaskState::TransactionRolledBack,
                }),
                ..
            }
        ));
    }

    #[test]
    fn error_frames_without_writer_failure_still_parse() {
        // `writer_task_failure` is `#[serde(default)]`: an Error frame for a
        // non-writer failure (parse error, version refusal) omits it and the
        // client must read `None`, not fail the parse.
        let bytes = br#"{"kind":"error","message":"boom","retryable":true}"#;
        let parsed: EventsResponse = serde_json::from_slice(bytes).expect("stateless frame parses");
        assert!(matches!(
            parsed,
            EventsResponse::Error {
                retryable: true,
                writer_task_failure: None,
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
    async fn stateless_non_retryable_error_maps_terminal_not_writer_terminated() {
        // The terminal arm of the client mapping, pinned deliberately: an
        // Error frame with `retryable: false` and NO `writer_task_failure` is
        // a non-writer failure (parse error, version refusal) and must map
        // to terminal `InvalidInput` — never to `WriterTaskTerminated`
        // (there is no state to reconstruct) and never to the retryable
        // `Pool` arm (retrying a version refusal cannot succeed).
        let client = EventsSplitClient::new(std::path::PathBuf::from("/tmp/never-bound.sock"))
            .expect("client builds");
        let store = ForwardingEventStore::new("test", client);
        let bytes = br#"{"kind":"error","message":"refused","retryable":false}"#;
        let parsed: EventsResponse = serde_json::from_slice(bytes).expect("frame parses");
        assert!(matches!(
            store.unexpected("append", parsed),
            StorageError::InvalidInput { .. }
        ));
        // Control: the same non-retryable frame WITH a writer state must
        // take the reconstruction arm instead — proving the terminal arm
        // above is selected by the field's absence, not by `retryable`.
        let with_state = br#"{"kind":"error","message":"died","retryable":false,"writer_task_failure":{"kind":"task_terminated","request_state":"side_effects_unknown"}}"#;
        let parsed: EventsResponse =
            serde_json::from_slice(with_state).expect("stateful frame parses");
        assert!(matches!(
            store.unexpected("append", parsed),
            StorageError::WriterTaskTerminated { .. }
        ));
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
                writer_task_failure: None,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proven_rollback_round_trips_as_non_terminal_request_failure() {
        use khive_storage::WriterTaskRequestState as S;

        let dir = tempfile::tempdir().unwrap();
        let client =
            EventsSplitClient::new(dir.path().join("never-bound.sock")).expect("client builds");
        let store = ForwardingEventStore::new("test", client);
        let response = storage_error_response(&StorageError::WriterTaskRequestFailed {
            request_state: S::TransactionRolledBack,
            source: Box::new(StorageError::Pool {
                operation: "writer_task_commit".into(),
                message: "commit refused".into(),
            }),
        });
        let bytes = serde_json::to_vec(&response).unwrap();
        let parsed: EventsResponse = serde_json::from_slice(&bytes).unwrap();
        let err = store.unexpected("append_events_idempotent", parsed);
        assert!(
            matches!(
                err,
                StorageError::WriterTaskRequestFailed {
                    request_state: S::TransactionRolledBack,
                    ..
                }
            ),
            "proven rollback must not reconstruct as a terminal writer: {err:?}"
        );
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
        // cap=1: the first namespace caches; the second evicts it and takes
        // the slot — bounded, and never condemned to per-request rebuilds.
        namespace_store_with_cap(&backend, &stores, "alpha", 1).expect("first store");
        namespace_store_with_cap(&backend, &stores, "beta", 1).expect("second store evicts");
        {
            let map = stores.lock().unwrap();
            assert_eq!(map.len(), 1, "cache must not grow past its cap");
            assert!(
                map.contains_key("beta"),
                "the newest namespace must be admitted at the cap"
            );
        }
        // Trimmed spelling of a cached namespace hits the same entry rather
        // than minting a duplicate handle (the backend trims namespaces).
        namespace_store_with_cap(&backend, &stores, "beta", 1).expect("cached beta");
        namespace_store_with_cap(&backend, &stores, "  beta  ", 1).expect("trimmed spelling");
        {
            let map = stores.lock().unwrap();
            assert_eq!(
                map.len(),
                1,
                "spellings of one namespace must share one entry"
            );
            assert!(map.contains_key("beta"), "trimmed key is the cache key");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn frame_budget_admits_before_allocating_and_releases_after() {
        let (mut client, mut server) = UnixStream::pair().expect("socketpair");
        let budget = Arc::new(tokio::sync::Semaphore::new(1024));
        let payload = vec![7u8; 100];
        write_frame(&mut client, &payload).await.expect("write");
        let (bytes, permit) = read_frame_budgeted(&mut server, &budget)
            .await
            .expect("budgeted read");
        assert_eq!(bytes, payload);
        // The permit holds exactly the frame's bytes against the budget…
        assert_eq!(budget.available_permits(), 1024 - 100);
        // …and releases them when the request lifecycle ends.
        drop(permit);
        assert_eq!(budget.available_permits(), 1024);
        // A frame past the per-frame cap is refused before admission: the
        // budget is untouched, so an oversized declaration cannot starve it.
        let mut oversized = Vec::from(((crate::daemon::MAX_FRAME_BYTES + 1) as u32).to_be_bytes());
        oversized.extend_from_slice(&[0u8; 8]);
        use tokio::io::AsyncWriteExt;
        client.write_all(&oversized).await.expect("raw prefix");
        let err = read_frame_budgeted(&mut server, &budget)
            .await
            .expect_err("oversized declaration must refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            budget.available_permits(),
            1024,
            "refusal must not consume budget"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_rejects_invalid_wire_namespace() {
        let backend = Arc::new(StorageBackend::memory().unwrap());
        let stores: NamespaceStores =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        // An attacker-shaped namespace: far past the 256-byte Namespace
        // bound. Must be refused as a typed non-retryable error before it
        // can become a cache key or a store.
        let huge = "n".repeat(64 * 1024);
        let response = dispatch_events_request(
            EventsRequest::CountEvents {
                protocol_version: EVENTS_PROTOCOL_VERSION,
                namespace: huge,
                filter: Default::default(),
            },
            &backend,
            &stores,
        )
        .await;
        assert!(
            matches!(
                &response,
                EventsResponse::Error {
                    retryable: false,
                    writer_task_failure: None,
                    ..
                }
            ),
            "oversized namespace must be a typed refusal, got {response:?}"
        );
        assert_eq!(
            stores.lock().unwrap().len(),
            0,
            "a rejected namespace must never enter the cache"
        );
        // Control: a valid namespace on the same dispatch path succeeds.
        let ok = dispatch_events_request(
            EventsRequest::CountEvents {
                protocol_version: EVENTS_PROTOCOL_VERSION,
                namespace: "local".to_string(),
                filter: Default::default(),
            },
            &backend,
            &stores,
        )
        .await;
        assert!(
            matches!(ok, EventsResponse::Count { .. }),
            "valid namespace must dispatch, got {ok:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_counts_and_logs_queued_and_in_flight_batches_as_dropped() {
        // ADR-170 requires shutdown drops to be visible via counters/logs.
        // Both `run_forwarder` shutdown arms must therefore count every
        // batch it loses: the one parked mid-delivery against a hung peer,
        // and every batch still sitting in the queue behind it. Drives
        // `run_forwarder` directly with a local `CancellationToken` (rather
        // than through `EventsSplitClient`, which wires the process-wide
        // `daemon_shutdown_token()` singleton) so cancelling shutdown here
        // cannot leak into other tests sharing the process.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hung-shutdown.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        // Accept and hold the connection open without ever reading or
        // replying, so the first delivery blocks in `deliver_batch`'s
        // read_frame until shutdown cancels it.
        let _server = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((stream, _)) = listener.accept().await {
                    held.push(stream);
                }
            }
        });

        let (tx, rx) = tokio::sync::mpsc::channel::<(String, Vec<Event>)>(8);
        let counters = Arc::new(ForwardingCounters::default());
        let outage_logged = Arc::new(AtomicBool::new(false));
        let shutdown = tokio_util::sync::CancellationToken::new();

        let forwarder = tokio::spawn(run_forwarder(
            socket,
            rx,
            Arc::clone(&counters),
            outage_logged,
            Duration::from_secs(30),
            shutdown.clone(),
        ));

        fn probe_event(tag: &str) -> Event {
            Event::new(
                "test",
                tag,
                khive_types::EventKind::Audit,
                khive_types::SubstrateKind::Event,
                "tester",
            )
        }

        tx.try_send(("test".to_string(), vec![probe_event("in-flight")]))
            .expect("queue has room for the in-flight batch");
        // No externally observable "delivery started" signal exists short of
        // instrumenting the forwarder; a short sleep reliably lands inside
        // the delivery `select!` arm before shutdown cancels it, given the
        // 30s delivery timeout has no chance to fire first.
        tokio::time::sleep(Duration::from_millis(200)).await;

        tx.try_send((
            "test".to_string(),
            vec![probe_event("queued-1"), probe_event("queued-2")],
        ))
        .expect("queue has room for the first queued batch");
        tx.try_send(("test".to_string(), vec![probe_event("queued-3")]))
            .expect("queue has room for the second queued batch");

        shutdown.cancel();

        tokio::time::timeout(Duration::from_secs(5), forwarder)
            .await
            .expect("forwarder must exit promptly on shutdown")
            .expect("forwarder task must not panic");

        assert_eq!(
            counters.dropped_batches.load(Ordering::Relaxed),
            3,
            "the in-flight batch and both queued batches must all be counted as dropped"
        );
        assert_eq!(
            counters.dropped_events.load(Ordering::Relaxed),
            4,
            "1 in-flight + 2 + 1 queued events must all be counted as dropped"
        );
        assert_eq!(
            counters.forwarded_batches.load(Ordering::Relaxed),
            0,
            "a hung peer never acknowledges anything in this test"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_during_backoff_drains_queued_batches() {
        // A failed delivery sends the forwarder into `FORWARDER_BACKOFF`
        // before it loops back to `rx.recv()`. Shutdown arriving while it
        // sits in that backoff sleep must drain and count whatever is left
        // in the queue, not just break out silently. Points at a socket path
        // with no listener bound so the very first delivery attempt fails
        // fast (connect refused) rather than needing a hung peer.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("no-listener.sock");

        let (tx, rx) = tokio::sync::mpsc::channel::<(String, Vec<Event>)>(8);
        let counters = Arc::new(ForwardingCounters::default());
        let outage_logged = Arc::new(AtomicBool::new(false));
        let shutdown = tokio_util::sync::CancellationToken::new();

        let forwarder = tokio::spawn(run_forwarder(
            socket,
            rx,
            Arc::clone(&counters),
            outage_logged,
            Duration::from_secs(30),
            shutdown.clone(),
        ));

        fn probe_event(tag: &str) -> Event {
            Event::new(
                "test",
                tag,
                khive_types::EventKind::Audit,
                khive_types::SubstrateKind::Event,
                "tester",
            )
        }

        tx.try_send(("test".to_string(), vec![probe_event("failed-delivery")]))
            .expect("queue has room for the first batch");

        // Wait for the failed-delivery drop to land, which proves the
        // forwarder has moved on to the `FORWARDER_BACKOFF` sleep.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if counters.dropped_batches.load(Ordering::Relaxed) >= 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "forwarder never dropped the first (unreachable-daemon) batch"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        tx.try_send((
            "test".to_string(),
            vec![
                probe_event("queued-during-backoff-1"),
                probe_event("queued-during-backoff-2"),
            ],
        ))
        .expect("queue has room for the batch queued during backoff");

        shutdown.cancel();

        tokio::time::timeout(Duration::from_secs(5), forwarder)
            .await
            .expect("forwarder must exit promptly on shutdown, not wait out the full backoff")
            .expect("forwarder task must not panic");

        assert_eq!(
            counters.dropped_batches.load(Ordering::Relaxed),
            2,
            "the failed-delivery batch and the batch queued during backoff must both be counted as dropped"
        );
        assert_eq!(
            counters.dropped_events.load(Ordering::Relaxed),
            3,
            "1 failed-delivery + 2 queued-during-backoff events must all be counted as dropped"
        );
        assert_eq!(
            counters.forwarded_batches.load(Ordering::Relaxed),
            0,
            "no listener is bound, so nothing can ever be acknowledged"
        );
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

    use khive_storage::event::EventAppendDisposition;
    use khive_types::{EventKind, SubstrateKind};

    fn test_event(namespace: &str) -> Event {
        Event::new(
            namespace,
            "test.verb",
            EventKind::Audit,
            SubstrateKind::Event,
            "actor:test",
        )
    }

    /// Boot a real events daemon on temp paths, poll until its socket accepts.
    #[cfg(unix)]
    async fn boot_daemon(dir: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let db = dir.path().join("events.db");
        let socket = dir.path().join("events.sock");
        let (db_clone, socket_clone) = (db.clone(), socket.clone());
        tokio::spawn(async move {
            let _ = run_events_daemon(&db_clone, &socket_clone).await;
        });
        for _ in 0..100 {
            if UnixStream::connect(&socket).await.is_ok() {
                return (db, socket);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("events daemon did not come up on {}", socket.display());
    }

    /// The split store's routing contract: plain appends land ONLY in the
    /// legacy store (whose raw-SQL consumers depend on finding them there),
    /// the idempotent audit lane lands ONLY in the events lane, and reads
    /// merge both sides into one event plane.
    #[tokio::test]
    async fn split_store_routes_plain_to_legacy_idempotent_to_lane_and_merges_reads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy_backend =
            direct_backend_for(&dir.path().join("legacy.db")).expect("legacy backend");
        let lane_backend = direct_backend_for(&dir.path().join("lane.db")).expect("lane backend");
        let legacy = legacy_backend
            .events_for_namespace("local")
            .expect("legacy store");
        let lane = lane_backend
            .events_for_namespace("local")
            .expect("lane store");
        let split = SplitEventStore::new(Arc::clone(&legacy), Arc::clone(&lane));

        let plain = test_event("local");
        let plain_id = plain.id;
        split.append_event(plain).await.expect("plain append");

        let audit = test_event("local");
        let audit_id = audit.id;
        let result = split
            .append_events_idempotent(vec![audit])
            .await
            .expect("idempotent append");
        assert_eq!(result.rows, vec![EventAppendDisposition::Inserted]);

        // Routing: each row is in exactly its own side.
        assert!(legacy
            .get_event(plain_id)
            .await
            .expect("legacy get")
            .is_some());
        assert!(lane.get_event(plain_id).await.expect("lane get").is_none());
        assert!(legacy
            .get_event(audit_id)
            .await
            .expect("legacy get")
            .is_none());
        assert!(lane.get_event(audit_id).await.expect("lane get").is_some());

        // Merged reads observe one event plane whichever side holds the row.
        assert!(split
            .get_event(plain_id)
            .await
            .expect("split get")
            .is_some());
        assert!(split
            .get_event(audit_id)
            .await
            .expect("split get")
            .is_some());
        assert_eq!(
            split
                .count_events(EventFilter::default())
                .await
                .expect("split count"),
            2
        );
        let page = split
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("split query");
        let ids: Vec<Uuid> = page.items.iter().map(|e| e.id).collect();
        assert!(ids.contains(&plain_id) && ids.contains(&audit_id));

        // Windowing across the merge: page size 1 at offsets 0 and 1 yields
        // the two rows exactly once each.
        let mut seen = Vec::new();
        for offset in 0..2 {
            let page = split
                .query_events(EventFilter::default(), PageRequest { offset, limit: 1 })
                .await
                .expect("windowed query");
            assert_eq!(page.items.len(), 1);
            seen.push(page.items[0].id);
        }
        seen.sort();
        let mut expected = vec![plain_id, audit_id];
        expected.sort();
        assert_eq!(seen, expected);
    }

    /// Mechanized co-residency guard: the amendment's load-bearing claim is
    /// that every class a raw-SQL consumer of the legacy `events` table
    /// depends on still LANDS in that table. Two of the three enumerated
    /// consumers (kg projection guard, graph-query union) fail silently if
    /// the classification drifts, so the classification is guarded here by
    /// the consumers' own access pattern — a raw SQL read against the legacy
    /// backend — rather than by enumeration. Arm 1 reddens if plain appends
    /// (the provenance/domain class) ever stop reaching the legacy table.
    /// Arm 2 is the boundary's other face and doubles as the positive
    /// control for the instrument: the same query DOES find lane-routed rows
    /// on the lane's own backend, so an empty arm-1 result could not be a
    /// broken query.
    #[tokio::test]
    async fn raw_sql_consumers_of_the_legacy_events_table_still_see_plain_appends() {
        use khive_storage::types::{SqlStatement, SqlValue};

        let dir = tempfile::tempdir().expect("tempdir");
        let legacy_backend =
            direct_backend_for(&dir.path().join("legacy-guard.db")).expect("legacy backend");
        let lane_backend =
            direct_backend_for(&dir.path().join("lane-guard.db")).expect("lane backend");
        let split = SplitEventStore::new(
            legacy_backend
                .events_for_namespace("local")
                .expect("legacy store"),
            lane_backend
                .events_for_namespace("local")
                .expect("lane store"),
        );

        // The schedule pack's creator-provenance write is a plain append; the
        // audit flusher's write is an idempotent batch. Route one of each.
        let provenance = test_event("local");
        let provenance_id = provenance.id;
        split.append_event(provenance).await.expect("plain append");
        let audit = test_event("local");
        let audit_id = audit.id;
        split
            .append_events_idempotent(vec![audit])
            .await
            .expect("idempotent append");

        let count_by_raw_sql = |backend: Arc<StorageBackend>, id: Uuid| async move {
            let mut reader = backend.sql().reader().await.expect("sql reader");
            let rows = reader
                .query_all(SqlStatement {
                    sql: "SELECT actor FROM events WHERE id = ?1".to_string(),
                    params: vec![SqlValue::Text(id.to_string())],
                    label: None,
                })
                .await
                .expect("raw events query");
            rows.len()
        };

        // Arm 1: the co-residency contract. A raw-SQL consumer of the legacy
        // table finds the plain-append row there.
        assert_eq!(
            count_by_raw_sql(Arc::clone(&legacy_backend), provenance_id).await,
            1,
            "plain appends must stay visible to raw-SQL consumers of the legacy events table"
        );
        // Arm 2a: the moved class is genuinely gone from the legacy table —
        // this is the silent-failure face of the boundary, held visible.
        assert_eq!(
            count_by_raw_sql(Arc::clone(&legacy_backend), audit_id).await,
            0,
            "audit-lane rows must not land in the legacy events table"
        );
        // Arm 2b: positive control for the instrument — the identical query
        // finds the moved row on the lane backend, so arm 2a's zero (and any
        // future arm-1 zero) is a routing fact, not a dead query.
        assert_eq!(
            count_by_raw_sql(Arc::clone(&lane_backend), audit_id).await,
            1,
            "the raw query must prove it can find rows where they actually live"
        );
    }

    /// End to end over the real socket: the idempotent lane returns true
    /// dispositions, and the row is readable back through the same store.
    #[cfg(unix)]
    #[tokio::test]
    async fn idempotent_append_round_trips_through_the_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_db, socket) = boot_daemon(&dir).await;
        let client = EventsSplitClient::new(socket).expect("client");
        let store = ForwardingEventStore::new("local", client);

        let event = test_event("local");
        let id = event.id;
        let result = store
            .append_events_idempotent(vec![event.clone()])
            .await
            .expect("idempotent append over socket");
        assert_eq!(result.rows, vec![EventAppendDisposition::Inserted]);

        // Retry of the identical row reports AlreadyPresentIdentical — the
        // daemon ran the real idempotent path, not a blind re-insert.
        let retry = store
            .append_events_idempotent(vec![event])
            .await
            .expect("idempotent retry");
        assert_eq!(
            retry.rows,
            vec![EventAppendDisposition::AlreadyPresentIdentical]
        );

        let fetched = store.get_event(id).await.expect("get over socket");
        assert_eq!(fetched.map(|e| e.id), Some(id));
        let count = store
            .count_events(EventFilter::default())
            .await
            .expect("count over socket");
        assert_eq!(count, 1);
    }

    /// The fire-and-forget lane delivers to the daemon store: enqueue returns
    /// immediately and the row becomes visible to a subsequent count.
    #[cfg(unix)]
    #[tokio::test]
    async fn fire_and_forget_append_lands_in_the_daemon_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_db, socket) = boot_daemon(&dir).await;
        let client = EventsSplitClient::new(socket).expect("client");
        let store = ForwardingEventStore::new("local", Arc::clone(&client));

        store
            .append_event(test_event("local"))
            .await
            .expect("append_event is fire-and-forget");

        // Poll for BOTH effects: the row landing daemon-side and the
        // forwarder's own counter — the row can be visible a beat before the
        // forwarder task is rescheduled to record the delivery.
        let mut count = 0;
        let mut metrics = client.metrics();
        for _ in 0..100 {
            count = store
                .count_events(EventFilter::default())
                .await
                .expect("count over socket");
            metrics = client.metrics();
            if count == 1 && metrics.forwarded_events >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(count, 1, "forwarded event must land in the daemon store");
        assert_eq!(metrics.dropped_events, 0);
        assert!(metrics.forwarded_events >= 1, "delivery must be counted");
    }

    /// R4 overflow arm: with the daemon dead, appends beyond the queue bound
    /// are DROPPED and counted while every append still returns Ok — the
    /// domain path proceeds. The drop counter is the loss-tolerance contract.
    #[cfg(unix)]
    #[tokio::test]
    async fn queue_overflow_drops_and_counts_but_never_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dead_socket = dir.path().join("nobody-home.sock");
        let client = EventsSplitClient::new_with_queue_depth(dead_socket, 2).expect("client");
        let store = ForwardingEventStore::new("local", Arc::clone(&client));

        for _ in 0..20 {
            store
                .append_event(test_event("local"))
                .await
                .expect("append_event must not error under overflow");
        }
        let metrics = client.metrics();
        assert!(
            metrics.dropped_events > 0,
            "overflow must register in the drop counter, got {metrics:?}"
        );
    }

    /// R4 dead-socket arm: synchronous lanes fail with a typed retryable
    /// storage error (never hang, never a generic panic), and the offline
    /// preflight validator keeps working with zero I/O.
    #[cfg(unix)]
    #[tokio::test]
    async fn dead_socket_reads_fail_typed_and_preflight_stays_local() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dead_socket = dir.path().join("nobody-home.sock");
        let client = EventsSplitClient::new(dead_socket).expect("client");
        let store = ForwardingEventStore::new("local", client);

        let error = store
            .get_event(Uuid::new_v4())
            .await
            .expect_err("read against a dead socket must fail");
        assert!(
            matches!(error, StorageError::Pool { .. }),
            "expected the typed unreachable error, got {error:?}"
        );

        // The audit-batch seam contract: preflight is local and functional
        // with the daemon down.
        assert!(store.supports_idempotent_audit_batch());
        store
            .preflight_event(&test_event("local"))
            .expect("offline preflight validates a well-formed event");
    }

    /// Version-skew arm: a frame carrying an unknown protocol version gets a
    /// typed non-retryable refusal, not a deserialization failure.
    #[cfg(unix)]
    #[tokio::test]
    async fn protocol_version_skew_is_a_typed_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (_db, socket) = boot_daemon(&dir).await;

        let mut stream = UnixStream::connect(&socket).await.expect("connect");
        let request = EventsRequest::CountEvents {
            protocol_version: EVENTS_PROTOCOL_VERSION + 1,
            namespace: "local".into(),
            filter: EventFilter::default(),
        };
        let payload = serde_json::to_vec(&request).expect("serialize");
        write_frame(&mut stream, &payload).await.expect("write");
        let bytes = read_frame(&mut stream).await.expect("read");
        let response: EventsResponse = serde_json::from_slice(&bytes).expect("parse");
        match response {
            EventsResponse::Error {
                message, retryable, ..
            } => {
                assert!(!retryable, "version skew is not retryable");
                assert!(message.contains("protocol version"), "message: {message}");
            }
            other => panic!("expected a typed refusal, got {other:?}"),
        }
    }

    /// The per-socket daemon guard is exclusive while held and reusable after
    /// release — the mechanism that keeps a supervisor respawn race down to
    /// one surviving daemon.
    #[cfg(unix)]
    #[test]
    fn events_daemon_guard_is_exclusive_then_reusable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("events.sock");
        let first = try_acquire_events_daemon_guard(&socket).expect("first acquire");
        assert!(
            try_acquire_events_daemon_guard(&socket).is_none(),
            "second acquire must fail while the first guard is held"
        );
        drop(first);
        assert!(
            try_acquire_events_daemon_guard(&socket).is_some(),
            "acquire must succeed again after the guard is released"
        );
    }

    /// A read-only file-backed runtime with the split configured must never
    /// create or schema-initialize an events database. Missing events db →
    /// `events()` serves the legacy store alone and leaves the filesystem
    /// untouched; a pre-existing events db (minted by an earlier writable
    /// host) is opened read-only and its rows merge into reads.
    /// (unix-gated for the sidecar-freeze harness step only.)
    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_runtime_never_creates_an_events_db() {
        use crate::{KhiveRuntime, Namespace, RuntimeConfig};

        let dir = tempfile::tempdir().expect("tempdir");
        let main_db = dir.path().join("main.db");
        // Materialize + migrate the main database with a writable runtime.
        drop(
            KhiveRuntime::new(RuntimeConfig {
                db_path: Some(main_db.clone()),
                ..RuntimeConfig::no_embeddings()
            })
            .expect("create main db"),
        );
        let events_db = events_db_path_beside(&main_db);
        assert!(!events_db.exists(), "precondition: no events db yet");

        // Freeze the WAL sidecars the writable runtime left behind — the
        // read-only opener refuses a snapshot with a writable -shm.
        khive_storage::test_support::freeze_snapshot_sidecars(&main_db);

        let split_config = |db: PathBuf| RuntimeConfig {
            db_path: Some(main_db.clone()),
            events_split: Some(EventsSplitConfig {
                db_path: db,
                socket_path: None,
            }),
            ..RuntimeConfig::no_embeddings()
        };

        // Arm 1: read-only runtime, no events db on disk. Reads work through
        // the legacy store and nothing is minted.
        let ro =
            KhiveRuntime::new_readonly(split_config(events_db.clone())).expect("read-only runtime");
        let token = ro.authorize(Namespace::local()).expect("token");
        let store = ro.events(&token).expect("events store");
        let count = store
            .count_events(EventFilter::default())
            .await
            .expect("count through legacy-only plane");
        assert_eq!(count, 0);
        assert!(
            !events_db.exists(),
            "a read-only runtime must not mint the events database"
        );

        // Arm 2: a writable host mints the events db and lands a lane row;
        // the read-only runtime then opens it read-only and merges the row.
        // The writer is scoped (not the process-global registry) and its WAL
        // sidecars frozen after drop — the read-only opener rightly refuses a
        // snapshot with a live writer, and that refusal is not this test's
        // subject.
        {
            let lane_backend = StorageBackend::sqlite(&events_db).expect("writable lane");
            lane_backend
                .events_for_namespace("local")
                .expect("lane store")
                .append_event(test_event("local"))
                .await
                .expect("seed lane row");
        }
        khive_storage::test_support::freeze_snapshot_sidecars(&events_db);
        let store = ro.events(&token).expect("events store with lane present");
        let count = store
            .count_events(EventFilter::default())
            .await
            .expect("merged count");
        assert_eq!(count, 1, "the pre-existing lane row must merge into reads");
    }

    /// Embedded (direct) mode: the process-global backend writes and reads
    /// `events.db` without any daemon.
    #[tokio::test]
    async fn direct_mode_appends_and_reads_without_a_daemon() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("events.db");
        let backend = direct_backend_for(&db).expect("direct backend");
        let store = backend.events_for_namespace("local").expect("store");
        store
            .append_event(test_event("local"))
            .await
            .expect("direct append");
        let count = store
            .count_events(EventFilter::default())
            .await
            .expect("count");
        assert_eq!(count, 1);
    }
}
