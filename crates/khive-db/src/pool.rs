//! Connection pool for SQLite: one exclusive writer, N concurrent readers.
use crossbeam_queue::ArrayQueue;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::SqliteError;
use crate::writer_task::WriterTaskHandle;
use khive_storage::error::StorageError;
use khive_storage::tx_registry::{DbIdentity, TxOrigin};

const CACHE_SIZE_KIB: &str = "-65536";
const MMAP_SIZE_BYTES: &str = "1073741824";
const DEFAULT_READER_CAP: usize = 8;

const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: u32 = 4000;
const DEFAULT_JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864; // 64 MiB
const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 256;

pub(crate) const TEST_HARNESS_ENV: &str = "KHIVE_TEST_HARNESS";

/// Configuration for the connection pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Database path. None = in-memory (pool degrades to single connection).
    pub path: Option<PathBuf>,
    /// Number of reader connections (default: min(num_cpus, 8)).
    pub max_readers: usize,
    /// WAL mode (must be true for pooling to work; default: true).
    pub wal_mode: bool,
    /// Busy timeout per connection (default: 30s).
    ///
    /// Overridable via `KHIVE_BUSY_TIMEOUT_SECS`.
    pub busy_timeout: Duration,
    /// Time to wait for a reader connection before returning an error (default: 5s).
    ///
    /// Overridable via `KHIVE_CHECKOUT_TIMEOUT_SECS`.
    pub checkout_timeout: Duration,
    /// Number of WAL pages that triggers an automatic checkpoint.
    ///
    /// Maps to `PRAGMA wal_autocheckpoint`. The default (4000 pages, ~16 MiB
    /// at SQLite's default 4 KiB page size) matches the pre-config behaviour.
    ///
    /// Overridable via `KHIVE_WAL_AUTOCHECKPOINT_PAGES`.
    pub wal_autocheckpoint_pages: u32,
    /// Maximum WAL journal size in bytes before SQLite resets the WAL.
    ///
    /// Maps to `PRAGMA journal_size_limit`. Default: 64 MiB.
    ///
    /// Overridable via `KHIVE_JOURNAL_SIZE_LIMIT_BYTES`.
    pub journal_size_limit_bytes: i64,
    /// Open the database read-only (default: false).
    ///
    /// When true, the pool's writer connection is opened with
    /// `SQLITE_OPEN_READ_ONLY` (no `SQLITE_OPEN_CREATE`, so a missing path is
    /// rejected instead of created) and `PRAGMA query_only = ON` is set on
    /// every connection that can execute SQL. Reader connections are already
    /// opened read-only regardless of this flag.
    pub read_only: bool,
    /// Route migrated store write paths through the single-writer
    /// `WriterTask` channel (ADR-067 Component A) instead of the legacy
    /// per-call pool-mutex/standalone-connection path. Off by default.
    ///
    /// Slice 1 wires exactly one path (`SqlEntityStore::upsert_entities`)
    /// behind this flag; enabling it does not yet claim ADR-067's
    /// single-writer guarantee — other write paths still open their own
    /// writers until later slices migrate them.
    ///
    /// `None` means the caller expressed no preference: [`ConnectionPool::new`]
    /// resolves it once `path` is known, defaulting to `true` for file-backed
    /// pools and `false` for in-memory ones. `Some(_)` is an explicit
    /// preference and always wins, in both directions, over that default.
    ///
    /// Overridable via `KHIVE_WRITE_QUEUE` (`"1"` or `"true"`,
    /// case-insensitive, sets `Some(true)`; any other value sets `Some(false)`;
    /// unset leaves it `None`).
    pub write_queue_enabled: Option<bool>,
    /// Bounded channel capacity for the `WriterTask` write queue.
    ///
    /// Overridable via `KHIVE_WRITE_QUEUE_CAPACITY`. Default: 256 pending
    /// operations (ADR-067 Component A recommended default).
    pub write_queue_capacity: usize,
    /// ADR-136 D1: when `true`, every write path that would otherwise
    /// silently degrade to the legacy pool-mutex/standalone-connection path
    /// on a missing or failed `WriterTask` handle instead returns an error.
    /// Exercises the completed routing (ADR-135 F2's strict-routing
    /// precondition) without changing behavior for callers that never set
    /// the env var.
    ///
    /// Overridable via `KHIVE_WRITE_ROUTING` (value `"strict"`,
    /// case-insensitive; anything else, or unset, leaves this `false`).
    pub write_routing_strict: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            path: None,
            max_readers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(1, DEFAULT_READER_CAP),
            wal_mode: true,
            busy_timeout: Duration::from_secs(
                std::env::var("KHIVE_BUSY_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(30),
            ),
            checkout_timeout: Duration::from_secs(
                std::env::var("KHIVE_CHECKOUT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5),
            ),
            wal_autocheckpoint_pages: std::env::var("KHIVE_WAL_AUTOCHECKPOINT_PAGES")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(DEFAULT_WAL_AUTOCHECKPOINT_PAGES),
            journal_size_limit_bytes: std::env::var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(DEFAULT_JOURNAL_SIZE_LIMIT_BYTES),
            read_only: false,
            write_queue_enabled: std::env::var("KHIVE_WRITE_QUEUE")
                .ok()
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true")),
            write_queue_capacity: std::env::var("KHIVE_WRITE_QUEUE_CAPACITY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(DEFAULT_WRITE_QUEUE_CAPACITY),
            write_routing_strict: std::env::var("KHIVE_WRITE_ROUTING")
                .map(|v| v.eq_ignore_ascii_case("strict"))
                .unwrap_or(false),
        }
    }
}

/// Prevent Cargo-launched tests and test subprocesses from opening the
/// operator's default data tree in every build profile. Activation is solely
/// the runtime `KHIVE_TEST_HARNESS=1` marker; production/installed binaries do
/// not receive that workspace Cargo environment.
///
/// There is deliberately no environment override: any inheritable escape
/// hatch set for one Cargo invocation leaks into the next `cargo test` in the
/// same shell and re-opens the store the guard exists to protect. A deliberate
/// session against the real store runs the built binary directly (for example
/// `target/release/...` or an installed binary), which never receives the
/// workspace Cargo environment and therefore never trips this guard.
/// Existing path ancestors are canonicalized before comparison, resolving
/// traversal, symlinks, and filesystem-provided case (including APFS case
/// folding). Missing trailing components remain lexical because they have no
/// filesystem identity yet. SQLite URI paths are rejected rather than trying
/// to reproduce SQLite's URI normalization rules.
fn refuse_home_data_store_in_tests(config: &PoolConfig) -> Result<(), SqliteError> {
    if std::env::var(TEST_HARNESS_ENV).as_deref() != Ok("1") {
        return Ok(());
    }

    let Some(path) = config.path.as_deref() else {
        return Ok(());
    };
    if path
        .as_os_str()
        .as_encoded_bytes()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"file:"))
    {
        return Err(SqliteError::InvalidData(format!(
            "test harness refused SQLite URI database path {}; use a filesystem path outside \
             HOME/.khive (deliberate sessions against a real store run the built binary \
             directly, outside the Cargo test environment)",
            path.display()
        )));
    }

    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let canonical_path = canonicalize_deepest_existing(path)?;
    let canonical_home_data_dir =
        canonicalize_deepest_existing(&PathBuf::from(home).join(".khive"))?;
    if canonical_path.starts_with(&canonical_home_data_dir) {
        return Err(SqliteError::InvalidData(format!(
            "test harness refused to open SQLite database under HOME/.khive: {} \
             (deliberate sessions against a real store run the built binary directly, \
             outside the Cargo test environment)",
            canonical_path.display()
        )));
    }
    Ok(())
}

fn canonicalize_deepest_existing(path: &Path) -> Result<PathBuf, SqliteError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(SqliteError::Io)?.join(path)
    };

    for ancestor in absolute.ancestors() {
        match fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                let missing = absolute.strip_prefix(ancestor).map_err(|error| {
                    SqliteError::InvalidData(format!(
                        "failed to preserve missing path components for {}: {error}",
                        absolute.display()
                    ))
                })?;
                canonical.push(missing);
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SqliteError::InvalidData(format!(
                    "failed to canonicalize database path ancestor {}: {error}",
                    ancestor.display()
                )));
            }
        }
    }

    Err(SqliteError::InvalidData(format!(
        "database path has no canonicalizable ancestor: {}",
        absolute.display()
    )))
}

