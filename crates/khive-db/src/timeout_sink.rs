//! Append-only NDJSON event sink for writer-timeout diagnostics.
//!
//! Exists so "zero writer-admission timeouts over the last 24h" is a claim
//! that can be checked from a running daemon instead of grepped out of
//! best-effort `tracing` output that most callers on this path never emit.
//! One line per event, written with a single `write(2)` call per line to a
//! file opened `O_APPEND` — concurrent writers never need a shared lock
//! because POSIX `O_APPEND` positions each whole-line write atomically at
//! end-of-file; no two single-syscall writes can interleave.
//!
//! A "zero timeouts" reading from this file is only meaningful alongside
//! continuous `heartbeat` rows: a silent sink (crashed thread, rotated-away
//! file, dead process) looks identical to a healthy quiet one unless the
//! heartbeat cadence is also checked.
//!
//! Fails open by construction: every write here is best-effort. A failure
//! is swallowed into a process-local counter and, at most once per
//! heartbeat interval, an attempt is made to record a `sink_error_summary`
//! row carrying that counter — itself best-effort, never propagated to the
//! caller. Nothing on this path ever blocks, slows, or errors a database
//! caller.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::pool::{ConnectionPool, TEST_HARNESS_ENV};

/// Default liveness cadence. A 24h-zero-timeouts claim is only valid
/// alongside heartbeats at (at least) this cadence — see module docs.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);

const NDJSON_FILE_NAME: &str = "writer_timeouts.ndjson";

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
}

impl Site {
    fn as_str(self) -> &'static str {
        match self {
            Site::PoolAdmission => "pool_admission",
            Site::StandaloneGraph => "standalone:graph",
            Site::StandaloneEvent => "standalone:event",
            Site::StandaloneText => "standalone:text",
            Site::StandaloneSqlBridge => "standalone:sql_bridge",
        }
    }
}

/// Process-global sink, lazily created by the first [`init`] call. Every
/// subsequent [`init`] call (from another pool booting in the same process)
/// is a no-op beyond the `OnceLock` check — one sink, one file, per process.
static SINK: OnceLock<Sink> = OnceLock::new();

