//! Append-only NDJSON event sink for writer-timeout diagnostics.
//!
//! Exists so "zero writer-admission timeouts over the last 24h" is a claim
//! that can be checked from a running daemon instead of grepped out of
//! best-effort `tracing` output that most callers on this path never emit.
//!
//! FAIL-OPEN IS A LATENCY BOUND, NOT JUST AN ERROR-SWALLOW: every database
//! caller path (`ConnectionPool::writer()`'s timeout arm, every standalone
//! busy/locked mapping) only ever pushes an already-formatted event onto a
//! bounded, non-blocking channel (`emit_timeout` → `SyncSender::try_send`)
//! and returns immediately — it never opens a file, never creates a
//! directory, never calls `fsync`, and never blocks on a full channel (a
//! full channel just drops the event and bumps a counter). All of that I/O
//! happens on a single dedicated background thread that owns the NDJSON
//! file exclusively, so there is never a second writer to race for
//! `O_APPEND` atomicity. Pool boot (`init`) mirrors this: it only resolves a
//! directory path (pure, no I/O) and spawns that thread — the thread itself
//! does the first `create_dir_all`/`open` on its own schedule, never on the
//! calling pool's boot path.
//!
//! A "zero timeouts" reading from this file is only meaningful alongside
//! continuous `heartbeat` rows: a silent sink (crashed thread, rotated-away
//! file, dead process) looks identical to a healthy quiet one unless the
//! heartbeat cadence is also checked. The sink is never initialized for an
//! in-memory pool (no database file to name it after) — only a file-backed
//! pool's boot claims the process-global writer thread, so `startup`/
//! `heartbeat` rows always carry a real, file-backed database identity.
//!
//! A short write (the OS accepting only part of a line) is treated as
//! TERMINAL for the sink instance rather than retried: with a single writer
//! thread there is no concurrent-writer interleaving to protect against, but
//! a later, well-formed line landing after an unterminated fragment would
//! itself be silent corruption of the NDJSON stream. Once poisoned, the sink
//! stops appending forever (the drop counter keeps counting, so the loss is
//! at least visible via `sink_error_summary`). An ordinary write/open error
//! (not a short write) is recoverable — the writer thread lazily retries
//! opening the file on its next wakeup, so a transiently unwritable log
//! directory heals without operator intervention.
//!
//! FILES ARE PER-PROCESS: each process writes `writer_timeouts.<pid>.ndjson`
//! in the resolved log directory, never a single shared file. This makes the
//! short-write-poisons-forever contract correct in the face of multiple
//! processes sharing a log directory (a daemon restart, or a short-lived CLI
//! invocation running alongside a long-lived daemon) — poisoning is scoped to
//! the one process whose write actually landed short, never silently
//! blocking a sibling process's otherwise-healthy stream. A READER of this
//! sink must glob `writer_timeouts.*.ndjson` across the directory and merge
//! by `ts_utc`; it must also treat a file whose last line is not valid JSON
//! (an unterminated fragment left by a process that was killed mid-write) as
//! truncated at the last complete line, not as a parse failure for the whole
//! file.
//!
//! A malformed trailing line is always the last line OF ITS FILE — that
//! contract holds not just within one file-open lifetime but across process
//! epochs too, because of one more rule: the writer thread never appends to
//! a pre-existing regular file at its predictable path. On every open
//! attempt it first checks whether something is already there; if it's a
//! regular file (a poisoned fragment left by an earlier epoch of this same
//! pid, e.g. after pid reuse, or by this process's own prior poisoned
//! attempt) it is rotated out of the way to `writer_timeouts.<pid>.r<epoch
//! millis>.ndjson` before a fresh file is created at the original path. A
//! rotated file still matches the reader glob below and keeps its own
//! terminal-fragment property; it just stops being the file this epoch
//! writes to. Something at the path that is NOT a regular file (a FIFO,
//! symlink, or device — an operator or test construct) is left untouched
//! and opened as-is, never rotated.
//!
//! EVENT LOSS AT PROCESS EXIT IS EXPECTED AND SCOPED: an event that has been
//! enqueued but not yet drained and written by the background writer thread
//! is lost if the process exits before the next drain — there is no
//! `atexit`/`Drop`-driven flush. This is deliberate: a flush on exit would
//! have to run synchronously on an exiting thread, is not reliably reachable
//! from every process-exit path (`SIGKILL`, `abort`, a panic that unwinds
//! past the point where any such hook would run), and — if implemented as a
//! blocking drain — would reintroduce exactly the caller-path-latency defect
//! this module exists to avoid, just relocated to shutdown instead of to a
//! write. Consequently, the "zero writer-admission timeouts" acceptance
//! claim this sink exists to support is only sound for LONG-LIVED daemon
//! processes, whose continuous heartbeat rows are what makes "no timeout rows
//! appeared" mean "the daemon was up and reporting, and truly saw none" — a
//! short-lived CLI process (a single `kkernel` invocation that exits after
//! one operation) contributes best-effort rows only: a timeout event it hits
//! right before exit may never make it to disk.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::pool::{ConnectionPool, TEST_HARNESS_ENV};

/// Default liveness cadence. A 24h-zero-timeouts claim is only valid
/// alongside heartbeats at (at least) this cadence — see module docs.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often the writer thread wakes up with no queued event, to retry a
/// previously-failed file open and keep liveness/heartbeat timing accurate.
const DRAIN_INTERVAL: Duration = Duration::from_secs(1);

/// Bounded capacity of the caller → writer-thread event channel. A caller
/// never blocks on this: a full channel just drops the event (counted).
const QUEUE_CAPACITY: usize = 1024;