/// A read-write connection pool for SQLite.
///
/// Architecture:
/// - 1 writer connection protected by a Mutex (exclusive access)
/// - N reader connections in a lock-free queue (concurrent access)
/// - All connections share the same database file in WAL mode
///
/// For in-memory databases, or when WAL mode is disabled/unavailable, the pool
/// degrades to single-connection mode and routes all operations through the
/// writer connection.
pub struct ConnectionPool {
    writer: Arc<Mutex<Connection>>,
    /// Process-local writer acquisition counters shared with the pool's
    /// lifetime-owned writer task. Keeping the counters at the actual
    /// acquisition boundaries means new verbs inherit instrumentation without
    /// per-verb classification (ADR-133 D8 / issue #1389).
    writer_acquisition_counters: Arc<WriterAcquisitionCounters>,
    readers: ArrayQueue<Connection>,
    max_readers: usize,
    config: PoolConfig,
    /// The pool-wide ADR-067 Component A writer task, spawned lazily and at
    /// most once per pool (per DB file) via [`Self::writer_task_handle`] —
    /// see that method's doc comment for why this lives here rather than on
    /// each store.
    writer_task: OnceLock<Option<WriterTaskHandle>>,
    /// The `tokio::spawn` JoinHandle of the writer task above, stored by
    /// [`crate::writer_task::spawn`] so short-lived callers (batch CLI
    /// paths) can await the task's exit — and therefore its connection's
    /// close-time WAL checkpoint — before treating the database file state
    /// as settled. Long-running callers never take it; dropping an untaken
    /// JoinHandle detaches the task, which is exactly the pre-existing
    /// behavior.
    writer_task_join: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// This pool's ADR-091 backend-scoped attribution origin, minted exactly
    /// once at construction (see [`mint_db_identity`]): `Database(_)` for a
    /// file-backed pool, `Memory` for an in-memory pool. Every
    /// `tx_registry::register_scoped` call site in this crate reaches its
    /// origin through [`Self::origin`] rather than re-deriving it.
    origin: TxOrigin,
    /// The canonical path `origin`'s `DbIdentity` was minted from, `None` for
    /// an in-memory pool. `DbIdentity` is deliberately opaque (no path
    /// accessor) — filesystem consumers that need the actual path (sidecar
    /// derivation) use this, the same canonical value the identity was
    /// minted from, via [`Self::canonical_path`].
    identity_path: Option<PathBuf>,
    /// Test-only instrumentation: counts how many times the writer-task
    /// init closure actually ran. Must never exceed 1 per pool no matter how
    /// many stores are constructed over it — that is the invariant
    /// `OnceLock::get_or_init` exists to guarantee, and what
    /// `pool.rs`'s and `entity_tests.rs`'s one-writer-per-pool tests assert.
    #[cfg(test)]
    writer_task_spawn_count: std::sync::atomic::AtomicUsize,
}

enum ReaderLease<'pool> {
    Pooled(Connection),
    Shared(parking_lot::MutexGuard<'pool, Connection>),
}

/// A reader connection checked out from the pool.
/// Returns the connection to the pool on drop.
pub struct ReaderGuard<'pool> {
    lease: Option<ReaderLease<'pool>>,
    pool: &'pool ConnectionPool,
}

impl<'pool> ReaderGuard<'pool> {
    /// Access the connection.
    pub fn conn(&self) -> &Connection {
        match self
            .lease
            .as_ref()
            .expect("reader guard missing connection")
        {
            ReaderLease::Pooled(conn) => conn,
            ReaderLease::Shared(guard) => guard,
        }
    }
}

impl<'pool> Deref for ReaderGuard<'pool> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn()
    }
}

impl<'pool> Drop for ReaderGuard<'pool> {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };

        match lease {
            ReaderLease::Pooled(conn) => self.pool.return_reader(conn),
            ReaderLease::Shared(_guard) => {}
        }
    }
}

/// A writer connection checked out from the pool.
/// The Mutex ensures only one writer at a time.
pub struct WriterGuard<'pool> {
    guard: parking_lot::MutexGuard<'pool, Connection>,
    /// The origin (ADR-091 backend-scoped attribution) of the pool this
    /// guard was checked out from, carried so `transaction` can register its
    /// span with the correct origin without holding a `&ConnectionPool`.
    origin: TxOrigin,
}

/// Process-local monotonic counters for every instrumented writer acquisition
/// boundary owned by one [`ConnectionPool`].
///
/// The aggregate `acquisitions` is the saturating sum of its three explicit
/// connection classes. Infrastructure-only opens (the diagnostics PASSIVE
/// probe, the writer task's one-time lifetime connection, and the checkpoint
/// task's dedicated long-lived connection) are excluded; zero-wait
/// maintenance probes also remain outside these request-traffic counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterAcquisitionSnapshot {
    /// Successful acquisitions across pooled, standalone, and writer-task
    /// connection classes.
    pub acquisitions: u64,
    /// Successful finite-wait pool-mutex writer checkouts.
    pub pooled_acquisitions: u64,
    /// Successful per-operation standalone writer connection opens.
    pub standalone_acquisitions: u64,
    /// Successful writer-task ownership acquisitions (one per dequeued
    /// top-level request or successful `BEGIN IMMEDIATE`).
    pub writer_task_acquisitions: u64,
    /// Finite-wait pool writer checkouts that exhausted their deadline.
    pub timeouts: u64,
}

/// Atomics backing [`WriterAcquisitionSnapshot`]. The writer task retains an
/// `Arc` after spawn so its per-request acquisition site can update the same
/// pool-scoped snapshot without retaining the whole pool.
#[derive(Debug, Default)]
pub(crate) struct WriterAcquisitionCounters {
    pooled_acquisitions: AtomicU64,
    standalone_acquisitions: AtomicU64,
    writer_task_acquisitions: AtomicU64,
    pooled_timeouts: AtomicU64,
}

impl WriterAcquisitionCounters {
    pub(crate) fn record_writer_task_acquisition(&self) {
        self.writer_task_acquisitions
            .fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> WriterAcquisitionSnapshot {
        let pooled_acquisitions = self.pooled_acquisitions.load(Ordering::Relaxed);
        let standalone_acquisitions = self.standalone_acquisitions.load(Ordering::Relaxed);
        let writer_task_acquisitions = self.writer_task_acquisitions.load(Ordering::Relaxed);
        WriterAcquisitionSnapshot {
            acquisitions: pooled_acquisitions
                .saturating_add(standalone_acquisitions)
                .saturating_add(writer_task_acquisitions),
            pooled_acquisitions,
            standalone_acquisitions,
            writer_task_acquisitions,
            timeouts: self.pooled_timeouts.load(Ordering::Relaxed),
        }
    }
}

impl<'pool> WriterGuard<'pool> {
    /// Returns a shared reference to the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.guard
    }

    /// Returns a mutable reference to the underlying connection.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }

    /// Execute a write transaction.
    /// Wraps the closure in BEGIN IMMEDIATE ... COMMIT.
    pub fn transaction<F, R>(&self, f: F) -> Result<R, SqliteError>
    where
        F: FnOnce(&Connection) -> Result<R, SqliteError>,
    {
        self.guard.execute_batch("BEGIN IMMEDIATE")?;
        let _tx_handle = khive_storage::tx_registry::register_scoped(
            Some("writer_guard_tx".to_string()),
            self.origin.clone(),
        );

        match f(&self.guard) {
            Ok(result) => {
                if let Err(err) = self.guard.execute_batch("COMMIT") {
                    let _ = self.guard.execute_batch("ROLLBACK");
                    return Err(err.into());
                }
                Ok(result)
            }
            Err(err) => {
                let _ = self.guard.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}

impl<'pool> Deref for WriterGuard<'pool> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn()
    }
}

impl<'pool> DerefMut for WriterGuard<'pool> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn_mut()
    }
}

