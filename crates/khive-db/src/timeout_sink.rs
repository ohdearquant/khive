//! Append-only NDJSON event sink for writer diagnostics.
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
//! happens on one dedicated background thread per resolved log directory,
//! owning its NDJSON file exclusively, so no second writer races for
//! `O_APPEND` atomicity. Pool boot (`init`) mirrors this: it only resolves a
//! directory path (pure, no I/O) and spawns that thread — the thread itself
//! does the first `create_dir_all`/`open` on its own schedule, never on the
//! calling pool's boot path.
//!
//! A "zero timeouts" reading from this file is only meaningful alongside
//! continuous `heartbeat` rows: a silent sink (crashed thread, rotated-away
//! file, dead process) looks identical to a healthy quiet one unless the
//! heartbeat cadence is also checked. The sink is never initialized for an
//! in-memory or read-only pool (neither can report a writer-admission timeout)
//! — only a writable file-backed pool registers with its resolved directory's
//! writer, so `startup`/`heartbeat` rows carry a writable database identity.
//! Skipping read-only pools prevents inspection from writing a sibling log tree.
//! Ordinary pool Drop leaves the sink running. An explicit close handle, obtained
//! before pool release and awaited after writer settlement, drains and joins it;
//! new registration in that directory is refused while it is closing.
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
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::pool::{ConnectionPool, TEST_HARNESS_ENV};

#[path = "timeout_sink/lifecycle.rs"]
mod lifecycle;
pub(crate) use lifecycle::{Registration, WriterLease};
pub use lifecycle::{TimeoutSinkCloseError, TimeoutSinkCloseHandle};

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

/// Test-harness-only synchronization point for proving that pool construction
/// does not wait for the sink writer. When set to a directory, the writer
/// creates `reached`, waits for `release`, then creates `resumed` before its
/// first sink filesystem operation. The hook is inert unless Cargo's
/// [`TEST_HARNESS_ENV`] marker is also present.
const STARTUP_BARRIER_DIR_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_STARTUP_BARRIER_DIR";

/// Caller-experienced write latency at or above this bound produces a
/// `slow_write` sink row. Exists because the ADR-136 write queue converts
/// what used to be a caller-visible admission FAILURE (a `timeout` row) into
/// caller-visible WAITING: under burst, contention now presents as latency,
/// which the failure-shaped rows are structurally unable to record — a
/// quiet sink no longer means an uncontended writer. The bound is on the
/// whole request span (queue wait + transaction stages), i.e.
/// the quantity a blocked caller actually experiences.
const SLOW_WRITE_THRESHOLD: Duration = Duration::from_secs(1);

/// Override for [`SLOW_WRITE_THRESHOLD`], in milliseconds. `0` disables
/// slow-write rows entirely. Read once per spawned `WriterTask` handle (at
/// spawn, not per write), so tests get a deterministic value by setting it
/// before spawning their own writer task.
const SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV: &str = "KHIVE_SLOW_WRITE_THRESHOLD_MS";

/// Resolve the slow-write latency bound: env override if parseable, else
/// [`SLOW_WRITE_THRESHOLD`]. `Some(0)` from the override resolves to `None`
/// (disabled) — a zero bound would stamp every write and turn the sink into
/// a write log, which it is not.
pub(crate) fn slow_write_threshold() -> Option<Duration> {
    match crate::env::env_parse_or(
        SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV,
        SLOW_WRITE_THRESHOLD.as_millis() as u64,
    ) {
        0 => None,
        ms => Some(Duration::from_millis(ms)),
    }
}

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
    /// A write of any writer class waited out the guard deadline for the
    /// volume lease, which it takes before its connection (ADR-154 §3).
    VolumeLease,
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
    /// `stores::entity::SqlEntityStore`'s pool-writer fallback.
    DirectRouteEntity,
    /// `stores::note::SqlNoteStore`'s pool-writer fallback.
    DirectRouteNote,
    /// `stores::graph::SqlGraphStore`'s standalone/pool-writer fallback.
    DirectRouteGraphGeneralWrite,
    /// `stores::event::SqlEventStore`'s standalone/pool-writer fallback.
    DirectRouteEventGeneralWrite,
    /// `stores::sparse::SqliteSparseStore`'s pool-writer fallback.
    DirectRouteSparseGeneralWrite,
    /// `stores::agents::SqlAgentStore`'s standalone/pool-writer fallback.
    DirectRouteAgentGeneralWrite,
    DirectRouteRuntimeMergeEntity,
    DirectRouteRuntimeMergeNote,
    DirectRouteRuntimeUpdateSymmetricEdge,
}

