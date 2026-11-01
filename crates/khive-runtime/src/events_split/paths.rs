use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

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
/// claiming the remote writer task terminated. Version 4 adds exact event-target
/// filtering and the refusal kind. Older readers would ignore the new filter
/// field, silently broadening a query. Version 5 adds grouped count requests
/// and responses. Version 6 preserves typed count read failures and the caller
/// budget attribution; clients and daemons must be upgraded together.
pub const EVENTS_PROTOCOL_VERSION: u32 = 6;

/// Default bound on the fire-and-forget append queue, in batches. The byte
/// bound below also applies, so a large batch cannot multiply this depth
/// into unbounded retained event memory.
#[cfg(unix)]
pub const DEFAULT_APPEND_QUEUE_BATCHES: usize = 4096;

/// Maximum serialized append-request bytes retained by the fire-and-forget
/// queue and its one in-flight delivery. A single request must also fit the
/// daemon's per-frame cap; the queue budget covers several such requests.
#[cfg(unix)]
pub const DEFAULT_APPEND_QUEUE_BYTES: usize = 32 * 1024 * 1024;

/// Timeout for one synchronous round-trip (idempotent appends, reads).
/// Covers the WHOLE attempt — connect, peer verification, write, read —
/// because each round-trip owns its connection (no shared lock to wait on
/// outside the clock).
#[cfg(unix)]
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Reconnect backoff for the background forwarder after a failed connect.
#[cfg(unix)]
pub(super) const FORWARDER_BACKOFF: Duration = Duration::from_secs(2);

/// Cap on concurrently served daemon connections. Admission control, not a
/// correctness bound: every permitted-uid process on the machine shares the
/// daemon, and without a cap each accepted connection holds a task and a file
/// descriptor for as long as the peer keeps the socket open — a stalled or
/// hostile same-uid peer could exhaust both. Excess connects are dropped
/// (peer sees EOF) and retried by the clients' own reconnect paths.
#[cfg(unix)]
pub(super) const MAX_EVENTS_CONNECTIONS: usize = 128;

/// Per-frame I/O deadline on a served connection: the longest the daemon
/// waits for one request frame to finish arriving, or one response frame to
/// finish sending. A peer that sends a partial frame and stalls is closed
/// instead of holding its task and descriptor forever. Idle well-behaved
/// clients are closed too — that is fine by construction: round-trips open a
/// fresh connection per call, and the forwarder's delivery path retries once
/// on a stale connection.
#[cfg(unix)]
pub(super) const CONN_IO_TIMEOUT: Duration = Duration::from_secs(60);

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
pub(super) const MAX_INFLIGHT_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Cap on the per-namespace store cache. Stores are cheap pool handles, but
/// the namespace string is client-supplied, so an unbounded map is
/// attacker-controlled memory growth. Beyond the cap, stores are built
/// per-request instead of cached — slower, never wrong.
#[cfg(unix)]
pub(super) const MAX_CACHED_NAMESPACE_STORES: usize = 1024;

/// Symlink-chain hop ceiling for events-path resolution. Matches the bound
/// the daemon's socket-path traversal guard enforces, which is itself the
/// kernel's total-links ceiling on Linux (40): both resolvers must agree,
/// or a chain one accepts derives a different identity than the other.
pub(super) const EVENTS_SYMLINK_HOP_BOUND: u32 = 40;

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
/// of at most this many rows, never one wide page. Even a page under this row
/// cap can exceed the IPC frame cap; the daemon then returns a non-retryable
/// `response_frame_size_limit` refusal so the caller can narrow the page.
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
pub(super) fn resolve_dangling_final_component(path: &Path) -> PathBuf {
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
pub(super) fn absolutize(path: &Path) -> PathBuf {
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