impl ConnectionPool {
    /// Create a new connection pool.
    ///
    /// Opens 1 writer + N reader connections to the same database when pooling
    /// is enabled. All connections are configured consistently (busy timeout,
    /// foreign keys, cache, mmap, temp store). For in-memory databases, or when
    /// WAL is disabled or unavailable, the pool falls back to single-connection
    /// mode.
    pub fn new(config: PoolConfig) -> Result<Self, SqliteError> {
        refuse_home_data_store_in_tests(&config)?;

        // Resolve "no preference" (`None`) now that `path` is known: on for
        // file-backed pools, off for in-memory ones. An explicit `Some(_)`
        // preference is left untouched and always wins.
        let mut config = config;
        config.write_queue_enabled =
            Some(config.write_queue_enabled.unwrap_or(config.path.is_some()));

        let writer = open_writer_connection(&config)?;
        let wal_enabled = configure_writer_connection(&writer, &config)?;
        let max_readers = effective_reader_count(&config, wal_enabled);

        let readers = ArrayQueue::new(max_readers.max(1));

        let (origin, identity_path) = match config.path.as_ref() {
            Some(path) => {
                let (identity, canonical) = mint_db_identity(path)?;
                (TxOrigin::Database(identity), Some(canonical))
            }
            None => (TxOrigin::Memory, None),
        };

        let pool = Self {
            writer: Arc::new(Mutex::new(writer)),
            writer_acquisition_counters: Arc::new(WriterAcquisitionCounters::default()),
            readers,
            max_readers,
            config,
            writer_task: OnceLock::new(),
            writer_task_join: Mutex::new(None),
            origin,
            identity_path,
            #[cfg(test)]
            writer_task_spawn_count: std::sync::atomic::AtomicUsize::new(0),
        };

        for _ in 0..pool.max_readers {
            let conn = pool.open_reader_connection()?;
            pool.readers
                .push(conn)
                .expect("reader queue must have capacity during pool initialization");
        }

        // Best-effort, process-global: the first pool to boot in this
        // process resolves the writer-timeout sink's log directory and
        // spawns its heartbeat thread; every later pool's call here is a
        // cheap no-op. See `crate::timeout_sink` module docs.
        crate::timeout_sink::init(
            pool.canonical_path().and_then(Path::parent),
            &crate::timeout_sink::db_label(&pool),
        );

        Ok(pool)
    }

    /// Check out a reader connection.
    ///
    /// Tries to pop from the lock-free queue. If empty, spins briefly then
    /// waits with exponential backoff up to `checkout_timeout`.
    ///
    /// # Deadlock Warning
    ///
    /// In degraded mode (WAL unavailable, `max_readers == 0`), this method locks
    /// the writer mutex. If the calling thread already holds a [`WriterGuard`],
    /// this will deadlock (parking_lot `Mutex` is not reentrant). Never call
    /// `reader()` while holding a `WriterGuard` on the same pool.
    pub fn reader(&self) -> Result<ReaderGuard<'_>, SqliteError> {
        if self.max_readers == 0 {
            return Ok(ReaderGuard {
                lease: Some(ReaderLease::Shared(self.writer.lock())),
                pool: self,
            });
        }

        let started = Instant::now();
        let mut attempt = 0u32;