/// Upper bound, in bytes, on a serialized event's `error` field — an
/// unbounded rusqlite/driver error message must not let one event blow out
/// the queue or the NDJSON line size.
const MAX_ERROR_BYTES: usize = 512;

/// Upper bound on how many queued events the writer thread drains in a
/// single wakeup before yielding back to its own loop (which re-checks the
/// heartbeat deadline and lets `recv_timeout` immediately pick the drain back
/// up if the channel is still non-empty). Bounds one wakeup's worst-case
/// latency to ~256 writes instead of however deep the queue happens to be —
/// under sustained load the heartbeat still fires on schedule because it is
/// checked after every single write, not just after a whole batch.
const DRAIN_BATCH_CAP: usize = 256;

/// Test-only escape hatch: sleep this many milliseconds before each write
/// attempt the writer thread makes. Not gated by `#[cfg(test)]` — like
/// [`HEARTBEAT_MS_OVERRIDE_ENV`], integration tests link the compiled crate
/// as an ordinary dependency (no `--cfg test`), so a `cfg(test)`-only hook
/// would be invisible to them. The check is one env var read, cached for the
/// writer thread's lifetime at startup, and a no-op when unset.
const WRITE_DELAY_MS_OVERRIDE_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_WRITE_DELAY_MS";

/// Per-process NDJSON file name — see the module docs' "FILES ARE
/// PER-PROCESS" section for why this is not a single shared file.
fn ndjson_file_name() -> String {
    format!("writer_timeouts.{}.ndjson", std::process::id())
}

/// Upper bound on rename-collision retries when rotating a pre-existing
/// regular file out of the way (bumping the millisecond component each
/// time) — bounded so a pathological run of same-millisecond collisions
/// can't spin forever.
const ROTATE_COLLISION_RETRIES: u32 = 16;

/// If `path` currently names a regular file, rename it to
/// `writer_timeouts.<pid>.r<epoch millis>.ndjson` in `dir` so a fresh file
/// can be created at `path` without appending to whatever was already
/// there — see the module docs' note on why a malformed line must stay the
/// last line of its own file across process epochs, not just within one
/// file-open lifetime. A no-op (`Ok`) if nothing is at `path`, or if
/// something is there but is not a regular file (FIFO, symlink, device —
/// an operator/test construct, deliberately left untouched and opened
/// as-is by the caller). `Err` only on a real rename failure, which the
/// caller treats as recoverable: skip opening this wakeup, retry the next.
fn rotate_if_regular_file(path: &Path, dir: &Path) -> Result<(), ()> {
    let is_regular_file = std::fs::symlink_metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    if !is_regular_file {
        return Ok(());
    }

    let mut millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    for _ in 0..ROTATE_COLLISION_RETRIES {
        let candidate = dir.join(format!(
            "writer_timeouts.{}.r{millis}.ndjson",
            std::process::id()
        ));
        if candidate.exists() {
            millis += 1;
            continue;
        }
        return std::fs::rename(path, &candidate).map_err(|_| ());
    }
    Err(())
}

/// Fallback log subdirectory, rooted at the database file's parent
/// directory, used only when the primary `<HOME>/.khive/logs` resolution
/// (mirroring `khived.log`'s own resolution) is unavailable.
const FALLBACK_LOG_SUBDIR: &str = ".khive-logs";

/// Explicit override for the sink's log directory. Checked before every
/// other resolution step. Primarily exists so tests never resolve into an
/// operator's real `~/.khive/logs`; also usable operationally in
/// environments where `HOME` doesn't point at the log directory.
const SINK_DIR_OVERRIDE_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_DIR";

/// Test-only override for the heartbeat cadence, so liveness can be
/// exercised without a multi-minute sleep.
const HEARTBEAT_MS_OVERRIDE_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_HEARTBEAT_MS";

/// Where a caller observed a writer-admission timeout or a busy/locked
/// error on a standalone writer connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Site {
    /// `ConnectionPool::writer()` timed out waiting for the pooled writer
    /// mutex.
    PoolAdmission,
    /// A standalone writer connection opened by `stores::graph`.
    StandaloneGraph,
    /// A standalone writer connection opened by `stores::event`.
    StandaloneEvent,
    /// A standalone writer connection opened by `stores::text`.
    StandaloneText,
    /// A standalone writer connection opened by `sql_bridge`.
    StandaloneSqlBridge,
    /// `SqlBridge::writer()`'s standalone-connection fallback, taken while
    /// the write queue is enabled (ADR-136 D1 gate 6c).
    DirectRouteSqlBridgeWriter,
    /// `SqlBridge::atomic_unit()`'s flag-off manual-transaction fallback,
    /// taken while the write queue is enabled (ADR-136 D1 gate 6c).
    DirectRouteAtomicUnit,
    /// `stores::vectors::vec_delete_subjects`'s `with_writer_unmanaged`
    /// fallback, taken while the write queue is enabled.
    DirectRouteVecDeleteSubjects,
    /// `stores::vectors::orphan_sweep`'s `with_writer_unmanaged` fallback,
    /// taken while the write queue is enabled.
    DirectRouteOrphanSweep,
    /// `stores::text::rename_namespace`'s `with_writer_unmanaged` fallback,
    /// taken while the write queue is enabled.
    DirectRouteFtsRenameNamespace,
    /// `stores::text::Fts5TextSearch::with_writer`'s general-helper
    /// `with_writer_unmanaged` fallback, taken while the write queue is
    /// enabled (ADR-136 D1 gate 3 amendment — the general FTS write path).
    DirectRouteFtsGeneralWrite,
    /// `stores::vectors::SqliteVecStore::with_writer`'s general-helper
    /// `with_writer_unmanaged` fallback, taken while the write queue is
    /// enabled (ADR-136 D1 gate 3 amendment — the general vector write path).
    DirectRouteVecGeneralWrite,
}