/// One JSON line: `{ts_utc, kind, db, site, error, timeout_ms?}`.
/// `site`/`error`/`timeout_ms`/`pid`/`version` are
/// omitted (not null) when not applicable to `kind` — `startup` rows carry
/// `pid`/`version` and no `site`/`error`; `timeout`/`sink_error_summary` rows
/// are the reverse.
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
    kind: &str,
    db: &str,
    site: Option<&str>,
    error: Option<&str>,
    timeout_ms: Option<u64>,
    pid: Option<u32>,
    version: Option<&str>,
) -> String {
    let record = EventRecord {
        ts_utc: now_rfc3339(),
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

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Attempt exactly one `write(2)` of the full line (already newline
/// terminated), optionally followed by `fsync`. `true` only if the whole
/// buffer was accepted in that single call (and, when requested, the fsync
/// also succeeded) — a short write is treated as a failure rather than
/// retried, so two concurrent appends can never interleave mid-line.
fn write_line_best_effort(file: &File, line: &str, fsync: bool) -> bool {
    let buf = line.as_bytes();
    let wrote_all = matches!((&*file).write(buf), Ok(n) if n == buf.len());
    if !wrote_all {
        return false;
    }
    if fsync {
        return file.sync_data().is_ok();
    }
    true
}

fn heartbeat_loop(file: File, db_identity: String, interval: Duration) {
    loop {
        thread::sleep(interval);
        let line = build_line(
            "heartbeat",
            &db_identity,
            None,
            None,
            None,
            Some(std::process::id()),
            Some(env!("CARGO_PKG_VERSION")),
        );
        // Best-effort: a heartbeat write failure has no caller to report to
        // and no shared counter to bump (this thread deliberately holds no
        // reference back to the owning `Sink`, so it never contends with
        // any database write path for so much as an atomic).
        let _ = write_line_best_effort(&file, &line, false);
    }
}

struct Sink {
    /// `None` when no writable log directory could be resolved (fully
    /// disabled — every method below becomes a no-op).
    file: Option<File>,
    /// The resolved NDJSON path. Not read anywhere today (every consumer
    /// wants "append a line", not "where is the file") but cheap to keep for
    /// a future diagnostics surface (e.g. `db_diagnostics`) that would want
    /// to report it.
    #[allow(dead_code)]
    path: Option<PathBuf>,
    /// Count of failed sink writes across this process's lifetime.
    error_count: AtomicU64,
    /// Millis-since-epoch of the last `sink_error_summary` attempt, or `0`
    /// for "never attempted".
    last_summary_attempt_ms: AtomicU64,
    /// Minimum gap, in milliseconds, between `sink_error_summary` attempts —
    /// mirrors this sink's heartbeat cadence.
    error_summary_interval_ms: u64,
}

impl Sink {
    /// Open (creating parent directories as needed) `<dir>/writer_timeouts.ndjson`
    /// for append. Falls back to a disabled (`file: None`) sink on any I/O
    /// error opening the file — construction itself must never fail or
    /// block a pool/backend boot. Deliberately has no side effects beyond
    /// the open (no startup row, no heartbeat thread) — see [`init`] for
    /// why those are sequenced after this sink wins the `OnceLock` race.
    fn open_in_dir(dir: &Path, heartbeat_interval: Duration) -> Self {
        let path = dir.join(NDJSON_FILE_NAME);
        let file = std::fs::create_dir_all(dir)
            .and_then(|_| OpenOptions::new().create(true).append(true).open(&path))
            .ok();

        Sink {
            file,
            path: Some(path),
            error_count: AtomicU64::new(0),
            last_summary_attempt_ms: AtomicU64::new(0),
            error_summary_interval_ms: heartbeat_interval.as_millis().min(u128::from(u64::MAX))
                as u64,
        }
    }

    fn spawn_heartbeat(&self, db_identity: String, interval: Duration) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        if let Ok(hb_file) = file.try_clone() {
            // Detached: a background heartbeat outlives whatever call
            // happened to trigger the sink's first init. Spawn failure (OS
            // resource exhaustion) degrades to "no heartbeats" rather than
            // failing the caller that triggered init.
            let _ = thread::Builder::new()
                .name("khive-writer-timeout-heartbeat".to_string())
                .spawn(move || heartbeat_loop(hb_file, db_identity, interval));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append(
        &self,
        kind: &str,
        db: &str,
        site: Option<&str>,
        error: Option<&str>,
        timeout_ms: Option<u64>,
        pid: Option<u32>,
        version: Option<&str>,
        fsync: bool,
    ) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let line = build_line(kind, db, site, error, timeout_ms, pid, version);
        if !write_line_best_effort(file, &line, fsync) {
            self.record_error();
        }
    }

    fn record_startup(&self, db: &str) {
        self.append(
            "startup",
            db,
            None,
            None,
            None,
            Some(std::process::id()),
            Some(env!("CARGO_PKG_VERSION")),
            false,
        );
    }

    /// fsync's — timeout rows are rare and already on a multi-second error
    /// path, so the extra durability cost is negligible.
    fn record_timeout(&self, db: &str, site: Site, error: &str, timeout_ms: Option<u64>) {
        self.append(
            "timeout",
            db,
            Some(site.as_str()),
            Some(error),
            timeout_ms,
            None,
            None,
            true,
        );
    }

    fn record_error(&self) {
        let count = self.error_count.fetch_add(1, Ordering::Relaxed) + 1;
        self.maybe_emit_error_summary(count);
    }

    fn maybe_emit_error_summary(&self, count: u64) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let now = now_ms();
        let last = self.last_summary_attempt_ms.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < self.error_summary_interval_ms {
            return;
        }
        if self
            .last_summary_attempt_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // Another thread already claimed this window.
            return;
        }
        let message = format!("sink write failures since last summary: {count}");
        let line = build_line(
            "sink_error_summary",
            "-",
            None,
            Some(&message),
            None,
            None,
            None,
        );
        // Deliberately bypasses `append`/`record_error`: a persistently
        // broken sink must never recurse into itself trying to report its
        // own brokenness.
        let _ = write_line_best_effort(file, &line, false);
    }

    #[cfg(test)]
    fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
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
/// neither resolution has anywhere to point (no `db_parent`, e.g. an
/// in-memory pool, and the primary resolution is unavailable too) — the
/// sink then stays disabled for this process.
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

/// Initialize the process-global sink on first *successful* call; every
/// later call (from another pool booting in the same process) is a cheap
/// no-op once a sink is in place. `db_parent` is the booting pool's database
/// file's parent directory (`None` for an in-memory pool); `db_identity`
/// names that pool in the `startup` row.
///
/// Deliberately does NOT claim the `OnceLock` slot when `resolve_log_dir`
/// has nothing to point at (e.g. an in-memory pool with no `HOME` and no
/// override) — claiming it with a permanently-disabled sink would lock out
/// every later pool in the process that *could* have resolved a directory,
/// for the lifetime of the process. Leaving the slot open lets whichever
/// pool boots first with a resolvable directory be the one that wins it.
pub(crate) fn init(db_parent: Option<&Path>, db_identity: &str) {
    if SINK.get().is_some() {
        return;
    }
    let Some(dir) = resolve_log_dir(db_parent) else {
        return;
    };
    let heartbeat_interval = heartbeat_interval_from_env();
    let sink = Sink::open_in_dir(&dir, heartbeat_interval);
    if SINK.set(sink).is_ok() {
        // Only the thread that actually won the `OnceLock` race writes the
        // startup row and spawns the heartbeat — otherwise a concurrent
        // race between two pools booting at once could write two startup
        // rows and spawn two heartbeat threads for what `OnceLock`
        // guarantees is one logical sink.
        let sink = SINK.get().expect("just set");
        if sink.file.is_some() {
            sink.record_startup(db_identity);
            sink.spawn_heartbeat(db_identity.to_string(), heartbeat_interval);
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

/// Record a writer-admission or busy/locked timeout event. A no-op if the
/// sink was never initialized (no pool has booted yet in this process) or
/// is disabled (no writable log directory resolvable).
pub(crate) fn emit_timeout(db: &str, site: Site, error: &str, timeout_ms: Option<u64>) {
    if let Some(sink) = SINK.get() {
        sink.record_timeout(db, site, error, timeout_ms);
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
    use std::sync::Arc;
    use std::thread;

    /// Test-only mirror of what [`init`] does once it has won the
    /// `OnceLock` race: open the file, then (only if that succeeded) write
    /// the startup row and spawn the heartbeat thread. Kept out of
    /// `Sink::open_in_dir` itself so production `init` can sequence those
    /// side effects strictly after claiming the global slot (see `init`'s
    /// doc comment) — tests that want an armed, standalone `Sink` go
    /// through this helper instead of reaching into that production
    /// sequencing.
    fn open_and_arm(dir: &Path, db_identity: &str, heartbeat_interval: Duration) -> Sink {
        let sink = Sink::open_in_dir(dir, heartbeat_interval);
        if sink.file.is_some() {
            sink.record_startup(db_identity);
            sink.spawn_heartbeat(db_identity.to_string(), heartbeat_interval);
        }
        sink
    }

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
    fn startup_row_present_and_heartbeat_arrives() {
        let dir = tempfile::tempdir().unwrap();
        let sink = open_and_arm(dir.path(), "test-db-liveness", Duration::from_millis(20));

        let ndjson_path = dir.path().join(NDJSON_FILE_NAME);
        let initial = std::fs::read_to_string(&ndjson_path).unwrap();
        assert!(
            initial.contains("\"kind\":\"startup\""),
            "expected a startup row immediately after open_in_dir, got: {initial}"
        );
        assert!(initial.contains("\"db\":\"test-db-liveness\""));

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let contents = std::fs::read_to_string(&ndjson_path).unwrap();
            if contents.contains("\"kind\":\"heartbeat\"") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("no heartbeat row appeared within 5s of a 20ms interval: {contents}");
            }
            thread::sleep(Duration::from_millis(10));
        }

        drop(sink);
    }

    #[test]
    fn fail_open_on_unresolvable_directory_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file occupying the path a directory needs makes
        // `create_dir_all` fail — a portable way to simulate "unwritable".
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let bogus_log_dir = blocker.join("logs");

        let sink = open_and_arm(&bogus_log_dir, "test-db-fail-open", Duration::from_secs(60));
        assert!(
            sink.file.is_none(),
            "open must degrade to disabled, not panic"
        );

        // Every recording call must still return normally on a disabled sink.
        sink.record_timeout("test-db-fail-open", Site::PoolAdmission, "boom", Some(5));
        sink.record_startup("test-db-fail-open");
    }

    #[test]
    fn fail_open_on_write_failure_counts_the_error_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NDJSON_FILE_NAME);
        std::fs::write(&path, b"").unwrap();
        // Opened read-only: every `write(2)` against this handle returns
        // `EBADF`, simulating a sink write failure without touching the
        // underlying fd's ownership (an out-of-band `close(2)` on a live
        // `File` trips Rust's I/O-safety abort, which is not the failure
        // mode under test here — a normal, ordinary I/O error is).
        let read_only_file = OpenOptions::new().read(true).open(&path).unwrap();
        let sink = Sink {
            file: Some(read_only_file),
            path: Some(path),
            error_count: AtomicU64::new(0),
            last_summary_attempt_ms: AtomicU64::new(0),
            error_summary_interval_ms: 60_000,
        };

        sink.record_timeout("test-db-fail-open-2", Site::PoolAdmission, "boom", Some(5));

        assert_eq!(
            sink.error_count(),
            1,
            "a write against a read-only handle must be counted, not panic or propagate"
        );
    }

    #[test]
    fn concurrent_append_produces_intact_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(open_and_arm(
            dir.path(),
            "test-db-concurrent",
            Duration::from_secs(60),
        ));

        const THREADS: usize = 32;
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let sink = Arc::clone(&sink);
                thread::spawn(move || {
                    sink.record_timeout(
                        "test-db-concurrent",
                        Site::StandaloneGraph,
                        &format!("contention-{i}"),
                        Some(i as u64),
                    );
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let ndjson_path = dir.path().join(NDJSON_FILE_NAME);
        let contents = std::fs::read_to_string(&ndjson_path).unwrap();
        let timeout_lines: Vec<&str> = contents
            .lines()
            .filter(|l| l.contains("\"kind\":\"timeout\""))
            .collect();
        assert_eq!(
            timeout_lines.len(),
            THREADS,
            "expected one intact line per thread"
        );
        for line in &timeout_lines {
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