        loop {
            if let Some(conn) = self.readers.pop() {
                return Ok(ReaderGuard {
                    lease: Some(ReaderLease::Pooled(conn)),
                    pool: self,
                });
            }

            if started.elapsed() >= self.config.checkout_timeout {
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }

            match attempt {
                0..=7 => {
                    let spins = 1usize << attempt;
                    for _ in 0..spins {
                        std::hint::spin_loop();
                    }
                }
                8..=15 => thread::yield_now(),
                _ => {
                    let remaining = self
                        .config
                        .checkout_timeout
                        .saturating_sub(started.elapsed());
                    let sleep = Duration::from_micros(50 * (1u64 << (attempt - 16).min(6)));
                    thread::sleep(sleep.min(remaining).min(Duration::from_millis(2)));
                }
            }

            attempt = attempt.saturating_add(1);
        }
    }

    /// Check out the writer connection.
    ///
    /// Waits up to `checkout_timeout` for the writer Mutex and returns
    /// `Err(SqliteError::WriterPoolCheckoutTimeout)` if the timeout is
    /// exceeded.
    pub fn writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
        let Some(guard) = self.writer.try_lock_for(self.config.checkout_timeout) else {
            self.writer_acquisition_counters
                .pooled_timeouts
                .fetch_add(1, Ordering::Relaxed);
            let message = format!(
                "timed out after {:?} waiting for sqlite writer connection",
                self.config.checkout_timeout
            );
            crate::timeout_sink::emit_timeout(
                &crate::timeout_sink::db_label(self),
                crate::timeout_sink::Site::PoolAdmission,
                &message,
                Some(
                    self.config
                        .checkout_timeout
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64,
                ),
            );
            return Err(SqliteError::WriterPoolCheckoutTimeout {
                timeout: self.config.checkout_timeout,
            });
        };
        self.writer_acquisition_counters
            .pooled_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        Ok(WriterGuard {
            guard,
            origin: self.origin(),
        })
    }

    /// Non-panicking writer checkout.
    ///
    /// Returns `Err` on timeout instead of panicking. Use this in request
    /// handlers where a 500 is preferable to crashing the process.
    pub fn try_writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
        self.writer()
    }

    /// Zero-wait writer checkout for background tasks.
    ///
    /// Uses `try_lock()` (no timeout, no spin) — returns `Err` immediately when
    /// any other caller holds the writer Mutex. Background tasks (e.g. the WAL
    /// checkpoint task) MUST use this instead of `try_writer` so that a busy
    /// writer causes the background task to skip its current tick rather than
    /// stalling for up to `checkout_timeout` (default 5s) while write traffic
    /// is in progress.
    pub fn try_writer_nowait(&self) -> Result<WriterGuard<'_>, SqliteError> {
        let guard = self.writer.try_lock().ok_or_else(|| {
            SqliteError::InvalidData(
                "writer connection busy (checkpoint skipped this tick)".to_string(),
            )
        })?;
        Ok(WriterGuard {
            guard,
            origin: self.origin(),
        })
    }

    /// Snapshot all instrumented writer acquisition outcomes since this pool
    /// was constructed.
    pub fn writer_acquisition_snapshot(&self) -> WriterAcquisitionSnapshot {
        self.writer_acquisition_counters.snapshot()
    }

    /// Clone the pool-scoped counter set for the lifetime-owned writer task.
    pub(crate) fn writer_acquisition_counters(&self) -> Arc<WriterAcquisitionCounters> {
        Arc::clone(&self.writer_acquisition_counters)
    }

    /// Get the current number of available reader connections.
    pub fn available_readers(&self) -> usize {
        self.readers.len()
    }

    /// Get the total number of reader connections in the pool.
    pub fn max_readers(&self) -> usize {
        self.max_readers
    }

    /// Return the pool configuration.
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// This pool's ADR-091 backend-scoped attribution origin (ADR-091,
    /// backend-scoped WAL-pin attribution design note): `Database(_)` for a
    /// file-backed pool, `Memory` for an in-memory pool. Every
    /// `tx_registry::register_scoped` call site threaded in this crate
    /// passes this value as the span's origin.
    pub fn origin(&self) -> TxOrigin {
        self.origin.clone()
    }

    /// The canonical path this pool's `origin()` identity was minted from,
    /// `None` for an in-memory pool. `DbIdentity` has no path accessor by
    /// design; sidecar derivation and other filesystem consumers use this —
    /// the same canonical value the identity was minted from — instead of
    /// re-deriving a path from the raw configured one.
    pub fn canonical_path(&self) -> Option<&Path> {
        self.identity_path.as_deref()
    }

    /// Return the pool-wide ADR-067 Component A writer task, spawning it
    /// lazily on first access if `PoolConfig::write_queue_enabled` is set.
    /// Exactly one writer task exists per `ConnectionPool` (per DB file); see
    /// crates/khive-db/docs/api/pool.md#connectionpoolwriter_task_handle--single-writer-task-rationale
    /// for why a per-store writer task would defeat the single-writer
    /// guarantee.
    ///
    /// Returns `Ok(None)` if the flag is off, or if the writer task failed to
    /// spawn for a reason other than a missing runtime (for example, an
    /// in-memory pool has no standalone-connection support) — callers fall
    /// back to the legacy pool-mutex write path in either case. A spawn
    /// failure is logged once here (at first access), not once per store.
    ///
    /// Returns `Err(StorageError::WriterTaskNoRuntime)` instead of panicking
    /// when `write_queue_enabled` is set but this is the first access and no
    /// Tokio runtime is available on the calling thread (checked via
    /// [`tokio::runtime::Handle::try_current`]) — spawning the writer task
    /// requires `tokio::spawn`, which panics outside a runtime. Callers that
    /// already treat a missing writer task as best-effort (construction-time
    /// degrade to the legacy path, matching slice 1's documented policy) can
    /// collapse this into `None` with `.ok().flatten()`; callers that need to
    /// fail loud on a genuine misconfiguration (write queue requested but no
    /// runtime to run it on) can propagate the `Err` directly.
    pub fn writer_task_handle(&self) -> Result<Option<WriterTaskHandle>, StorageError> {
        // Pinned invariant: `ConnectionPool::new` resolves `None` ("no
        // preference") to a concrete `Some(..)` once `path` is known, so by
        // the time any reader sees this config it is resolved. A `None` here
        // would mean a future construction path skipped that resolution and
        // would silently read as disabled — trip loudly in debug instead.
        debug_assert!(
            self.config.write_queue_enabled.is_some(),
            "write_queue_enabled must be resolved to Some(..) by ConnectionPool::new \
             before any writer_task_handle read"
        );
        if !self.config.write_queue_enabled.unwrap_or(false) {
            return Ok(None);
        }
        // Fast path: already resolved (spawned, degraded, or off) by an
        // earlier call — no need to re-check the runtime.
        if let Some(existing) = self.writer_task.get() {
            return Ok(existing.clone());
        }
        // Not yet initialized and the flag is on: spawning requires
        // `tokio::spawn`, which panics outside a runtime context. Check
        // first and fail loud with a typed error instead.
        if tokio::runtime::Handle::try_current().is_err() {
            return Err(StorageError::WriterTaskNoRuntime);
        }
        Ok(self
            .writer_task
            .get_or_init(|| {
                #[cfg(test)]
                self.writer_task_spawn_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                match crate::writer_task::spawn(self, self.config.write_queue_capacity) {
                    Ok(handle) => Some(handle),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "KHIVE_WRITE_QUEUE=1 but the writer task failed to spawn; \
                             writes fall back to the pool-mutex path"
                        );
                        None
                    }
                }
            })
            .clone())
    }

    /// Test-only: how many times the writer-task init closure actually ran.
    /// Must be at most 1 for the pool's whole lifetime, regardless of how
    /// many times [`Self::writer_task_handle`] is called or how many stores
    /// are constructed over this pool.
    #[cfg(test)]
    pub(crate) fn writer_task_spawn_count(&self) -> usize {
        self.writer_task_spawn_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Record the writer task's `tokio::spawn` JoinHandle. Called exactly
    /// once, by [`crate::writer_task::spawn`], immediately after spawning —
    /// the same `writer_task` OnceLock init that makes spawn at-most-once
    /// per pool makes this write at-most-once per pool.
    ///
    /// First-wins: if a handle is already stored, the existing handle is
    /// kept and the new one is dropped (dropping a `JoinHandle` detaches
    /// its task without cancelling it). A second store violates the
    /// at-most-once contract and trips the debug_assert in debug builds;
    /// release builds keep the first handle rather than silently swapping
    /// the drain owner out from under whichever caller already took it.
    pub(crate) fn set_writer_task_join(&self, join: tokio::task::JoinHandle<()>) {
        let mut slot = self.writer_task_join.lock();
        let first_store = slot.is_none();
        debug_assert!(
            first_store,
            "writer task JoinHandle stored twice; the writer_task OnceLock is \
             supposed to make spawn at-most-once per pool"
        );
        if first_store {
            *slot = Some(join);
        }
    }

    /// Take the writer task's JoinHandle, if a writer task was spawned and
    /// the handle has not already been taken.
    ///
    /// Intended for short-lived batch callers that drop every
    /// [`WriterTaskHandle`] clone (closing the queue) and then need to await
    /// the task's exit before treating the database file as settled: the
    /// task's connection close fires SQLite's close-time WAL checkpoint, so
    /// until the task exits the file bytes can still move after the caller's
    /// last write returned.
    ///
    /// One-shot: `None` means either the write queue never spawned
    /// (disabled, or spawn degraded) or another caller already took the
    /// handle — in both cases there is nothing further to await here.
    /// Exactly one subsystem may own the drain: the single caller that
    /// receives `Some(_)` is the sole owner of the task-exit await (and of
    /// the close-time WAL checkpoint that settles the database file); every
    /// later caller receives `None` and must not arrange its own await.
    pub fn take_writer_task_join(&self) -> Option<tokio::task::JoinHandle<()>> {
        self.writer_task_join.lock().take()
    }

    /// Compatibility method: returns the writer connection wrapped in `Arc<Mutex>`.
    ///
    /// WARNING: This exists only for backward compatibility with code that
    /// calls `store.conn()`. New code should use `reader()` and `writer()`.
    pub fn legacy_conn(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.writer)
    }

    fn open_reader_connection(&self) -> Result<Connection, SqliteError> {
        let path = self
            .config
            .path
            .as_ref()
            .expect("reader connections require a file-backed database");
        open_reader_connection(path, &self.config)
    }

    /// Open a standalone read-write connection to the same file-backed database.
    ///
    /// Stores whose trait methods take `Send + 'static` closures (executed via
    /// `spawn_blocking`) cannot hold the pooled `WriterGuard`'s `MutexGuard`
    /// across the call — it opens an independent connection instead. This
    /// must still honor `PoolConfig::read_only`: opening
    /// `SQLITE_OPEN_READ_WRITE` unconditionally here would let a read-only
    /// backend's graph/event/text stores bypass the flag that the pooled
    /// writer enforces via `query_only`. A fully configured successful open
    /// increments the standalone acquisition class exactly once.
    pub fn open_standalone_writer(&self) -> Result<Connection, SqliteError> {
        let conn = self.open_standalone_writer_untracked()?;
        self.writer_acquisition_counters
            .standalone_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        Ok(conn)
    }

    /// Open an infrastructure-owned standalone writer connection without
    /// counting it as one write-operation acquisition.
    ///
    /// Restricted to the diagnostics PASSIVE probe, the writer task's
    /// one-time lifetime connection, and the checkpoint task's dedicated
    /// long-lived connection (opened once at startup and reused across
    /// ticks — see `CheckpointConnection::ensure_open`). Actual file-backed
    /// write paths must call [`Self::open_standalone_writer`] so their
    /// acquisitions are observable.
    pub(crate) fn open_standalone_writer_untracked(&self) -> Result<Connection, SqliteError> {
        let path = self.config.path.as_ref().ok_or_else(|| {
            SqliteError::InvalidData(
                "in-memory databases do not support standalone connections".to_string(),
            )
        })?;

        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "database is read-only: standalone write connections are not permitted".to_string(),
            ));
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(self.config.busy_timeout)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    /// Open a standalone read-only connection to the same file-backed database.
    ///
    /// Companion to `open_standalone_writer` for stores that also need an
    /// independent reader connection outside the pooled reader queue.
    pub fn open_standalone_reader(&self) -> Result<Connection, SqliteError> {
        let path = self.config.path.as_ref().ok_or_else(|| {
            SqliteError::InvalidData(
                "in-memory databases do not support standalone connections".to_string(),
            )
        })?;

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(self.config.busy_timeout)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    fn return_reader(&self, conn: Connection) {
        if self.max_readers == 0 {
            return;
        }

        let conn = if reset_reader_connection(&conn) && reader_connection_is_healthy(&conn) {
            Some(conn)
        } else {
            close_connection_quietly(conn);
            self.open_reader_connection().ok()
        };

        if let Some(conn) = conn {
            if let Err(conn) = self.readers.push(conn) {
                eprintln!(
                    "[sqlite-pool] reader pool queue full, discarding replacement connection"
                );
                close_connection_quietly(conn);
            }
        }
    }
}