impl Site {
    fn as_str(self) -> &'static str {
        match self {
            Site::PoolAdmission => "pool_admission",
            Site::StandaloneGraph => "standalone:graph",
            Site::StandaloneEvent => "standalone:event",
            Site::StandaloneText => "standalone:text",
            Site::StandaloneSqlBridge => "standalone:sql_bridge",
            Site::DirectRouteSqlBridgeWriter => "direct_route:sql_bridge_writer",
            Site::DirectRouteAtomicUnit => "direct_route:atomic_unit",
            Site::DirectRouteVecDeleteSubjects => "direct_route:vec_delete_subjects",
            Site::DirectRouteOrphanSweep => "direct_route:orphan_sweep",
            Site::DirectRouteFtsRenameNamespace => "direct_route:fts_rename_namespace",
            Site::DirectRouteFtsGeneralWrite => "direct_route:fts_general_write",
            Site::DirectRouteVecGeneralWrite => "direct_route:vec_general_write",
        }
    }
}

/// A fully-formatted sink event, ready to hand off to the writer thread.
/// Built entirely on the caller's thread (no I/O) and pushed through a
/// bounded channel — this is the only thing a database caller path ever
/// does. `error` is already truncated to [`MAX_ERROR_BYTES`] where present.
///
/// `kind` distinguishes the four durable row shapes this sink emits:
/// `"timeout"` (a writer-admission or busy/locked timeout, carries `site` +
/// `error`), `"queue_saturation"` (a caller-visible `WriteQueueFull`, carries
/// `timeout_ms`), `"writer_task_retirement"` (a `WriterTask` terminal
/// retirement, carries `error` as the retirement reason), and
/// `"direct_route_violation"` (a direct writer acquisition bypassing an
/// enabled queue, carries `site`).
struct QueuedEvent {
    ts_utc: String,
    kind: &'static str,
    db: String,
    site: Option<&'static str>,
    error: Option<String>,
    timeout_ms: Option<u64>,
}

/// Process-global handle to the writer thread's inbox. Set at most once per
/// process by [`init`] — see that function's doc comment for why an
/// in-memory pool never claims this slot.
struct SinkHandle {
    sender: SyncSender<QueuedEvent>,
    /// Shared with the writer thread: incremented here on a full channel
    /// (caller side) and there on a write/open failure (writer-thread side)
    /// — one counter, reported via `sink_error_summary`.
    dropped: Arc<AtomicU64>,
}

static SINK: OnceLock<SinkHandle> = OnceLock::new();

/// One JSON line: `{ts_utc, kind, db, site, error, timeout_ms?}`.
/// `site`/`error`/`timeout_ms`/`pid`/`version` are omitted (not null) when
/// not applicable to `kind` — `startup` rows carry `pid`/`version` and no
/// `site`/`error`; `timeout`/`sink_error_summary` rows are the reverse.
#[derive(serde::Serialize)]
struct EventRecord<'a> {
    ts_utc: String,
    kind: &'a str,
    db: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    site: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
}