impl Site {
    fn as_str(self) -> &'static str {
        match self {
            Site::PoolAdmission => "pool_admission",
            Site::VolumeLease => "volume_lease",
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
            Site::DirectRouteEntity => "direct_route:entity",
            Site::DirectRouteNote => "direct_route:note",
            Site::DirectRouteGraphGeneralWrite => "direct_route:graph_general_write",
            Site::DirectRouteEventGeneralWrite => "direct_route:event_general_write",
            Site::DirectRouteSparseGeneralWrite => "direct_route:sparse_general_write",
            Site::DirectRouteAgentGeneralWrite => "direct_route:agent_general_write",
            Site::DirectRouteRuntimeMergeEntity => "direct_route:runtime_merge_entity",
            Site::DirectRouteRuntimeMergeNote => "direct_route:runtime_merge_note",
            Site::DirectRouteRuntimeUpdateSymmetricEdge => {
                "direct_route:runtime_update_symmetric_edge"
            }
        }
    }
}

/// A fully-formatted sink event, ready to hand off to the writer thread.
/// Built entirely on the caller's thread (no I/O) and pushed through a
/// bounded channel — this is the only thing a database caller path ever
/// does. `error` is already truncated to [`MAX_ERROR_BYTES`] where present.
///
/// `kind` distinguishes the durable row shapes this sink emits:
/// `"timeout"` (a writer-admission or busy/locked timeout, carries `site` +
/// `error`; a volume-lease timeout also carries `phase` = `"lock_contended"`),
/// `"queue_saturation"` (a caller-visible `WriteQueueFull`, carries
/// `timeout_ms`), `"writer_task_retirement"` (a `WriterTask` terminal
/// retirement, carries `error` as the retirement reason),
/// `"writer_guard_drop"` (a pooled writer guard released inside a transaction
/// and settled by its drop, carries `error` as the settlement outcome),
/// `"sqlite_full"` (SQLite exhausted the volume during a write),
/// `"direct_route_violation"` (a direct writer acquisition bypassing an
/// enabled queue, carries `site`), and `"slow_write"` (a queued write whose
/// send-to-reply span met [`SLOW_WRITE_THRESHOLD`], carries `elapsed_ms` +
/// `queue_depth` plus the four decomposed writer-stage durations, and a
/// `labels` array naming the statements the writer ran).
struct QueuedEvent {
    ts_utc: String,
    kind: &'static str,
    db: String,
    site: Option<&'static str>,
    phase: Option<&'static str>,
    error: Option<String>,
    timeout_ms: Option<u64>,
    elapsed_ms: Option<u64>,
    queue_depth: Option<u64>,
    writer_stages: Option<WriterStageFields>,
    labels: Option<SlowWriteLabels>,
}

/// Bounded labels carried on a `"slow_write"` row: at most
/// [`crate::writer_task::MAX_WRITE_LABELS`] distinct labels in first-seen
/// order, plus the count of further distinct labels that were dropped.
#[derive(Clone, serde::Serialize)]
pub(crate) struct SlowWriteLabels {
    pub(crate) labels: Box<[String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) labels_truncated: Option<u64>,
}

#[derive(Clone, serde::Serialize)]
struct WriterStageFields {
    queue_wait_micros: u64,
    transaction_acquire_micros: u64,
    body_micros: u64,
    commit_micros: u64,
    total_micros: u64,
}