/// Bound on the final-component symlink chain [`resolve_symlink_chain`]
/// follows before failing loud, mirroring the OS's own loop limit (e.g.
/// Linux/macOS `ELOOP`, commonly 40 hops) rather than looping forever on a
/// cycle.
const MAX_SYMLINK_DEPTH: u32 = 40;

/// Mint the canonical [`DbIdentity`] for a configured database path.
///
/// The sole minting point (ADR-091 backend-scoped attribution design note):
/// `tx_registry` origin threading and `sidecar_dir_for` re-keying both
/// consume this function's output rather than re-deriving it. Operationally
/// three steps:
///
/// 1. A relative configured path is resolved against the process's current
///    directory BEFORE any canonicalization — a bare file name has an empty
///    parent, and canonicalizing an empty path fails.
/// 2. If the resolved path exists, canonicalize the full path: this
///    resolves symlinks at every level, including a symlink at the
///    database-file level itself (a `link.sqlite` pointing at the real file
///    mints the target's identity).
/// 3. If the resolved path does not yet exist (first open), a dangling
///    file-level symlink is a valid first-open state — SQLite creates the
///    target through the link on first write, and minting the link's own
///    name would diverge from a later opener using the target path
///    directly. The final-component symlink chain is followed to its
///    ultimate target first (bounded, see [`MAX_SYMLINK_DEPTH`]), then that
///    target's PARENT directory is canonicalized and the file name is
///    appended unchanged — the same pattern `FsBlobStore` uses for its
///    root-keyed write locks (`stores/blob.rs::write_lock_for_root`), and
///    for the same reason: `Path::canonicalize` requires an existing path.
///
/// A resolved target whose parent directory does not exist fails minting
/// exactly as the subsequent database open itself would fail.
///
/// Returns the minted [`DbIdentity`] alongside the canonical [`PathBuf`] it
/// was built from — `DbIdentity` has no path accessor by design, so callers
/// that need the filesystem path (sidecar derivation) keep this pairing
/// rather than re-deriving it from the raw configured path.
fn mint_db_identity(configured_path: &Path) -> Result<(DbIdentity, PathBuf), SqliteError> {
    let absolute = if configured_path.is_absolute() {
        configured_path.to_path_buf()
    } else {
        let cwd = std::env::current_dir().map_err(|e| {
            SqliteError::InvalidData(format!(
                "cannot mint database identity for {configured_path:?}: failed to resolve the \
                 process current directory: {e}"
            ))
        })?;
        cwd.join(configured_path)
    };

    if absolute.exists() {
        let canonical = absolute.canonicalize().map_err(|e| {
            SqliteError::InvalidData(format!(
                "cannot mint database identity: failed to canonicalize existing path \
                 {absolute:?}: {e}"
            ))
        })?;
        return Ok((
            DbIdentity::new(canonical.clone().into_os_string()),
            canonical,
        ));
    }

    let resolved_target = resolve_symlink_chain(&absolute)?;
    let parent = resolved_target.parent().ok_or_else(|| {
        SqliteError::InvalidData(format!(
            "cannot mint database identity for {resolved_target:?}: path has no parent \
             directory"
        ))
    })?;
    let file_name = resolved_target.file_name().ok_or_else(|| {
        SqliteError::InvalidData(format!(
            "cannot mint database identity for {resolved_target:?}: path has no file name"
        ))
    })?;
    let canonical_parent = parent.canonicalize().map_err(|e| {
        SqliteError::InvalidData(format!(
            "cannot mint database identity: parent directory {parent:?} of first-open path \
             {resolved_target:?} does not exist or is inaccessible: {e}"
        ))
    })?;
    let mut identity_path = canonical_parent;
    identity_path.push(file_name);
    Ok((
        DbIdentity::new(identity_path.clone().into_os_string()),
        identity_path,
    ))
}

/// Follow a (possibly dangling) final-component symlink chain to its
/// ultimate target, bounded at [`MAX_SYMLINK_DEPTH`] hops. A path that is
/// not itself a symlink — including one that does not exist at all —
/// returns unchanged on the first iteration; this is the common case, a
/// first-open path with no symlink involved.
fn resolve_symlink_chain(path: &Path) -> Result<PathBuf, SqliteError> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_DEPTH {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&current).map_err(|e| {
                    SqliteError::InvalidData(format!(
                        "cannot mint database identity: failed to read symlink {current:?}: {e}"
                    ))
                })?;
                current = if target.is_absolute() {
                    target
                } else {
                    match current.parent() {
                        Some(parent) => parent.join(&target),
                        None => target,
                    }
                };
            }
            _ => return Ok(current),
        }
    }
    Err(SqliteError::InvalidData(format!(
        "cannot mint database identity for {path:?}: symlink chain exceeds \
         {MAX_SYMLINK_DEPTH} levels"
    )))
}

fn effective_reader_count(config: &PoolConfig, wal_enabled: bool) -> usize {
    if config.path.is_some() && config.wal_mode && wal_enabled {
        config.max_readers
    } else {
        0
    }
}

fn open_writer_connection(config: &PoolConfig) -> Result<Connection, SqliteError> {
    match config.path.as_ref() {
        Some(path) => {
            let flags = if config.read_only {
                writer_read_only_open_flags()
            } else {
                writer_open_flags()
            };
            Connection::open_with_flags(path, flags).map_err(Into::into)
        }
        None => Connection::open_in_memory().map_err(Into::into),
    }
}

fn open_reader_connection(path: &Path, config: &PoolConfig) -> Result<Connection, SqliteError> {
    let conn = Connection::open_with_flags(path, reader_open_flags())?;
    configure_reader_connection(&conn, config)?;
    Ok(conn)
}

fn writer_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// Read-only writer-slot open flags: no `SQLITE_OPEN_CREATE`, so a missing
/// path is rejected rather than silently created.
fn writer_read_only_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn reader_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn configure_writer_connection(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<bool, SqliteError> {
    if config.read_only {
        // Read-only writer slot: skip write-intent PRAGMAs (journal_mode,
        // wal_autocheckpoint, journal_size_limit all require write access to
        // change) and lock the connection down with query_only instead.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(config.busy_timeout)?;
        conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
        conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "query_only", "ON")?;

        let wal_enabled =
            config.wal_mode && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");
        return Ok(wal_enabled);
    }

    let wants_wal = config.path.is_some() && config.wal_mode;

    if wants_wal {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }

    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    let wal_enabled = wants_wal && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");

    if wal_enabled {
        conn.pragma_update(None, "wal_autocheckpoint", config.wal_autocheckpoint_pages)?;
        conn.pragma_update(None, "journal_size_limit", config.journal_size_limit_bytes)?;
    }

    Ok(wal_enabled)
}

fn configure_reader_connection(conn: &Connection, config: &PoolConfig) -> Result<(), SqliteError> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

fn current_journal_mode(conn: &Connection) -> Result<String, SqliteError> {
    conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
        .map(|mode| mode.to_ascii_lowercase())
        .map_err(Into::into)
}

fn reset_reader_connection(conn: &Connection) -> bool {
    if conn.is_autocommit() {
        return true;
    }

    match conn.execute_batch("ROLLBACK") {
        Ok(()) => conn.is_autocommit(),
        Err(rusqlite::Error::SqliteFailure(err, _)) => {
            if matches!(
                err.code,
                rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::DiskFull
            ) {
                return false;
            }
            conn.is_autocommit()
        }
        Err(_) => false,
    }
}

fn reader_connection_is_healthy(conn: &Connection) -> bool {
    match conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)) {
        Ok(_) => true,
        Err(rusqlite::Error::SqliteFailure(err, _)) => !matches!(
            err.code,
            rusqlite::ErrorCode::CannotOpen
                | rusqlite::ErrorCode::NotADatabase
                | rusqlite::ErrorCode::DatabaseCorrupt
                | rusqlite::ErrorCode::PermissionDenied
                | rusqlite::ErrorCode::SystemIoFailure
        ),
        Err(_) => true,
    }
}

fn close_connection_quietly(conn: Connection) {
    match conn.close() {
        Ok(()) => {}
        Err((conn, _)) => drop(conn),
    }
}