#[allow(clippy::too_many_arguments)]
fn build_line(
    ts_utc: String,
    kind: &str,
    db: &str,
    site: Option<&str>,
    error: Option<&str>,
    timeout_ms: Option<u64>,
    pid: Option<u32>,
    version: Option<&str>,
) -> String {
    let record = EventRecord {
        ts_utc,
        kind,
        db,
        site,
        error,
        timeout_ms,
        pid,
        version,
    };
    let mut line = serde_json::to_string(&record).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

/// [`build_line`] stamped with the current time — used by the writer thread
/// for rows it produces synchronously (startup/heartbeat/summary), as
/// opposed to a [`QueuedEvent`]'s own enqueue-time timestamp.
#[allow(clippy::too_many_arguments)]
fn build_line_now(
    kind: &str,
    db: &str,
    site: Option<&str>,
    error: Option<&str>,
    timeout_ms: Option<u64>,
    pid: Option<u32>,
    version: Option<&str>,
) -> String {
    build_line(
        now_rfc3339(),
        kind,
        db,
        site,
        error,
        timeout_ms,
        pid,
        version,
    )
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Truncate `s` to at most `max_bytes` bytes (at a UTF-8 char boundary),
/// appending a marker so a truncated line is visually distinguishable from
/// one that was always short. Bounds a single event's contribution to both
/// the channel and the NDJSON line size regardless of how verbose the
/// underlying driver error is.
fn truncate_error(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("...(truncated)");
    truncated
}

/// Push `event` onto `sender` without ever blocking. A full channel drops
/// the event and increments `dropped` — the only failure mode a caller-side
/// enqueue can have. Pulled out of [`emit_timeout`] so it is unit-testable
/// against a bare channel, with no dependency on the process-global `SINK`.
fn enqueue(sender: &SyncSender<QueuedEvent>, dropped: &AtomicU64, event: QueuedEvent) {
    if sender.try_send(event).is_err() {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Generic append target with terminal-on-short-write semantics: a short
/// write must never be retried, since a later intact line could otherwise
/// land concatenated onto an unterminated fragment. Generic over `W: Write`
/// so the poisoning/recovery decision logic is unit-testable against an
/// in-memory mock, with no real file or thread involved — production only
/// ever instantiates `AppendSink<File>`.
struct AppendSink<W> {
    target: Option<W>,
    poisoned: bool,
    /// Test-only: sleep this long before every write attempt. `Duration::ZERO`
    /// (the default) means no delay — the ordinary production path.
    write_delay: Duration,
}

impl<W: Write> AppendSink<W> {
    fn new() -> Self {
        Self {
            target: None,
            poisoned: false,
            write_delay: Duration::ZERO,
        }
    }

    /// Test-only: arm an artificial per-write delay, so an integration test
    /// can prove the caller path never reaches this writer even when it is
    /// genuinely slow (as opposed to merely absent, which the unwritable-
    /// directory test already covers).
    fn set_write_delay(&mut self, delay: Duration) {
        self.write_delay = delay;
    }

    fn is_open(&self) -> bool {
        self.target.is_some()
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Install a freshly (re)opened target. A no-op once poisoned — a
    /// poisoned sink instance never accepts a replacement target, by
    /// design: "stop appending forever" per the terminal-short-write
    /// contract.
    fn set_target(&mut self, target: W) {
        if !self.poisoned {
            self.target = Some(target);
        }
    }

    /// Attempt one `write(2)`-equivalent call of the full line. `false` on
    /// any outcome that doesn't land the whole line — including "no target
    /// open" and "already poisoned". A short write (partial acceptance)
    /// poisons this instance permanently; an ordinary error just drops the
    /// target so a later [`Self::set_target`] can retry.
    fn write_line(&mut self, line: &str) -> bool {
        if self.poisoned {
            return false;
        }
        let Some(target) = self.target.as_mut() else {
            return false;
        };
        if !self.write_delay.is_zero() {
            thread::sleep(self.write_delay);
        }
        match target.write(line.as_bytes()) {
            Ok(n) if n == line.len() => true,
            Ok(_) => {
                self.poisoned = true;
                self.target = None;
                false
            }
            Err(_) => {
                self.target = None;
                false
            }
        }
    }

    #[cfg(test)]
    fn into_target(self) -> Option<W> {
        self.target
    }
}

impl AppendSink<File> {
    /// Open (creating parent directories as needed) `<dir>/writer_timeouts.<pid>.ndjson`
    /// for append. A no-op if already open or poisoned; on failure the
    /// target simply stays `None` so a later call can retry — this is what
    /// makes a transiently-unwritable directory recoverable rather than a
    /// one-shot permanent disable.
    fn ensure_open(&mut self, dir: &Path) {
        if self.poisoned || self.target.is_some() {
            return;
        }
        let path = dir.join(ndjson_file_name());
        // Never append to a file this process did not create in this
        // epoch. A regular file already at this predictable path is a
        // poisoned fragment from an earlier epoch (pid reuse, or this same
        // process's own prior poisoned attempt) and must be rotated away
        // first. A FIFO/symlink/device at this path is a deliberate
        // operator or test construct (the stalled-writer FIFO fixture
        // relies on exactly this: it is opened as-is, never rotated) and
        // is left alone. A rotation failure is recoverable, same policy as
        // an open failure: skip this wakeup, retry the next one — never
        // fall through to appending the pre-existing file.
        if rotate_if_regular_file(&path, dir).is_err() {
            return;
        }
        let opened = std::fs::create_dir_all(dir)
            .and_then(|_| OpenOptions::new().create(true).append(true).open(&path))
            .ok();
        if let Some(file) = opened {
            self.set_target(file);
        }
    }

    fn sync(&self) {
        if let Some(file) = self.target.as_ref() {
            let _ = file.sync_data();
        }
    }
}

/// Best-effort retention window for `writer_timeouts.*.ndjson` files (both
/// live per-process files and rotated `.rNNN.` fragments). Deliberately
/// generous relative to the sink's own 24h "zero timeouts" acceptance
/// window — this is headroom for slow-to-notice operational issues, not a
/// long-term retention promise. A reader that needs history past this
/// window must copy the files out before it elapses.
const RETENTION_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// One best-effort pass over `dir`, removing `writer_timeouts.*.ndjson`
/// entries whose mtime is older than [`RETENTION_MAX_AGE`]. Owned entirely
/// by the writer thread, run once at startup, so no database caller path
/// and no other process ever pays for a directory scan. Per-entry errors
/// (unreadable metadata, a file removed by a concurrent pruning run of
/// another process) are ignored — a single bad entry must not abort the
/// rest of the pass.
fn prune_expired_sink_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("writer_timeouts.") || !name.ends_with(".ndjson") {
            continue;
        }
        let Ok(age) = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .and_then(|modified| {
                now.duration_since(modified)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            })
        else {
            continue;
        };
        if age > RETENTION_MAX_AGE {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Retry opening the sink file if (and only if) it isn't already permanently
/// poisoned — a poisoned instance must never attempt to reopen (that would
/// contradict "stop appending forever"), so this checks
/// [`AppendSink::is_poisoned`] before paying even the path-join cost of
/// [`AppendSink::ensure_open`].
fn retry_open_if_healthy(sink: &mut AppendSink<File>, dir: &Path) {
    if !sink.is_poisoned() {
        sink.ensure_open(dir);
    }
}

/// Body of the single, process-global sink writer thread: the only code in
/// the process that ever touches the NDJSON file. Opens the file lazily on
/// its own schedule (never on a caller's path — see module docs), retries a
/// failed open every [`DRAIN_INTERVAL`], drains queued timeout events as
/// they arrive (fsync'd — see module docs on why timeouts alone pay that
/// cost), and emits a `heartbeat` row (plus, if anything has been dropped
/// since the last one, a `sink_error_summary` row) every `heartbeat_interval`.
fn write_event(sink: &mut AppendSink<File>, dropped: &AtomicU64, event: QueuedEvent) {
    if !sink.is_open() {
        dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let line = build_line(
        event.ts_utc,
        event.kind,
        &event.db,
        event.site,
        event.error.as_deref(),
        event.timeout_ms,
        None,
        None,
    );
    if sink.write_line(&line) {
        sink.sync();
    } else {
        dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Emit a `heartbeat` row (plus, if anything has been dropped since the last
/// one, a `sink_error_summary` row) if `heartbeat_interval` has elapsed since
/// `last_heartbeat`. Called both between individual writes inside the drain
/// loop and once per outer-loop iteration, so a full channel — up to
/// [`DRAIN_BATCH_CAP`] events deep — can never starve heartbeat cadence: the
/// deadline is checked after every single write, not just after a whole
/// batch.
fn maybe_emit_heartbeat(
    sink: &mut AppendSink<File>,
    dir: &Path,
    db_identity: &str,
    dropped: &AtomicU64,
    last_heartbeat: &mut Instant,
    last_summary_dropped: &mut u64,
    heartbeat_interval: Duration,
) {
    if last_heartbeat.elapsed() < heartbeat_interval {
        return;
    }
    *last_heartbeat = Instant::now();
    sink.ensure_open(dir);
    if !sink.is_open() {
        return;
    }
    let hb = build_line_now(
        "heartbeat",
        db_identity,
        None,
        None,
        None,
        Some(std::process::id()),
        Some(env!("CARGO_PKG_VERSION")),
    );
    sink.write_line(&hb);

    let current_dropped = dropped.load(Ordering::Relaxed);
    if current_dropped > *last_summary_dropped {
        let message = format!(
            "sink drops since last summary: {}",
            current_dropped - *last_summary_dropped
        );
        let summary = build_line_now(
            "sink_error_summary",
            "-",
            None,
            Some(&message),
            None,
            None,
            None,
        );
        if sink.write_line(&summary) {
            *last_summary_dropped = current_dropped;
        }
    }
}

fn writer_thread_loop(
    go: Receiver<()>,
    receiver: Receiver<QueuedEvent>,
    dir: PathBuf,
    db_identity: String,
    dropped: Arc<AtomicU64>,
    heartbeat_interval: Duration,
) {
    // No filesystem effect without a published handle: `init` only sends
    // this token after `SINK.set` confirms this thread's handle won the
    // `OnceLock` race. On a lost race the sender is dropped without ever
    // sending, this `recv` returns `Err`, and the thread exits having
    // touched nothing on disk.
    if go.recv().is_err() {
        return;
    }

    // Retention is owned by the writer thread, run once per process
    // lifetime, after the publication gate above and before the first
    // file open — see `prune_expired_sink_files` and the module docs.
    prune_expired_sink_files(&dir);

    let mut sink = AppendSink::<File>::new();
    sink.set_write_delay(write_delay_from_env());
    let mut last_heartbeat = Instant::now();
    let mut last_summary_dropped = 0u64;

    sink.ensure_open(&dir);
    if sink.is_open() {
        let startup = build_line_now(
            "startup",
            &db_identity,
            None,
            None,
            None,
            Some(std::process::id()),
            Some(env!("CARGO_PKG_VERSION")),
        );
        sink.write_line(&startup);
    }

    loop {
        match receiver.recv_timeout(DRAIN_INTERVAL) {
            Ok(first) => {
                retry_open_if_healthy(&mut sink, &dir);
                write_event(&mut sink, &dropped, first);
                maybe_emit_heartbeat(
                    &mut sink,
                    &dir,
                    &db_identity,
                    &dropped,
                    &mut last_heartbeat,
                    &mut last_summary_dropped,
                    heartbeat_interval,
                );

                // Drain the rest of this wakeup's backlog incrementally
                // (recv one, write one) rather than collecting an unbounded
                // `Vec` up front — a producer-side burst must not force this
                // thread to hold arbitrarily many events in memory before it
                // writes any of them, and the heartbeat deadline is
                // re-checked after every single write below so a full
                // channel can never starve it. Capped at `DRAIN_BATCH_CAP`
                // per wakeup; anything past the cap stays queued and is
                // picked up on the very next loop iteration (`recv_timeout`
                // returns immediately since the channel is still non-empty).
                let mut drained_this_wakeup = 1usize;
                while drained_this_wakeup < DRAIN_BATCH_CAP {
                    match receiver.try_recv() {
                        Ok(event) => {
                            write_event(&mut sink, &dropped, event);
                            drained_this_wakeup += 1;
                            maybe_emit_heartbeat(
                                &mut sink,
                                &dir,
                                &db_identity,
                                &dropped,
                                &mut last_heartbeat,
                                &mut last_summary_dropped,
                                heartbeat_interval,
                            );
                        }
                        Err(_) => break,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                retry_open_if_healthy(&mut sink, &dir);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }

        maybe_emit_heartbeat(
            &mut sink,
            &dir,
            &db_identity,
            &dropped,
            &mut last_heartbeat,
            &mut last_summary_dropped,
            heartbeat_interval,
        );
    }
}

/// Resolve the daemon logs directory the same way `khive-mcp`'s
/// `khived.log` is resolved (`<HOME>/.khive/logs`) without depending on
/// `khive-mcp` — `khive-db` sits below it in the dependency chain — by
/// re-deriving the same `HOME`-relative join. Falls back to
/// `<db_parent>/.khive-logs` when that primary resolution is unavailable:
/// `HOME` unset, or running under the Cargo test harness (which must never
/// resolve into an operator's real `~/.khive/logs` — same rationale as
/// `pool::refuse_home_data_store_in_tests`). Returns `None` only when
/// neither resolution has anywhere to point — in practice this only
/// happens when `db_parent` is `None`, which [`init`] never passes through
/// to this function (an in-memory pool never calls it at all).
fn resolve_log_dir(db_parent: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(SINK_DIR_OVERRIDE_ENV) {
        return Some(PathBuf::from(dir));
    }

    let under_test_harness = std::env::var(TEST_HARNESS_ENV).as_deref() == Ok("1");
    if !under_test_harness {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(Path::new(&home).join(".khive").join("logs"));
        }
    }

    db_parent.map(|p| p.join(FALLBACK_LOG_SUBDIR))
}

fn heartbeat_interval_from_env() -> Duration {
    std::env::var(HEARTBEAT_MS_OVERRIDE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(HEARTBEAT_INTERVAL)
}

/// Test-only: read [`WRITE_DELAY_MS_OVERRIDE_ENV`] once at writer-thread
/// startup. Unset (the production case) resolves to `Duration::ZERO`, which
/// [`AppendSink::write_line`] treats as "no delay" — a no-op check on every
/// write, not a behavior change.
fn write_delay_from_env() -> Duration {
    std::env::var(WRITE_DELAY_MS_OVERRIDE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::ZERO)
}

/// Initialize the process-global sink on first call from a file-backed
/// pool; every later call (from another pool booting in the same process)
/// is a cheap no-op once a sink is in place. Does no filesystem I/O itself —
/// only path resolution (pure) plus spawning the writer thread, which does
/// its own first `create_dir_all`/`open` on its own schedule (see module
/// docs and [`writer_thread_loop`]). This is what keeps pool boot off the
/// filesystem-latency hook entirely.
///
/// `db_parent` is the booting pool's database file's parent directory,
/// `None` for an in-memory pool. An in-memory pool never claims the sink:
/// there is no database file to name a `startup` row after, and claiming
/// the slot first would starve out a later file-backed pool that could
/// have supplied a real identity. `db_identity` names the claiming pool in
/// the `startup` row.
pub(crate) fn init(db_parent: Option<&Path>, db_identity: &str) {
    let Some(db_parent) = db_parent else {
        return;
    };
    if SINK.get().is_some() {
        return;
    }
    let Some(dir) = resolve_log_dir(Some(db_parent)) else {
        return;
    };

    let heartbeat_interval = heartbeat_interval_from_env();
    let (sender, receiver) = mpsc::sync_channel::<QueuedEvent>(QUEUE_CAPACITY);
    let (go_sender, go_receiver) = mpsc::sync_channel::<()>(1);
    let dropped = Arc::new(AtomicU64::new(0));
    let thread_dropped = Arc::clone(&dropped);
    let db_identity = db_identity.to_string();

    // Spawn the writer thread FIRST, before publishing anything through the
    // process-global `OnceLock`. `Builder::spawn` fails when the OS can't
    // create a new thread (resource exhaustion — the process is already at
    // its thread-count or memory limit); if that happens here, the slot
    // must stay unclaimed rather than get permanently wedged into "sink
    // present, but its writer thread never actually started" — a later
    // pool booting in the same process (by which point the transient
    // exhaustion may have cleared) gets to retry `init` from scratch instead
    // of inheriting a dead claim. The spawned thread does no filesystem
    // work of its own accord: it blocks on `go_receiver` until this
    // function sends the go token below, so spawning it here has no
    // observable effect until publication succeeds.
    let spawn_result = thread::Builder::new()
        .name("khive-writer-timeout-sink".to_string())
        .spawn(move || {
            writer_thread_loop(
                go_receiver,
                receiver,
                dir,
                db_identity,
                thread_dropped,
                heartbeat_interval,
            )
        });

    if spawn_result.is_ok() {
        let handle = SinkHandle { sender, dropped };
        // On a lost `OnceLock` race (another pool's `init` call published
        // first), `handle` — and with it `sender` — is simply dropped here,
        // and `go_sender` below is never sent, only dropped. The thread
        // spawned above sees its `go_receiver` error on its blocking `recv`
        // and exits immediately, having done no filesystem work; the pool
        // that won the race already has its own thread running.
        if SINK.set(handle).is_ok() {
            let _ = go_sender.send(());
        }
    }
}

/// This pool's identity string for the sink's `db` field: its canonical
/// database path, or `"memory"` for an in-memory pool.
pub(crate) fn db_label(pool: &ConnectionPool) -> String {
    pool.canonical_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "memory".to_string())
}

/// Record a writer-admission or busy/locked timeout event. Builds the event
/// (no I/O) and hands it to the writer thread via a non-blocking bounded
/// channel — this call itself can never block, slow, or error a database
/// caller. A no-op if the sink was never initialized (no file-backed pool
/// has booted yet in this process).
pub(crate) fn emit_timeout(db: &str, site: Site, error: &str, timeout_ms: Option<u64>) {
    let Some(handle) = SINK.get() else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "timeout",
        db: db.to_string(),
        site: Some(site.as_str()),
        error: Some(truncate_error(error, MAX_ERROR_BYTES)),
        timeout_ms,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a `queue_saturation` event: a caller-visible
/// `StorageError::WriteQueueFull` — the bounded `WriterTask` channel was
/// full for the caller's whole `send_with_timeout` deadline and the request
/// was never accepted (ADR-136 D1 gate 6a). Before this, `WriteQueueFull`
/// reached the caller with no sink emission at all.
pub(crate) fn emit_queue_saturation(db: &str, timeout_ms: u64) {
    let Some(handle) = SINK.get() else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "queue_saturation",
        db: db.to_string(),
        site: None,
        error: None,
        timeout_ms: Some(timeout_ms),
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a `writer_task_retirement` event: the `WriterTask` drain loop
/// reached a terminal request or connection state and is closing admission
/// permanently (ADR-136 D1 gate 6b). Before this, retirement was observable
/// only through a `tracing::error!` line before the queue closed.
pub(crate) fn emit_writer_task_retirement(db: &str, reason: &str) {
    let Some(handle) = SINK.get() else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "writer_task_retirement",
        db: db.to_string(),
        site: None,
        error: Some(truncate_error(reason, MAX_ERROR_BYTES)),
        timeout_ms: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a `direct_route_violation` event: a write path acquired a writer
/// connection directly (standalone connection or pool mutex), bypassing the
/// `WriterTask` queue, while `PoolConfig::write_queue_enabled` was `true`
/// (ADR-136 D1 gate 6c). Emitted on the degrade path itself — never gated on
/// `write_routing_strict`, since strict mode turns the same condition into a
/// caller-visible error instead of a degrade, so there is no bypass left to
/// report there.
pub(crate) fn emit_direct_route_violation(db: &str, site: Site) {
    let Some(handle) = SINK.get() else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "direct_route_violation",
        db: db.to_string(),
        site: Some(site.as_str()),
        error: None,
        timeout_ms: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// `true` for the two rusqlite error codes a standalone writer connection
/// surfaces under write contention (`SQLITE_BUSY` / `SQLITE_LOCKED`).
pub(crate) fn is_busy_or_locked(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if matches!(
                inner.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Emit a `timeout` event for `err` if (and only if) it classifies as
/// busy/locked; otherwise a no-op. Convenience for the standalone-writer
/// call sites in `stores::graph` and `stores::event`, which see the raw
/// `rusqlite::Error` before it is mapped to `StorageError`.
pub(crate) fn maybe_emit_busy(db: &str, site: Site, err: &rusqlite::Error) {
    if is_busy_or_locked(err) {
        emit_timeout(db, site, &err.to_string(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn resolve_log_dir_prefers_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(SINK_DIR_OVERRIDE_ENV, dir.path());
        let resolved = resolve_log_dir(None);
        std::env::remove_var(SINK_DIR_OVERRIDE_ENV);
        assert_eq!(resolved, Some(dir.path().to_path_buf()));
    }

    #[test]
    fn resolve_log_dir_falls_back_to_db_parent_under_test_harness() {
        std::env::remove_var(SINK_DIR_OVERRIDE_ENV);
        // KHIVE_TEST_HARNESS=1 is force-set by .cargo/config.toml for every
        // cargo test/binary in this workspace — assert the behavior this
        // module relies on to keep tests off the real ~/.khive/logs.
        assert_eq!(std::env::var(TEST_HARNESS_ENV).as_deref(), Ok("1"));
        let parent = Path::new("/tmp/does-not-need-to-exist");
        let resolved = resolve_log_dir(Some(parent));
        assert_eq!(resolved, Some(parent.join(FALLBACK_LOG_SUBDIR)));
    }

    #[test]
    fn resolve_log_dir_none_without_db_parent_under_test_harness() {
        std::env::remove_var(SINK_DIR_OVERRIDE_ENV);
        assert_eq!(resolve_log_dir(None), None);
    }

    #[test]
    fn truncate_error_leaves_short_strings_untouched() {
        let short = "database is locked";
        assert_eq!(truncate_error(short, MAX_ERROR_BYTES), short);
    }

    #[test]
    fn truncate_error_bounds_long_strings() {
        let long = "x".repeat(MAX_ERROR_BYTES * 4);
        let truncated = truncate_error(&long, MAX_ERROR_BYTES);
        assert!(
            truncated.len() <= MAX_ERROR_BYTES + "...(truncated)".len(),
            "truncated length {} exceeds bound",
            truncated.len()
        );
        assert!(truncated.ends_with("...(truncated)"));
    }

    #[test]
    fn truncate_error_respects_utf8_char_boundaries() {
        // A multi-byte character straddling the byte cutoff must not panic
        // and must produce valid UTF-8.
        let s = "a".repeat(MAX_ERROR_BYTES - 1) + "€€€€";
        let truncated = truncate_error(&s, MAX_ERROR_BYTES);
        assert!(truncated.is_char_boundary(truncated.len() - "...(truncated)".len()));
    }

    /// `enqueue` (the whole of what a caller-path emission does) never
    /// blocks on a full channel — it drops the event and counts it. No
    /// `SINK`/`OnceLock`/thread involved, so this is deterministic.
    #[test]
    fn enqueue_drops_and_counts_on_full_channel() {
        let (sender, _receiver) = mpsc::sync_channel::<QueuedEvent>(1);
        let dropped = AtomicU64::new(0);
        let make_event = || QueuedEvent {
            ts_utc: now_rfc3339(),
            kind: "timeout",
            db: "test-db".to_string(),
            site: Some(Site::PoolAdmission.as_str()),
            error: Some("boom".to_string()),
            timeout_ms: Some(5),
        };

        enqueue(&sender, &dropped, make_event());
        assert_eq!(dropped.load(Ordering::Relaxed), 0);

        // Channel capacity is 1 and nothing has drained it yet, so this
        // second enqueue must be dropped rather than block.
        enqueue(&sender, &dropped, make_event());
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    /// A short write (partial acceptance) is TERMINAL for the sink instance
    /// — no later line may land after an unterminated fragment. Uses an
    /// in-memory mock `Write` so the failure mode is exact and
    /// reproducible, with no reliance on OS-level file-descriptor tricks.
    struct ShortWriteOnceThenOk {
        calls: usize,
        written: Vec<u8>,
    }

    impl Write for ShortWriteOnceThenOk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 && buf.len() > 1 {
                // Accept only part of the buffer once.
                let n = buf.len() - 1;
                self.written.extend_from_slice(&buf[..n]);
                Ok(n)
            } else {
                self.written.extend_from_slice(buf);
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn append_sink_short_write_poisons_permanently() {
        let mut sink = AppendSink::<ShortWriteOnceThenOk>::new();
        sink.set_target(ShortWriteOnceThenOk {
            calls: 0,
            written: Vec::new(),
        });

        assert!(
            !sink.write_line("first line\n"),
            "a short write must be reported as failed"
        );
        assert!(sink.is_poisoned());
        assert!(!sink.is_open(), "the target must be dropped on poisoning");

        // Even a fresh, healthy target must never be accepted again.
        sink.set_target(ShortWriteOnceThenOk {
            calls: 0,
            written: Vec::new(),
        });
        assert!(
            !sink.is_open(),
            "set_target must refuse to arm a poisoned sink"
        );
        assert!(
            !sink.write_line("second line\n"),
            "a poisoned sink must never write again"
        );
    }

    /// Distinguishes "hard I/O error" (recoverable — the target is
    /// dropped, but a later `set_target` heals it) from "short write"
    /// (terminal — see the test above).
    /// Shares a "has failed once, globally" flag across separately
    /// constructed instances — mirrors the real scenario: the writer
    /// thread's *first* target (this drain's `File`) fails, gets dropped,
    /// and a *later, distinct* target (the next `ensure_open`'s freshly
    /// reopened `File`) succeeds. Resetting a per-instance counter instead
    /// would make the second instance fail too, which is not what "retry
    /// heals" means.
    struct ErrorOnceThenOk {
        already_failed: Arc<std::sync::atomic::AtomicBool>,
        written: Vec<u8>,
    }

    impl Write for ErrorOnceThenOk {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.already_failed.swap(true, Ordering::Relaxed) {
                return Err(std::io::Error::other("injected failure"));
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn append_sink_hard_error_is_recoverable_not_poisoned() {
        let already_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut sink = AppendSink::<ErrorOnceThenOk>::new();
        sink.set_target(ErrorOnceThenOk {
            already_failed: Arc::clone(&already_failed),
            written: Vec::new(),
        });

        assert!(!sink.write_line("first line\n"));
        assert!(
            !sink.is_poisoned(),
            "an ordinary write error must not be terminal"
        );
        assert!(!sink.is_open(), "the failed target is dropped");

        // A later retry (mirroring the writer thread's next `ensure_open`
        // reopening a fresh `File`) must be able to arm and write
        // successfully.
        sink.set_target(ErrorOnceThenOk {
            already_failed: Arc::clone(&already_failed),
            written: Vec::new(),
        });
        assert!(sink.write_line("second line\n"));
    }

    /// A temporarily unwritable log directory must heal once it becomes
    /// writable — a permanent silent disable after one failed open is not
    /// acceptable behavior.
    #[test]
    fn append_sink_recovers_after_transient_directory_failure() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let bogus_dir = blocker.join("logs");

        let mut sink = AppendSink::<File>::new();
        sink.ensure_open(&bogus_dir);
        assert!(!sink.is_open(), "open against a blocked path must fail");
        assert!(!sink.is_poisoned(), "a failed open must not be terminal");

        // "Fix" the directory: remove the blocking file, so the same path
        // (now backed by a real directory) can be created and opened.
        std::fs::remove_file(&blocker).unwrap();
        sink.ensure_open(&bogus_dir);
        assert!(
            sink.is_open(),
            "a later ensure_open against a now-writable directory must succeed"
        );
        assert!(sink.write_line("recovered\n"));

        let contents = std::fs::read_to_string(bogus_dir.join(ndjson_file_name())).unwrap();
        assert!(contents.contains("recovered"));
    }

    /// Coverage-contract concurrency test: many threads enqueuing
    /// simultaneously must never panic or corrupt the channel; every
    /// accepted event, once drained by a single-threaded writer, produces
    /// an intact JSON line — no interleaving is even structurally possible
    /// with one writer thread, but this proves the producer side is sound
    /// under real concurrent load.
    #[test]
    fn concurrent_enqueue_then_single_threaded_drain_produces_intact_lines() {
        let (sender, receiver) = mpsc::sync_channel::<QueuedEvent>(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));

        const THREADS: usize = 64;
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let sender = sender.clone();
                let dropped = Arc::clone(&dropped);
                thread::spawn(move || {
                    let event = QueuedEvent {
                        ts_utc: now_rfc3339(),
                        kind: "timeout",
                        db: "concurrent-test".to_string(),
                        site: Some(Site::StandaloneGraph.as_str()),
                        error: Some(format!("contention-{i}")),
                        timeout_ms: Some(i as u64),
                    };
                    enqueue(&sender, &dropped, event);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        drop(sender);

        let mut sink = AppendSink::<Vec<u8>>::new();
        sink.set_target(Vec::new());
        let mut written_lines = 0usize;
        while let Ok(event) = receiver.try_recv() {
            let line = build_line(
                event.ts_utc,
                event.kind,
                &event.db,
                event.site,
                event.error.as_deref(),
                event.timeout_ms,
                None,
                None,
            );
            if sink.write_line(&line) {
                written_lines += 1;
            }
        }

        assert_eq!(
            written_lines + dropped.load(Ordering::Relaxed) as usize,
            THREADS,
            "every enqueue must be either written or counted as dropped"
        );
        assert_eq!(
            dropped.load(Ordering::Relaxed),
            0,
            "queue capacity exceeds THREADS"
        );

        let bytes = sink.into_target().expect("target must still be armed");
        let contents = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), THREADS);
        for line in &lines {
            let parsed: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("corrupt line {line:?}: {e}"));
            assert_eq!(parsed["kind"], "timeout");
            assert_eq!(parsed["site"], "standalone:graph");
        }
    }

    #[test]
    fn is_busy_or_locked_classifies_sqlite_error_codes() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("database is locked".to_string()),
        );
        assert!(is_busy_or_locked(&busy));

        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            Some("table is locked".to_string()),
        );
        assert!(is_busy_or_locked(&locked));

        let other = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("constraint failed".to_string()),
        );
        assert!(!is_busy_or_locked(&other));

        assert!(!is_busy_or_locked(&rusqlite::Error::QueryReturnedNoRows));
    }
}