/// Handle to one resolved directory's writer-thread inbox.
struct SinkHandle {
    sender: SyncSender<QueuedEvent>,
    /// Shared with the writer thread: incremented here on a full channel
    /// (caller side) and there on a write/open failure (writer-thread side)
    /// — one counter, reported via `sink_error_summary`.
    dropped: Arc<AtomicU64>,
}

#[cfg(test)]
static SQLITE_FULL_CAPTURE: std::sync::Mutex<Option<mpsc::Sender<String>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn capture_sqlite_full_for_test(sender: Option<mpsc::Sender<String>>) {
    *SQLITE_FULL_CAPTURE.lock().unwrap() = sender;
}

/// One JSON line: `{ts_utc, kind, db, site, phase?, error, timeout_ms?}`.
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
    phase: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    queue_depth: Option<u64>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    writer_stages: Option<WriterStageFields>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    labels: Option<SlowWriteLabels>,
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
    phase: Option<&str>,
    error: Option<&str>,
    timeout_ms: Option<u64>,
    elapsed_ms: Option<u64>,
    queue_depth: Option<u64>,
    writer_stages: Option<WriterStageFields>,
    labels: Option<SlowWriteLabels>,
    pid: Option<u32>,
    version: Option<&str>,
) -> String {
    let record = EventRecord {
        ts_utc,
        kind,
        db,
        site,
        phase,
        error,
        timeout_ms,
        elapsed_ms,
        queue_depth,
        writer_stages,
        labels,
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
        None,
        error,
        timeout_ms,
        None,
        None,
        None,
        None,
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
/// against a bare channel, with no dependency on the routing registry.
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
        event.phase,
        event.error.as_deref(),
        event.timeout_ms,
        event.elapsed_ms,
        event.queue_depth,
        event.writer_stages,
        event.labels,
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
    // No filesystem effect without a published registry entry. Registration
    // retains this thread's join before sending its startup token.
    if go.recv().is_err() {
        return;
    }

    pause_at_startup_barrier_for_test();

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
    let millis = crate::env::env_parse_or(HEARTBEAT_MS_OVERRIDE_ENV, 0_u64);
    if millis > 0 {
        Duration::from_millis(millis)
    } else {
        HEARTBEAT_INTERVAL
    }
}

/// Test-only: read [`WRITE_DELAY_MS_OVERRIDE_ENV`] once at writer-thread
/// startup. Unset (the production case) resolves to `Duration::ZERO`, which
/// [`AppendSink::write_line`] treats as "no delay" — a no-op check on every
/// write, not a behavior change.
fn write_delay_from_env() -> Duration {
    Duration::from_millis(crate::env::env_parse_or(WRITE_DELAY_MS_OVERRIDE_ENV, 0_u64))
}

fn pause_at_startup_barrier_for_test() {
    if std::env::var(TEST_HARNESS_ENV).as_deref() != Ok("1") {
        return;
    }
    let Some(dir) = std::env::var_os(STARTUP_BARRIER_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    if std::fs::write(dir.join("reached"), b"reached").is_err() {
        return;
    }
    while !dir.join("release").exists() {
        thread::sleep(Duration::from_millis(1));
    }
    let _ = std::fs::write(dir.join("resumed"), b"resumed");
}

/// Register a writable file-backed pool with its resolved directory's sink.
/// This resolves paths and spawns a thread without caller-side filesystem I/O.
/// Pools sharing the directory share its writer. Ordinary Drop keeps the sink
/// running; explicit close refuses new registration until the old thread joins.
/// In-memory and read-only pools do not register. Thread-allocation failure is
/// best-effort as before; a later pool may retry initialization.
pub(crate) fn init(
    db_parent: Option<&Path>,
    db_identity: &str,
) -> Result<Option<Registration>, crate::error::SqliteError> {
    let Some(db_parent) = db_parent else {
        return Ok(None);
    };
    let Some(dir) = resolve_log_dir(Some(db_parent)) else {
        return Ok(None);
    };
    lifecycle::init(dir, db_identity)
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
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "timeout",
        db: db.to_string(),
        site: Some(site.as_str()),
        phase: None,
        error: Some(truncate_error(error, MAX_ERROR_BYTES)),
        timeout_ms,
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a writer timeout at the volume lease: the write waited out the guard
/// deadline because another writer held the volume. The same `timeout` row as
/// the pool mutex stage, with `site` naming the lease and `phase` the capacity
/// phase the caller received (`CapacityUnavailable { phase: LockContended }`).
pub(crate) fn emit_lease_timeout(db: &str, error: &str, timeout_ms: u64) {
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "timeout",
        db: db.to_string(),
        site: Some(Site::VolumeLease.as_str()),
        phase: Some(khive_storage::CapacityUnavailablePhase::LockContended.as_str()),
        error: Some(truncate_error(error, MAX_ERROR_BYTES)),
        timeout_ms: Some(timeout_ms),
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

fn is_sqlite_full(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            current.downcast_ref::<rusqlite::Error>()
        {
            if code.code == rusqlite::ErrorCode::DiskFull {
                return true;
            }
        }
        source = current.source();
    }
    false
}

/// An actual SQLite out-of-space error means the reserve was bypassed or an
/// admitted transaction exceeded it. Keep that escalation separate from
/// contention timeouts; the sink itself remains nonblocking and best effort.
pub(crate) fn maybe_emit_sqlite_full(db: &str, error: &(dyn std::error::Error + 'static)) {
    if !is_sqlite_full(error) {
        return;
    }
    #[cfg(test)]
    if let Some(sender) = SQLITE_FULL_CAPTURE.lock().unwrap().as_ref() {
        let _ = sender.send(db.to_string());
    }
    tracing::error!(db, error = %error, "SQLite ran out of disk space during a write");
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = sqlite_full_event(db, error);
    enqueue(&handle.sender, &handle.dropped, event);
}

fn sqlite_full_event(db: &str, error: &(dyn std::error::Error + 'static)) -> QueuedEvent {
    QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "sqlite_full",
        db: db.to_string(),
        site: None,
        phase: None,
        error: Some(truncate_error(&error.to_string(), MAX_ERROR_BYTES)),
        timeout_ms: None,
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    }
}

/// Record a `queue_saturation` event: a caller-visible
/// `StorageError::WriteQueueFull` — the bounded `WriterTask` channel was
/// full for the caller's whole `send_with_timeout` deadline and the request
/// was never accepted (ADR-136 D1 gate 6a). Before this, `WriteQueueFull`
/// reached the caller with no sink emission at all.
pub(crate) fn emit_queue_saturation(db: &str, timeout_ms: u64) {
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "queue_saturation",
        db: db.to_string(),
        site: None,
        phase: None,
        error: None,
        timeout_ms: Some(timeout_ms),
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a `writer_task_retirement` event: the `WriterTask` drain loop
/// reached a terminal request or connection state and is closing admission
/// permanently (ADR-136 D1 gate 6b). Before this, retirement was observable
/// only through a `tracing::error!` line before the queue closed.
pub(crate) fn emit_writer_task_retirement(db: &str, reason: &str) {
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "writer_task_retirement",
        db: db.to_string(),
        site: None,
        phase: None,
        error: Some(truncate_error(reason, MAX_ERROR_BYTES)),
        timeout_ms: None,
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a pooled writer guard released inside a transaction, which its drop
/// then settled. `outcome` says whether the rollback restored autocommit or the
/// writer was retired.
pub(crate) fn emit_writer_guard_drop(db: &str, outcome: &str) {
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "writer_guard_drop",
        db: db.to_string(),
        site: None,
        phase: None,
        error: Some(truncate_error(outcome, MAX_ERROR_BYTES)),
        timeout_ms: None,
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

#[cfg(test)]
thread_local! {
    static DIRECT_ROUTE_CAPTURE: std::cell::RefCell<Option<Vec<(String, Site)>>> = const {
        std::cell::RefCell::new(None)
    };
}

/// `slow_write` rows [`db`, labels] captured by [`SlowWriteCapture`]. The
/// drain loop runs a request's telemetry on a blocking-pool thread the test
/// cannot itself run on, so the capture is process-global rather than
/// thread-local; the capture tests hold [`SLOW_WRITE_CAPTURE_LOCK`] and run
/// `#[serial]` so that at most one arming test is active at a time. Each
/// captured row carries its db label so a test can select its own row even if
/// a concurrent unrelated writer task (which could inherit the arming test's
/// threshold override at spawn) contributes one.
#[cfg(test)]
static SLOW_WRITE_CAPTURE: std::sync::Mutex<Option<Vec<(String, SlowWriteLabels)>>> =
    std::sync::Mutex::new(None);

/// Serializes [`SlowWriteCapture`] arms within the test binary; a slow-write
/// test must also carry `#[serial_test::serial]` so no other test is
/// mid-write while the capture is armed. An async lock, because a capture
/// test holds it across the awaited write.
#[cfg(test)]
pub(crate) static SLOW_WRITE_CAPTURE_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

/// An armed [`SLOW_WRITE_CAPTURE`], disarmed on drop (including on unwind).
/// The caller must hold [`SLOW_WRITE_CAPTURE_LOCK`] for the guard's whole
/// life; [`Self::drain`] consumes it and returns the captured rows in
/// emission order.
#[cfg(test)]
pub(crate) struct SlowWriteCapture {
    _private: (),
}

#[cfg(test)]
impl SlowWriteCapture {
    /// Arm the capture; panics if one is already armed.
    pub(crate) fn arm() -> Self {
        let mut slot = SLOW_WRITE_CAPTURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(slot.is_none(), "nested slow-write capture");
        *slot = Some(Vec::new());
        Self { _private: () }
    }

    /// Return the `(db, labels)` rows captured so far and disarm.
    pub(crate) fn drain(self) -> Vec<(String, SlowWriteLabels)> {
        let rows = SLOW_WRITE_CAPTURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        std::mem::forget(self);
        rows
    }
}

#[cfg(test)]
impl Drop for SlowWriteCapture {
    fn drop(&mut self) {
        *SLOW_WRITE_CAPTURE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Record a `(db, labels)` row into the capture when one is armed (test
/// helper).
#[cfg(test)]
fn record_slow_write_capture(db: &str, labels: SlowWriteLabels) {
    if let Some(rows) = SLOW_WRITE_CAPTURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        rows.push((db.to_owned(), labels));
    }
}

#[cfg(test)]
pub(crate) fn capture_direct_routes<R>(f: impl FnOnce() -> R) -> (R, Vec<(String, Site)>) {
    struct CaptureGuard;
    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            DIRECT_ROUTE_CAPTURE.with(|capture| *capture.borrow_mut() = None);
        }
    }
    DIRECT_ROUTE_CAPTURE.with(|capture| {
        assert!(capture.borrow().is_none(), "nested direct-route capture");
        *capture.borrow_mut() = Some(Vec::new());
    });
    let _guard = CaptureGuard;
    let result = f();
    let events = DIRECT_ROUTE_CAPTURE.with(|capture| capture.borrow_mut().take().unwrap());
    (result, events)
}

/// Record a `direct_route_violation` event: a write path acquired a writer
/// connection directly (standalone connection or pool mutex), bypassing the
/// `WriterTask` queue, while `PoolConfig::write_queue_enabled` was `true`
/// (ADR-136 D1 gate 6c). Emitted on the degrade path itself — never gated on
/// `write_routing_strict`, since strict mode turns the same condition into a
/// caller-visible error instead of a degrade, so there is no bypass left to
/// report there.
pub(crate) fn emit_direct_route_violation(db: &str, site: Site) {
    #[cfg(test)]
    DIRECT_ROUTE_CAPTURE.with(|capture| {
        if let Some(events) = capture.borrow_mut().as_mut() {
            events.push((db.to_owned(), site));
        }
    });
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "direct_route_violation",
        db: db.to_string(),
        site: Some(site.as_str()),
        phase: None,
        error: None,
        timeout_ms: None,
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// Record a `slow_write` event: a queued write whose whole request span met
/// the slow-write threshold. Emitted before the typed reply wakes the caller,
/// for success or error alike. The compatibility `elapsed_ms`/`queue_depth`
/// fields remain, while the microsecond stage fields distinguish admission,
/// SQLite write-lock acquisition, operation body, and COMMIT (#1849).
pub(crate) fn emit_slow_write(db: &str, stages: &crate::writer_task::WriterStageObservation) {
    #[cfg(test)]
    record_slow_write_capture(db, slow_write_labels_from(stages));
    let Some(handle) = lifecycle::route(db) else {
        return;
    };
    let event = QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "slow_write",
        db: db.to_string(),
        site: None,
        phase: None,
        error: None,
        timeout_ms: None,
        elapsed_ms: Some(stages.total_micros / 1_000),
        queue_depth: Some(stages.queue_depth_at_entry),
        writer_stages: Some(WriterStageFields {
            queue_wait_micros: stages.queue_wait_micros,
            transaction_acquire_micros: stages.transaction_acquire_micros,
            body_micros: stages.body_micros,
            commit_micros: stages.commit_micros,
            total_micros: stages.total_micros,
        }),
        labels: Some(slow_write_labels_from(stages)),
    };
    enqueue(&handle.sender, &handle.dropped, event);
}

/// The bounded labels the sink would write for `stages`, for tests that
/// assert the `slow_write` row shape without reading the NDJSON file.
#[cfg(test)]
pub(crate) fn slow_write_labels_for_test(
    stages: &crate::writer_task::WriterStageObservation,
) -> SlowWriteLabels {
    slow_write_labels_from(stages)
}

/// Project the writer-stage observation's bounded, first-seen-ordered label
/// list onto the row's shape. The observation already carries at most
/// [`crate::writer_task::MAX_WRITE_LABELS`] distinct labels plus its dropped
/// count, so this is a move, not a re-bound. An observation with no labels
/// yields an empty array and no `labels_truncated` key.
fn slow_write_labels_from(stages: &crate::writer_task::WriterStageObservation) -> SlowWriteLabels {
    SlowWriteLabels {
        labels: stages.labels.iter().cloned().collect(),
        labels_truncated: stages.labels_truncated,
    }
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

/// Emit a `timeout` event for busy/locked, or a separate `sqlite_full`
/// escalation for out-of-space. Standalone-writer call sites see the raw
/// driver error here before mapping it to `StorageError`.
pub(crate) fn maybe_emit_busy(db: &str, site: Site, err: &rusqlite::Error) {
    if is_busy_or_locked(err) {
        emit_timeout(db, site, &err.to_string(), None);
    } else {
        maybe_emit_sqlite_full(db, err);
    }
}

/// A typed direct transaction can fail at `BEGIN IMMEDIATE`, before its DML
/// closure runs. Follow the driver source chain at the completed operation
/// boundary so that busy errors from both BEGIN and the body reach the same
/// standalone timeout site exactly once. `execute_direct_transaction` owns
/// SQLITE_FULL escalation, so this helper emits only busy/locked rows.
pub(crate) fn maybe_emit_busy_storage_error(
    db: &str,
    site: Site,
    error: &khive_storage::StorageError,
) {
    let mut cause: &(dyn std::error::Error + 'static) = error;
    loop {
        if let Some(sqlite_error) = cause.downcast_ref::<rusqlite::Error>() {
            if is_busy_or_locked(sqlite_error) {
                emit_timeout(db, site, &sqlite_error.to_string(), None);
            }
            return;
        }
        let Some(source) = cause.source() else {
            return;
        };
        cause = source;
    }
}

#[cfg(test)]
#[path = "timeout_sink_tests.rs"]
mod tests;