fn pool_exhausted_error(timeout: Duration, max_readers: usize) -> SqliteError {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(format!(
            "Pool exhausted: no reader available after {timeout:?} (max_readers={max_readers})"
        )),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Restores the process CWD on drop — including on panic — so a mid-test
    /// assertion failure (or an unexpected panic from the code under test)
    /// can never leave the process chdir'd into a `tempfile::tempdir()` that
    /// unwinds out from under every later test sharing this process.
    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn enter(dir: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    const POOL_ENV_VARS: [&str; 7] = [
        "KHIVE_BUSY_TIMEOUT_SECS",
        "KHIVE_CHECKOUT_TIMEOUT_SECS",
        "KHIVE_WAL_AUTOCHECKPOINT_PAGES",
        "KHIVE_JOURNAL_SIZE_LIMIT_BYTES",
        "KHIVE_WRITE_QUEUE",
        "KHIVE_WRITE_QUEUE_CAPACITY",
        "KHIVE_WRITE_ROUTING",
    ];

    struct PoolEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl PoolEnvGuard {
        fn capture() -> Self {
            Self {
                saved: POOL_ENV_VARS
                    .into_iter()
                    .map(|key| (key, std::env::var_os(key)))
                    .collect(),
            }
        }
    }

    impl Drop for PoolEnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn clear_pool_env() -> PoolEnvGuard {
        let guard = PoolEnvGuard::capture();
        for var in POOL_ENV_VARS {
            std::env::remove_var(var);
        }
        guard
    }

    #[test]
    #[serial]
    fn pool_config_default_values_match_constants() {
        // Ensure defaults are not accidentally changed. The process env may
        // legitimately carry overrides (CI jobs set KHIVE_CHECKOUT_TIMEOUT_SECS),
        // so clear them first — this test asserts the constants, not the env.
        let _pool_env = clear_pool_env();
        let cfg = PoolConfig::default();
        assert_eq!(
            cfg.wal_autocheckpoint_pages,
            DEFAULT_WAL_AUTOCHECKPOINT_PAGES
        );
        assert_eq!(
            cfg.journal_size_limit_bytes,
            DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
        );
        assert_eq!(cfg.busy_timeout, Duration::from_secs(30));
        assert_eq!(cfg.checkout_timeout, Duration::from_secs(5));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_wal_autocheckpoint() {
        std::env::set_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "8000");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES");
        assert_eq!(cfg.wal_autocheckpoint_pages, 8000);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_journal_size_limit() {
        std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "134217728");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
        assert_eq!(cfg.journal_size_limit_bytes, 134_217_728);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_busy_timeout() {
        std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", "60");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_BUSY_TIMEOUT_SECS");
        assert_eq!(cfg.busy_timeout, Duration::from_secs(60));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_checkout_timeout() {
        std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", "10");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS");
        assert_eq!(cfg.checkout_timeout, Duration::from_secs(10));
    }

    #[test]
    #[serial]
    fn pool_config_write_queue_defaults_unset() {
        let _pool_env = clear_pool_env();
        let cfg = PoolConfig::default();
        assert_eq!(cfg.write_queue_enabled, None);
        assert_eq!(cfg.write_queue_capacity, DEFAULT_WRITE_QUEUE_CAPACITY);
    }

    #[test]
    #[serial]
    fn clear_pool_env_restores_overrides_on_drop() {
        let _ambient_env = PoolEnvGuard::capture();
        std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", "73");

        {
            let _pool_env = clear_pool_env();
            assert_eq!(std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS"), None);
        }

        assert_eq!(
            std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS"),
            Some(std::ffi::OsString::from("73"))
        );
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_queue_enabled() {
        std::env::set_var("KHIVE_WRITE_QUEUE", "1");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_QUEUE");
        assert_eq!(cfg.write_queue_enabled, Some(true));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_queue_enabled_accepts_true_case_insensitive() {
        std::env::set_var("KHIVE_WRITE_QUEUE", "True");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_QUEUE");
        assert_eq!(cfg.write_queue_enabled, Some(true));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_queue_enabled_accepts_zero_as_explicit_off() {
        std::env::set_var("KHIVE_WRITE_QUEUE", "0");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_QUEUE");
        assert_eq!(cfg.write_queue_enabled, Some(false));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_queue_invalid_value_is_explicit_off() {
        // Documented contract (`write_queue_enabled` docs): `"1"`/`"true"`
        // (case-insensitive) set `Some(true)`; any other value — garbage
        // included — sets `Some(false)`, never `None`.
        std::env::set_var("KHIVE_WRITE_QUEUE", "banana");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_QUEUE");
        assert_eq!(cfg.write_queue_enabled, Some(false));
    }

    #[test]
    #[serial]
    fn pool_config_write_routing_strict_defaults_off() {
        let _pool_env = clear_pool_env();
        let cfg = PoolConfig::default();
        assert!(!cfg.write_routing_strict);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_routing_strict() {
        std::env::set_var("KHIVE_WRITE_ROUTING", "strict");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_ROUTING");
        assert!(cfg.write_routing_strict);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_routing_strict_case_insensitive() {
        std::env::set_var("KHIVE_WRITE_ROUTING", "STRICT");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_ROUTING");
        assert!(cfg.write_routing_strict);
    }

    #[test]
    #[serial]
    fn pool_config_env_write_routing_ignores_unrecognized_value() {
        std::env::set_var("KHIVE_WRITE_ROUTING", "eventual");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_ROUTING");
        assert!(!cfg.write_routing_strict);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_write_queue_capacity() {
        std::env::set_var("KHIVE_WRITE_QUEUE_CAPACITY", "64");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_QUEUE_CAPACITY");
        assert_eq!(cfg.write_queue_capacity, 64);
    }

    #[test]
    #[serial]
    fn pool_config_env_invalid_write_queue_capacity_falls_back_to_default() {
        std::env::set_var("KHIVE_WRITE_QUEUE_CAPACITY", "0");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WRITE_QUEUE_CAPACITY");
        assert_eq!(cfg.write_queue_capacity, DEFAULT_WRITE_QUEUE_CAPACITY);
    }

    #[test]
    #[serial]
    fn pool_config_env_invalid_falls_back_to_default() {
        std::env::set_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "not_a_number");
        std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES");
        std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
        assert_eq!(
            cfg.wal_autocheckpoint_pages,
            DEFAULT_WAL_AUTOCHECKPOINT_PAGES
        );
        assert_eq!(
            cfg.journal_size_limit_bytes,
            DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
        );
    }

    #[test]
    fn file_backed_pool_opens_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_pool.db");
        let cfg = PoolConfig {
            path: Some(path.clone()),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");
        assert!(path.exists());
        assert!(pool.max_readers() > 0);
    }

    #[tokio::test]
    #[serial]
    async fn unset_write_queue_resolves_on_for_file_backed_pool() {
        let _pool_env = clear_pool_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unset_file_backed.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: None,
            ..PoolConfig::default()
        })
        .expect("file-backed pool should open");
        assert_eq!(pool.config().write_queue_enabled, Some(true));
        // Behavioral half: the resolved value actually routes — a writer
        // task spawns for this pool, not merely a config field flipping.
        assert!(
            pool.writer_task_handle()
                .expect("spawn inside a runtime context must not error")
                .is_some(),
            "resolved-on file-backed pool must actually spawn the writer task"
        );
    }

    #[tokio::test]
    #[serial]
    async fn unset_write_queue_resolves_off_for_memory_backed_pool() {
        let _pool_env = clear_pool_env();
        let pool = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: None,
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
        assert_eq!(pool.config().write_queue_enabled, Some(false));
        // Behavioral half: resolved-off means no writer task, even inside a
        // runtime context where one could spawn.
        assert!(
            pool.writer_task_handle()
                .expect("disabled queue must resolve without error")
                .is_none(),
            "resolved-off in-memory pool must not spawn a writer task"
        );
    }

    #[test]
    #[serial]
    fn explicit_false_stays_off_for_file_backed_pool() {
        let _pool_env = clear_pool_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("explicit_false_file_backed.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("file-backed pool should open");
        assert_eq!(pool.config().write_queue_enabled, Some(false));
    }

    #[tokio::test]
    #[serial]
    async fn explicit_true_stays_on_for_memory_backed_pool() {
        let _pool_env = clear_pool_env();
        let pool = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
        assert_eq!(pool.config().write_queue_enabled, Some(true));
        // Pinned behavioral contract: the explicit-on preference survives in
        // the stored config, but an in-memory pool cannot host a writer
        // task — `writer_task::spawn` fails its standalone-connection open
        // and degrades to no writer task, so callers fall back to the
        // legacy pool-mutex write path and there is no JoinHandle to drain.
        assert!(
            pool.writer_task_handle()
                .expect("spawn degrade must resolve without error")
                .is_none(),
            "explicit-on in-memory pool must degrade to no writer task"
        );
        assert_eq!(
            pool.writer_task_spawn_count(),
            1,
            "the spawn attempt must happen exactly once and degrade, not retry"
        );
        assert!(
            pool.take_writer_task_join().is_none(),
            "a degraded spawn stores no JoinHandle to drain"
        );
    }

    #[test]
    fn standalone_writer_open_counts_its_connection_class_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standalone_writer_counter.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            ..PoolConfig::default()
        })
        .expect("file-backed pool");

        let _standalone = pool
            .open_standalone_writer()
            .expect("standalone writer opens");

        assert_eq!(
            pool.writer_acquisition_snapshot(),
            WriterAcquisitionSnapshot {
                acquisitions: 1,
                pooled_acquisitions: 0,
                standalone_acquisitions: 1,
                writer_task_acquisitions: 0,
                timeouts: 0,
            },
            "the public standalone boundary must contribute to the aggregate exactly once"
        );
    }

    #[test]
    fn in_memory_pool_degrades_to_single_connection() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("in-memory pool should open");
        assert_eq!(pool.max_readers(), 0);
    }

    #[test]
    fn writer_checkout_and_release_works() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        {
            let _writer = pool.writer().expect("writer checkout should succeed");
        }
        // After drop, writer should be re-acquirable.
        let _writer2 = pool
            .writer()
            .expect("second writer checkout should succeed");
    }

    #[test]
    fn writer_checkout_snapshot_counts_successes_and_timeouts_at_the_pool_boundary() {
        let cfg = PoolConfig {
            path: None,
            checkout_timeout: Duration::from_millis(1),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();

        assert_eq!(
            pool.writer_acquisition_snapshot(),
            WriterAcquisitionSnapshot::default()
        );

        let held = pool.writer().expect("first checkout succeeds");
        let error = match pool.writer() {
            Ok(_) => panic!("the held pool mutex must force a finite-wait timeout"),
            Err(error) => error,
        };
        assert!(
            matches!(
                &error,
                SqliteError::WriterPoolCheckoutTimeout { timeout }
                    if *timeout == Duration::from_millis(1)
            ),
            "timeout must have a stable, structurally matchable stage: {error}"
        );
        assert_eq!(
            pool.writer_acquisition_snapshot(),
            WriterAcquisitionSnapshot {
                acquisitions: 1,
                pooled_acquisitions: 1,
                standalone_acquisitions: 0,
                writer_task_acquisitions: 0,
                timeouts: 1,
            }
        );

        drop(held);
        let _reacquired = pool.writer().expect("checkout succeeds after release");
        assert_eq!(
            pool.writer_acquisition_snapshot(),
            WriterAcquisitionSnapshot {
                acquisitions: 2,
                pooled_acquisitions: 2,
                standalone_acquisitions: 0,
                writer_task_acquisitions: 0,
                timeouts: 1,
            }
        );
    }

    #[test]
    fn zero_wait_maintenance_skip_is_not_reported_as_a_checkout_timeout() {
        let pool = ConnectionPool::new(PoolConfig::default()).unwrap();
        let held = pool.writer().expect("finite-wait checkout succeeds");
        let before = pool.writer_acquisition_snapshot();

        assert!(
            pool.try_writer_nowait().is_err(),
            "zero-wait maintenance checkout must skip while held"
        );

        assert_eq!(
            pool.writer_acquisition_snapshot(),
            before,
            "a checkpoint-style zero-wait skip is not a finite-wait checkout timeout"
        );
        drop(held);
    }

    /// ADR-091 Plank 0: `WriterGuard::transaction` registers/deregisters a
    /// tx_registry entry around the closure. See
    /// crates/khive-db/docs/api/pool.md#writer_guard_transaction_registers_during_closure_only
    #[test]
    #[serial(tx_registry)]
    fn writer_guard_transaction_registers_during_closure_only() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        let guard = pool.writer().unwrap();

        let mut seen_during_closure = false;
        let result: Result<(), SqliteError> = guard.transaction(|_conn| {
            seen_during_closure = khive_storage::tx_registry::snapshot()
                .iter()
                .any(|(_, label)| label.as_deref() == Some("writer_guard_tx"));
            Ok(())
        });
        result.expect("transaction should commit");

        assert!(
            seen_during_closure,
            "expected a writer_guard_tx entry visible inside the closure"
        );
        assert!(
            !khive_storage::tx_registry::snapshot()
                .iter()
                .any(|(_, label)| label.as_deref() == Some("writer_guard_tx")),
            "expected the entry to be gone after the transaction completes"
        );
    }

    /// ADR-067 Component A: `writer_task_handle` must fail loud (typed
    /// error, not panic) with no Tokio runtime available. See
    /// crates/khive-db/docs/api/pool.md#writer_task_handle_fails_loud_without_tokio_runtime
    #[test]
    fn writer_task_handle_fails_loud_without_tokio_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_task_no_runtime.db");
        let cfg = PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");

        let result = pool.writer_task_handle();

        assert!(
            matches!(result, Err(StorageError::WriterTaskNoRuntime)),
            "expected Err(StorageError::WriterTaskNoRuntime) outside a Tokio \
             runtime, got {result:?}"
        );
        assert_eq!(
            pool.writer_task_spawn_count(),
            0,
            "the guard must reject before ever attempting tokio::spawn"
        );
    }

    /// Join-handle lifecycle: a spawn-configured pool stores exactly one
    /// JoinHandle — the first `take_writer_task_join` after spawn returns
    /// it, and every later take returns `None` (the one-shot contract that
    /// lets exactly one subsystem own the drain).
    #[tokio::test]
    async fn take_writer_task_join_returns_some_once_then_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("join_lifecycle.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        })
        .expect("file-backed pool should open");

        // Spawning is lazy: nothing to take before the first
        // `writer_task_handle()` call actually spawns the task.
        assert!(
            pool.take_writer_task_join().is_none(),
            "before spawn there is no JoinHandle to take"
        );
        pool.writer_task_handle()
            .expect("runtime is present")
            .expect("write queue enabled must spawn a writer task");

        assert!(
            pool.take_writer_task_join().is_some(),
            "the first take must return the spawned task's JoinHandle"
        );
        assert!(
            pool.take_writer_task_join().is_none(),
            "the second take must return None — the handle is one-shot"
        );
    }

    /// Debug half of the first-wins contract: a second
    /// `set_writer_task_join` call is a construction bug, and debug builds
    /// trip the method's debug_assert loudly instead of carrying on.
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "writer task JoinHandle stored twice")]
    async fn set_writer_task_join_second_store_trips_debug_assert() {
        let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool should open");
        pool.set_writer_task_join(tokio::spawn(async {}));
        pool.set_writer_task_join(tokio::spawn(async {}));
    }

    /// Release half of the first-wins contract: with the debug_assert
    /// compiled out, a second `set_writer_task_join` call keeps the
    /// EXISTING handle and drops the new one. The stored handle is
    /// therefore the first task's, so awaiting the taken handle completes
    /// the FIRST task's observable effect.
    #[cfg(not(debug_assertions))]
    #[tokio::test]
    async fn set_writer_task_join_first_wins_keeps_existing_handle() {
        let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool should open");

        // First task: completes promptly and signals completion — the
        // observable effect the bounded await below asserts on.
        let (first_done_tx, first_done_rx) = tokio::sync::oneshot::channel::<()>();
        let first = tokio::spawn(async move {
            let _ = first_done_tx.send(());
        });
        // Second task: parks on a receiver nobody sends to, so it never
        // completes on its own. If first-wins failed and this task's handle
        // were the stored one, the bounded await below would time out.
        let (_never_sent, never_rx) = tokio::sync::oneshot::channel::<()>();
        let second = tokio::spawn(async move {
            let _ = never_rx.await;
        });

        pool.set_writer_task_join(first);
        pool.set_writer_task_join(second);

        let taken = pool
            .take_writer_task_join()
            .expect("the first handle must still be stored");
        tokio::time::timeout(Duration::from_secs(5), taken)
            .await
            .expect("stored handle must be the first task's; the second never completes")
            .expect("the first task must not panic");
        assert!(
            first_done_rx.await.is_ok(),
            "completing the taken handle must mean the FIRST task ran to completion"
        );
    }

    /// ADR-091 backend-scoped attribution: the real path, a directory
    /// symlink, a file-level symlink, a relative spelling, and a bare file
    /// name (resolved against the current directory) must all mint an
    /// identical `DbIdentity` and canonical path for the same database.
    #[test]
    #[serial(pool_cwd)]
    fn mint_db_identity_alias_convergence() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let db_path = real_dir.join("khive.db");
        fs::write(&db_path, b"").unwrap();

        let dir_symlink = dir.path().join("dir_link");
        let file_symlink = dir.path().join("file_link.db");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real_dir, &dir_symlink).unwrap();
            std::os::unix::fs::symlink(&db_path, &file_symlink).unwrap();
        }

        let (via_real, canonical_real) = mint_db_identity(&db_path).unwrap();

        // Relative spelling: resolved against the process CWD (step 1).
        let relative_result = {
            let _cwd = CwdGuard::enter(&real_dir);
            mint_db_identity(&PathBuf::from("khive.db"))
        };
        let (via_relative, canonical_relative) = relative_result.unwrap();
        assert_eq!(canonical_real, canonical_relative);
        assert_eq!(via_real, via_relative);

        #[cfg(unix)]
        {
            let (via_dir_symlink, canonical_dir_symlink) =
                mint_db_identity(&dir_symlink.join("khive.db")).unwrap();
            assert_eq!(canonical_real, canonical_dir_symlink);
            assert_eq!(via_real, via_dir_symlink);

            let (via_file_symlink, canonical_file_symlink) =
                mint_db_identity(&file_symlink).unwrap();
            assert_eq!(canonical_real, canonical_file_symlink);
            assert_eq!(via_real, via_file_symlink);
        }

        // Bare file name: resolved against the current directory (step 1).
        let bare_name_result = {
            let _cwd = CwdGuard::enter(&real_dir);
            mint_db_identity(&PathBuf::from("khive.db"))
        };
        let (via_bare_name, canonical_bare_name) = bare_name_result.unwrap();
        assert_eq!(canonical_real, canonical_bare_name);
        assert_eq!(via_real, via_bare_name);
    }

    /// ADR-091 backend-scoped attribution: `DbIdentity`/canonical-path
    /// equality across alias spellings (proven above by
    /// `mint_db_identity_alias_convergence`) does not by itself prove the
    /// walpin sidecar re-key — `sidecar_dir_for` is a separate, purely
    /// lexical derivation (`walpin::sidecar_dir_for`) that must be fed the
    /// *minted* canonical path, never the raw configured one. This test
    /// opens a real `ConnectionPool` (not the private `mint_db_identity` free
    /// function) through each alias spelling and asserts
    /// `sidecar_dir_for(pool.canonical_path())` converges to one directory —
    /// exercising the actual `ConnectionPool::new` → `canonical_path()` wiring
    /// every sidecar consumer (`checkpoint.rs`) reads from.
    #[test]
    #[serial(pool_cwd)]
    fn sidecar_dir_for_alias_convergence() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        fs::create_dir(&real_dir).unwrap();
        let db_path = real_dir.join("khive.db");
        fs::write(&db_path, b"").unwrap();

        let dir_symlink = dir.path().join("dir_link");
        let file_symlink = dir.path().join("file_link.db");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real_dir, &dir_symlink).unwrap();
            std::os::unix::fs::symlink(&db_path, &file_symlink).unwrap();
        }

        let pool_for = |path: &Path| -> Arc<ConnectionPool> {
            let cfg = PoolConfig {
                path: Some(path.to_path_buf()),
                ..PoolConfig::default()
            };
            Arc::new(ConnectionPool::new(cfg).expect("file-backed pool should open"))
        };
        let sidecar_of = |pool: &ConnectionPool| -> PathBuf {
            crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed pool"))
        };

        let via_real = pool_for(&db_path);
        let sidecar_real = sidecar_of(&via_real);

        let via_relative = {
            let _cwd = CwdGuard::enter(&real_dir);
            pool_for(Path::new("khive.db"))
        };
        assert_eq!(
            sidecar_real,
            sidecar_of(&via_relative),
            "a relative spelling of the same database must derive the same sidecar directory"
        );

        #[cfg(unix)]
        {
            let via_dir_symlink = pool_for(&dir_symlink.join("khive.db"));
            assert_eq!(
                sidecar_real,
                sidecar_of(&via_dir_symlink),
                "opening through a directory symlink must derive the same sidecar directory"
            );

            let via_file_symlink = pool_for(&file_symlink);
            assert_eq!(
                sidecar_real,
                sidecar_of(&via_file_symlink),
                "opening through a file-level symlink must derive the same sidecar directory"
            );
        }

        let via_bare_name = {
            let _cwd = CwdGuard::enter(&real_dir);
            pool_for(Path::new("khive.db"))
        };
        assert_eq!(
            sidecar_real,
            sidecar_of(&via_bare_name),
            "a bare file name resolved against the current directory must derive the same \
             sidecar directory"
        );
    }

    /// ADR-091 backend-scoped attribution: opening via a file-level symlink
    /// whose target does not exist yet (a valid first-open state), then
    /// after the target is created, opening via the target path directly,
    /// must mint identical `DbIdentity` values — the first-open path
    /// resolves the final component before canonicalizing the parent.
    #[cfg(unix)]
    #[test]
    fn mint_db_identity_dangling_symlink_first_open_convergence() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.db");
        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!target.exists(), "target must not exist yet (dangling)");

        let (via_dangling_link, canonical_via_link) = mint_db_identity(&link).unwrap();

        // Now create the target (as SQLite would on first write) and mint
        // again directly against the target path.
        fs::write(&target, b"").unwrap();
        let (via_target, canonical_via_target) = mint_db_identity(&target).unwrap();

        assert_eq!(canonical_via_link, canonical_via_target);
        assert_eq!(via_dangling_link, via_target);
    }

    /// A resolved target whose parent directory does not exist must fail
    /// minting exactly as the subsequent database open itself would fail.
    #[test]
    fn mint_db_identity_missing_parent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nonexistent_subdir").join("khive.db");
        let result = mint_db_identity(&missing);
        assert!(
            result.is_err(),
            "minting must fail when the parent directory does not exist"
        );
    }

    /// Non-UTF-8 database paths (Unix) must round-trip through
    /// `DbIdentity`/canonicalization without loss.
    #[cfg(unix)]
    #[test]
    fn mint_db_identity_non_utf8_path_round_trips() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        // 0xFF is not valid UTF-8 as a standalone byte.
        let raw_name = OsStr::from_bytes(b"khive-\xffdb.sqlite");
        let db_path = dir.path().join(raw_name);
        // Some Unix filesystems (notably macOS's APFS) reject non-UTF-8
        // names outright at the syscall level — that is a filesystem
        // limitation, not a `mint_db_identity` bug, so skip rather than
        // fail where the underlying `write` itself cannot succeed.
        if let Err(e) = fs::write(&db_path, b"") {
            eprintln!(
                "skipping mint_db_identity_non_utf8_path_round_trips: filesystem rejected a \
                 non-UTF-8 file name ({e}); this platform's filesystem does not support the \
                 case under test"
            );
            return;
        }

        let (identity, canonical) = mint_db_identity(&db_path).unwrap();
        assert_eq!(canonical.file_name().unwrap(), raw_name);

        let (identity_again, canonical_again) = mint_db_identity(&db_path).unwrap();
        assert_eq!(identity, identity_again);
        assert_eq!(canonical, canonical_again);
    }
}
