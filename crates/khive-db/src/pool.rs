//! Connection pool for SQLite: one exclusive writer, N concurrent readers.
use crossbeam_queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use rusqlite::hooks::{AuthContext, Authorization};
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::io::Read as _;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::error::SqliteError;
use crate::writer_task::WriterTaskHandle;
use khive_storage::error::StorageError;
use khive_storage::tx_registry::{DbIdentity, TxOrigin};

const CACHE_SIZE_KIB: &str = "-65536";
const MMAP_SIZE_BYTES: &str = "1073741824";
const DEFAULT_READER_CAP: usize = 8;

const DEFAULT_JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864; // 64 MiB
const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 256;

/// Bounded WAL autocheckpoint applied to writer-capable connections while no
/// dedicated checkpoint owner has claimed the pool (4,000 pages ≈ 16 MiB at
/// SQLite's default 4 KiB page size — SQLite's historic behaviour for this
/// pool). Not a tuning parameter: there is no config field or environment
/// override, and the only way to change the effective value is an actual
/// ownership claim ([`ConnectionPool::claim_checkpoint_ownership`]), which a
/// runtime may make only when it really runs the scheduled checkpoint task.
pub(crate) const FALLBACK_WAL_AUTOCHECKPOINT_PAGES: u32 = 4_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointOwnership {
    Unclaimed,
    Claiming,
    Claimed,
}

struct CheckpointOwnershipState {
    phase: CheckpointOwnership,
    #[cfg(test)]
    connection_waiters: usize,
}

#[cfg(test)]
struct CheckpointConnectionConfigPause {
    selected: std::sync::Barrier,
    resume: std::sync::Barrier,
}

#[cfg(test)]
impl CheckpointConnectionConfigPause {
    fn new() -> Self {
        Self {
            selected: std::sync::Barrier::new(2),
            resume: std::sync::Barrier::new(2),
        }
    }
}

struct CheckpointOwnershipGate {
    state: Mutex<CheckpointOwnershipState>,
    changed: Condvar,
    #[cfg(test)]
    connection_config_pause: Mutex<Option<Arc<CheckpointConnectionConfigPause>>>,
    #[cfg(test)]
    claim_lock_observed: Mutex<Option<std::sync::mpsc::SyncSender<bool>>>,
}

impl CheckpointOwnershipGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(CheckpointOwnershipState {
                phase: CheckpointOwnership::Unclaimed,
                #[cfg(test)]
                connection_waiters: 0,
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            connection_config_pause: Mutex::new(None),
            #[cfg(test)]
            claim_lock_observed: Mutex::new(None),
        }
    }

    /// Join an in-flight claim, or become the one caller that configures it.
    /// Returns `false` when another caller has already completed the claim.
    fn begin_claim(&self) -> bool {
        #[cfg(test)]
        let claim_lock_observed = self.claim_lock_observed.lock().take();
        #[cfg(test)]
        let mut state = if let Some(observed) = claim_lock_observed {
            match self.state.try_lock() {
                Some(state) => {
                    let _ = observed.send(false);
                    state
                }
                None => {
                    let _ = observed.send(true);
                    self.state.lock()
                }
            }
        } else {
            self.state.lock()
        };
        #[cfg(not(test))]
        let mut state = self.state.lock();
        loop {
            match state.phase {
                CheckpointOwnership::Unclaimed => {
                    state.phase = CheckpointOwnership::Claiming;
                    self.changed.notify_all();
                    return true;
                }
                CheckpointOwnership::Claiming => self.changed.wait(&mut state),
                CheckpointOwnership::Claimed => return false,
            }
        }
    }

    fn finish_claim(&self, succeeded: bool) {
        let mut state = self.state.lock();
        debug_assert_eq!(state.phase, CheckpointOwnership::Claiming);
        state.phase = if succeeded {
            CheckpointOwnership::Claimed
        } else {
            CheckpointOwnership::Unclaimed
        };
        self.changed.notify_all();
    }

    fn settled_state(&self) -> parking_lot::MutexGuard<'_, CheckpointOwnershipState> {
        let mut state = self.state.lock();
        while state.phase == CheckpointOwnership::Claiming {
            #[cfg(test)]
            {
                state.connection_waiters += 1;
                self.changed.notify_all();
            }
            self.changed.wait(&mut state);
            #[cfg(test)]
            {
                state.connection_waiters -= 1;
                self.changed.notify_all();
            }
        }
        state
    }

    #[cfg(test)]
    fn wal_autocheckpoint_pages(&self) -> u32 {
        let state = self.settled_state();
        match state.phase {
            CheckpointOwnership::Unclaimed => FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            CheckpointOwnership::Claimed => 0,
            CheckpointOwnership::Claiming => unreachable!("claim wait must settle the state"),
        }
    }

    /// Wait for any in-flight claim, select the resulting posture, and retain
    /// the gate until SQLite has applied that connection-local PRAGMA. A claim
    /// therefore linearizes entirely before or after this configuration,
    /// never between its state sample and side effect.
    fn configure_wal_autocheckpoint(&self, conn: &Connection) -> Result<(), SqliteError> {
        let state = self.settled_state();
        let pages = match state.phase {
            CheckpointOwnership::Unclaimed => FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            CheckpointOwnership::Claimed => 0,
            CheckpointOwnership::Claiming => unreachable!("claim wait must settle the state"),
        };
        #[cfg(test)]
        if let Some(pause) = self.connection_config_pause.lock().take() {
            pause.selected.wait();
            pause.resume.wait();
        }
        conn.pragma_update(None, "wal_autocheckpoint", pages)?;
        drop(state);
        Ok(())
    }
}

fn deny_retired_writer(_context: AuthContext<'_>) -> Authorization {
    Authorization::Deny
}

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
    /// per-call pool-mutex/standalone-connection path. Enabled by default
    /// for file-backed pools when unset; explicit override always wins.
    /// That default is a compatibility-routing posture subordinate to
    /// ADR-135 Amendment 1 and ADR-136 D1/D2 — the strict-routing default
    /// flip has NOT happened.
    ///
    /// The store layer resolves all of its routed write paths at write time;
    /// the classification table in `writer_task.rs` remains the authoritative
    /// inventory. This tranche does not claim the repository-wide
    /// single-writer guarantee: direct runtime-orchestration call sites remain
    /// #1847 follow-up work, and the strict default is still evidence-gated.
    ///
    /// `None` means the caller expressed no preference: [`ConnectionPool::new`]
    /// resolves it once `path` is known, defaulting to `true` for file-backed
    /// pools and `false` for in-memory ones. `Some(_)` is an explicit
    /// preference and always wins, in both directions, over that default.
    /// An explicit `Some(true)` on an in-memory pool is accepted DELIBERATELY
    /// and emits a warning before degrading to the legacy path — an in-memory
    /// pool cannot host a writer task (`writer_task::spawn`'s
    /// standalone-connection open fails); see
    /// `ConnectionPool::writer_task_handle` and the
    /// `explicit_true_stays_on_for_memory_backed_pool` test.
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
    /// ADR-136 D1: when `true`, every covered store write path that would
    /// otherwise silently degrade to the legacy pool-mutex/standalone-
    /// connection path on a missing or failed `WriterTask` handle instead
    /// returns an error.
    /// Exercises the store-layer routing tranche toward ADR-135 F2's
    /// strict-routing precondition without changing behavior for callers that
    /// never set the env var.
    ///
    /// Overridable via `KHIVE_WRITE_ROUTING` (value `"strict"`,
    /// case-insensitive; anything else, or unset, leaves this `false`).
    pub write_routing_strict: bool,
    /// Dedicated admission deadline (ADR-131 Decision 2) bounding ONLY the
    /// wait for capacity on the `WriterTask` write queue —
    /// [`WriterTaskHandle::send_bounded`]/`send_top_level_bounded`'s default
    /// timeout. Distinct from `checkout_timeout`, which bounds reader/pool
    /// checkout instead; the two authorities used to be conflated (#1382,
    /// #1643) before this field existed.
    ///
    /// Default: 2000 ms. Validated at [`ConnectionPool::new`] to fall in
    /// `[100, 10000]` ms; a value outside that range is a configuration
    /// error (`SqliteError::InvalidConfig`), never silently clamped into
    /// range.
    ///
    /// Overridable via `KHIVE_WRITE_ADMISSION_DEADLINE_MS`.
    pub write_admission_deadline_ms: u64,
}

/// ADR-131 Decision 2's validated range for `write_admission_deadline_ms`.
const WRITE_ADMISSION_DEADLINE_MS_RANGE: std::ops::RangeInclusive<u64> = 100..=10_000;
const DEFAULT_WRITE_ADMISSION_DEADLINE_MS: u64 = 2000;

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
            journal_size_limit_bytes: std::env::var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(DEFAULT_JOURNAL_SIZE_LIMIT_BYTES),
            read_only: false,
            // `var_os`, not `var`: the documented contract is "any SET value
            // other than 1/true means Some(false)" — a set-but-non-Unicode
            // value must count as set (var() would return Err and silently
            // fall through to the file-backed default of enabled).
            write_queue_enabled: std::env::var_os("KHIVE_WRITE_QUEUE").map(|v| {
                v.to_str()
                    .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            }),
            write_queue_capacity: std::env::var("KHIVE_WRITE_QUEUE_CAPACITY")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(DEFAULT_WRITE_QUEUE_CAPACITY),
            write_routing_strict: std::env::var("KHIVE_WRITE_ROUTING")
                .map(|v| v.eq_ignore_ascii_case("strict"))
                .unwrap_or(false),
            write_admission_deadline_ms: std::env::var("KHIVE_WRITE_ADMISSION_DEADLINE_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_WRITE_ADMISSION_DEADLINE_MS),
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

/// Enforce ADR-131 Decision 2's `write_admission_deadline_ms` bound at
/// configuration load: `[100, 10000]` ms, rejected rather than clamped when
/// out of range so a misconfiguration is never silently reinterpreted as a
/// different deadline than the operator asked for.
fn validate_write_admission_deadline(deadline_ms: u64) -> Result<(), SqliteError> {
    if WRITE_ADMISSION_DEADLINE_MS_RANGE.contains(&deadline_ms) {
        return Ok(());
    }
    Err(SqliteError::InvalidConfig(format!(
        "write_admission_deadline_ms must be in [{}, {}] ms, got {deadline_ms}",
        WRITE_ADMISSION_DEADLINE_MS_RANGE.start(),
        WRITE_ADMISSION_DEADLINE_MS_RANGE.end()
    )))
}

/// A read-write connection pool for SQLite.
///
/// Architecture:
/// - 1 writer connection protected by a Mutex (exclusive access)
/// - N reader connections in a lock-free queue (concurrent access)
/// - All connections share the same database file in WAL mode
///
/// Writable in-memory databases, or writable file databases when WAL mode is
/// disabled/unavailable, degrade to single-connection mode and route all
/// operations through the writer connection. A file-backed read-only pool
/// always retains at least one dedicated read-only connection: rollback-journal
/// snapshots do not need WAL to support concurrent readers, and inspection must
/// never alias a read onto the query-only writer slot.
pub struct ConnectionPool {
    writer: Arc<Mutex<Connection>>,
    /// Three-state gate for whether the ADR-091 scheduled task has claimed
    /// routine WAL reclamation for this pool. Until claimed, every
    /// writer-capable connection keeps a bounded SQLite autocheckpoint
    /// ([`FALLBACK_WAL_AUTOCHECKPOINT_PAGES`]) so a writable pool without a
    /// checkpoint task cannot grow its WAL without bound. After
    /// [`Self::claim_checkpoint_ownership`], writer-capable connections open
    /// with `wal_autocheckpoint = 0` and routine checkpoint I/O stays off
    /// application commit paths.
    checkpoint_ownership: CheckpointOwnershipGate,
    /// Fail-closed guard for the legacy pool-mutex writer. A transaction
    /// owner retires this connection after a body panic or when it cannot
    /// prove that finalization restored autocommit mode; subsequent checkouts
    /// must never reuse it.
    pooled_writer_retired: AtomicBool,
    /// Process-local writer acquisition counters shared with the pool's
    /// lifetime-owned writer task. Keeping the counters at the actual
    /// acquisition boundaries means new verbs inherit instrumentation without
    /// per-verb classification (ADR-133 D8 / issue #1389).
    writer_acquisition_counters: Arc<WriterAcquisitionCounters>,
    readers: ArrayQueue<Connection>,
    max_readers: usize,
    config: PoolConfig,
    /// Canonical physical target used by every connection in a file-backed
    /// read-only pool. Classification and open must share this exact spelling:
    /// deriving WAL sidecars from a configured symlink while SQLite follows it
    /// to another file can hide committed frames or a live writable `-shm`.
    /// The value is an `immutable=1` URI only for a clean, checkpointed WAL;
    /// rollback-journal databases and frozen WAL+SHM snapshots retain the
    /// canonical ordinary path and SQLite locking/change detection.
    read_only_open_target: Option<PathBuf>,
    sql_bridge_reader_slots: Arc<Semaphore>,
    sql_bridge_writer_slots: Arc<Semaphore>,
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
    /// Monotonic "a writer-task JoinHandle was stored at least once" flag
    /// backing [`Self::set_writer_task_join`]'s at-most-once guard: it holds
    /// the invariant even after [`Self::take_writer_task_join`] empties the
    /// slot, so a second store never re-arms it.
    writer_task_join_stored: AtomicBool,
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
    reusable: bool,
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

    /// Fail closed when connection-global state could not be restored after
    /// a read. A pooled reader is closed and replaced on drop; a degraded
    /// shared-writer reader is quarantined for the lifetime of the pool.
    pub(crate) fn discard(&mut self) {
        self.reusable = false;
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
            ReaderLease::Pooled(conn) if self.reusable => self.pool.return_reader(conn),
            ReaderLease::Pooled(conn) => {
                close_connection_quietly(conn);
                if let Ok(conn) = self.pool.open_reader_connection() {
                    if let Err(conn) = self.pool.readers.push(conn) {
                        close_connection_quietly(conn);
                    }
                }
            }
            ReaderLease::Shared(guard) if !self.reusable => {
                self.pool.retire_pooled_writer(&guard);
            }
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
    /// foreign keys, cache, mmap, temp store). Writable in-memory databases and
    /// writable non-WAL files fall back to single-connection mode. Read-only
    /// files retain a dedicated reader regardless of journal mode.
    pub fn new(config: PoolConfig) -> Result<Self, SqliteError> {
        refuse_home_data_store_in_tests(&config)?;
        validate_write_admission_deadline(config.write_admission_deadline_ms)?;

        // Resolve "no preference" (`None`) now that `path` is known: on for
        // file-backed pools, off for in-memory ones. An explicit `Some(_)`
        // preference is left untouched and always wins.
        let mut config = config;
        let inert_memory_queue_request =
            config.path.is_none() && config.write_queue_enabled == Some(true);
        config.write_queue_enabled =
            Some(config.write_queue_enabled.unwrap_or(config.path.is_some()));
        if inert_memory_queue_request {
            tracing::warn!(
                "write queue explicitly requested for an in-memory pool; it is inert because \
                 in-memory pools cannot host a writer task"
            );
        }

        // Mint the physical identity before WAL classification or SQLite open.
        // Every read-only connection below uses this same canonical path (or
        // an immutable URI derived from it), so a symlink cannot split main-file
        // resolution from sidecar resolution.
        let (origin, identity_path) = match config.path.as_ref() {
            Some(path) => {
                let (identity, canonical) = mint_db_identity(path)?;
                (TxOrigin::Database(identity), Some(canonical))
            }
            None => (TxOrigin::Memory, None),
        };
        let read_only_open_target = read_only_open_target(&config, identity_path.as_deref())?;
        let writer = open_writer_connection(&config, read_only_open_target.as_deref())?;
        let wal_enabled = configure_writer_connection(&writer, &config)?;
        let max_readers = effective_reader_count(&config, wal_enabled);

        let readers = ArrayQueue::new(max_readers.max(1));

        let pool = Self {
            writer: Arc::new(Mutex::new(writer)),
            checkpoint_ownership: CheckpointOwnershipGate::new(),
            pooled_writer_retired: AtomicBool::new(false),
            writer_acquisition_counters: Arc::new(WriterAcquisitionCounters::default()),
            readers,
            max_readers,
            config,
            read_only_open_target,
            sql_bridge_reader_slots: Arc::new(Semaphore::new(max_readers.max(1))),
            sql_bridge_writer_slots: Arc::new(Semaphore::new(1)),
            writer_task: OnceLock::new(),
            writer_task_join: Mutex::new(None),
            writer_task_join_stored: AtomicBool::new(false),
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

        // Best-effort, process-global diagnostics belong only to pools that
        // can acquire a writer. A read-only inspection pool has no writer
        // timeout to report and must neither mutate `<db_parent>/.khive-logs`
        // nor consume the global sink claim before a later writable pool.
        if !pool.config.read_only {
            crate::timeout_sink::init(
                pool.canonical_path().and_then(Path::parent),
                &crate::timeout_sink::db_label(&pool),
            );
        }

        Ok(pool)
    }

    /// Check out a reader connection.
    ///
    /// Tries to pop from the lock-free queue. If empty, spins briefly then
    /// waits with exponential backoff up to `checkout_timeout`.
    ///
    /// In degraded mode (WAL unavailable, `max_readers == 0`), this method
    /// checks the shared writer mutex in bounded slices and returns pool
    /// exhaustion after `checkout_timeout`; it never blocks indefinitely on
    /// the non-reentrant mutex.
    pub fn reader(&self) -> Result<ReaderGuard<'_>, SqliteError> {
        self.reader_until(|| false)?.ok_or_else(|| {
            SqliteError::InvalidData("uncancelled reader checkout stopped unexpectedly".into())
        })
    }

    /// Check out a reader while cooperatively polling a request cancellation
    /// predicate. The predicate is evaluated before connection acquisition and
    /// between backoff slices, so an abandoned request does not sit through the
    /// full pool checkout timeout or execute a statement when a reader later
    /// becomes available.
    pub(crate) fn reader_until<C>(
        &self,
        should_stop: C,
    ) -> Result<Option<ReaderGuard<'_>>, SqliteError>
    where
        C: Fn() -> bool,
    {
        if self.max_readers == 0 {
            self.ensure_pooled_writer_active()?;
            let started = Instant::now();
            loop {
                if should_stop() {
                    return Ok(None);
                }
                let remaining = self
                    .config
                    .checkout_timeout
                    .saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(pool_exhausted_error(
                        self.config.checkout_timeout,
                        self.max_readers,
                    ));
                }
                if let Some(guard) = self
                    .writer
                    .try_lock_for(remaining.min(Duration::from_millis(2)))
                {
                    self.ensure_pooled_writer_active()?;
                    return Ok(Some(ReaderGuard {
                        lease: Some(ReaderLease::Shared(guard)),
                        pool: self,
                        reusable: true,
                    }));
                }
            }
        }

        let started = Instant::now();
        let mut attempt = 0u32;

        loop {
            if should_stop() {
                return Ok(None);
            }
            if let Some(conn) = self.readers.pop() {
                return Ok(Some(ReaderGuard {
                    lease: Some(ReaderLease::Pooled(conn)),
                    pool: self,
                    reusable: true,
                }));
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
        self.ensure_pooled_writer_active()?;
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
        self.ensure_pooled_writer_active()?;
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
        self.ensure_pooled_writer_active()?;
        let guard = self.writer.try_lock().ok_or_else(|| {
            SqliteError::InvalidData(
                "writer connection busy (checkpoint skipped this tick)".to_string(),
            )
        })?;
        self.ensure_pooled_writer_active()?;
        Ok(WriterGuard {
            guard,
            origin: self.origin(),
        })
    }

    pub(crate) fn retire_pooled_writer(&self, conn: &Connection) {
        self.pooled_writer_retired.store(true, Ordering::Release);
        if let Err(error) = conn.authorizer(Some(deny_retired_writer)) {
            tracing::error!(
                %error,
                "failed to install the retired pooled-writer quarantine authorizer"
            );
        }
    }

    fn ensure_pooled_writer_active(&self) -> Result<(), SqliteError> {
        if self.pooled_writer_retired.load(Ordering::Acquire) {
            return Err(SqliteError::InvalidData(
                "pooled writer connection retired after a terminal transaction fault".to_string(),
            ));
        }
        Ok(())
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

    /// Pool-wide permits for file-backed raw-SQL reader opens and active reads.
    pub(crate) fn sql_bridge_reader_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sql_bridge_reader_slots)
    }

    /// Pool-wide permit for a file-backed raw-SQL writer handle.
    pub(crate) fn sql_bridge_writer_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sql_bridge_writer_slots)
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

    /// Whether the write queue is effectively enabled for this pool: the
    /// resolved `write_queue_enabled` flag AND file-backed.
    ///
    /// `ConnectionPool::new` resolves the "no preference" (`None`) preference
    /// to a concrete `Some(..)` once `path` is known, so every reader of
    /// `config.write_queue_enabled` sees a resolved value; the `debug_assert`
    /// pins that invariant and a `None` that slipped past would read as
    /// disabled. Bypassing `ConnectionPool::new` to construct a pool is a
    /// construction-path bug. Use this instead of repeating
    /// `config().write_queue_enabled.unwrap_or(false) && config().path.is_some()`
    /// at every routing/violation site.
    pub fn write_queue_active(&self) -> bool {
        debug_assert!(
            self.config.write_queue_enabled.is_some(),
            "write_queue_enabled must be resolved to Some(..) by ConnectionPool::new \
             before any write_queue_active read"
        );
        self.config.write_queue_enabled.unwrap_or(false) && self.config.path.is_some()
    }

    /// Whether a writer-task JoinHandle has been stored at least once.
    ///
    /// Unlike [`Self::take_writer_task_join`], this remains true after the
    /// one-shot handle slot is emptied, distinguishing a task that never
    /// spawned from a handle another caller already consumed.
    pub fn writer_task_join_was_stored(&self) -> bool {
        self.writer_task_join_stored.load(Ordering::SeqCst)
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
        // Same pinned invariant `write_queue_active` asserts, kept inline
        // here because this gate keys on the flag ALONE: an explicit
        // `Some(true)` on an in-memory pool must still attempt the spawn
        // and degrade (documented + tested in
        // `explicit_true_stays_on_for_memory_backed_pool`), so the
        // file-backed half of `write_queue_active` cannot gate this early
        // return.
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

    /// Resolve the writer task for a store write at the moment the write is
    /// issued, rather than trusting only a handle cached by a synchronous
    /// store constructor. Construction can legitimately run before Tokio is
    /// entered, in which case `writer_task_handle()` returns
    /// `WriterTaskNoRuntime` without caching a terminal `None`.
    ///
    /// Strict routing makes every missing handle fail closed here. The
    /// caller remains responsible for recording a non-strict direct fallback
    /// at the exact fallback seam with [`Self::record_direct_route`].
    pub(crate) fn writer_task_for_write(
        &self,
        cached: Option<&WriterTaskHandle>,
        operation: &'static str,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        let handle = match cached {
            Some(handle) => Some(handle.clone()),
            None => match self.writer_task_handle() {
                Ok(handle) => handle,
                Err(error) if self.config.write_routing_strict => return Err(error),
                Err(_) => None,
            },
        };

        if handle.is_none() && self.config.write_routing_strict {
            return Err(StorageError::Pool {
                operation: operation.into(),
                message: "strict write routing requires a writer-task handle; no handle is \
                          available, so the direct writer fallback was refused"
                    .into(),
            });
        }
        Ok(handle)
    }

    /// Record one actual compatibility fallback around the writer task. A
    /// file-backed pool with the queue enabled should never reach this seam
    /// in strict mode because [`Self::writer_task_for_write`] refuses first.
    pub(crate) fn record_direct_route(&self, site: crate::timeout_sink::Site) {
        if self.write_queue_active() {
            crate::timeout_sink::emit_direct_route_violation(
                &crate::timeout_sink::db_label(self),
                site,
            );
        }
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
    /// First-wins: if a handle was ever stored (including one a caller has
    /// since taken — `writer_task_join_stored` remembers), the existing
    /// state is kept and the new handle is dropped (dropping a `JoinHandle`
    /// detaches its task without cancelling it). A second store violates the
    /// at-most-once contract and trips the debug_assert in debug builds;
    /// release builds keep the first handle rather than silently swapping
    /// the drain owner out from under whichever caller already took it.
    pub(crate) fn set_writer_task_join(&self, join: tokio::task::JoinHandle<()>) {
        // `swap(true)` returns the prior value: `true` means a handle was
        // stored at least once before, so this is a second store — even when
        // the slot itself is empty because `take_writer_task_join` already
        // ran (the slot alone cannot tell "never stored" from "taken").
        let first_store = !self.writer_task_join_stored.swap(true, Ordering::SeqCst);
        debug_assert!(
            first_store,
            "writer task JoinHandle stored twice (even counting a taken one); \
             the writer_task OnceLock is supposed to make spawn at-most-once per pool"
        );
        if first_store {
            *self.writer_task_join.lock() = Some(join);
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
        let path = self.read_connection_path()?;
        open_reader_connection(path, &self.config)
    }

    fn read_connection_path(&self) -> Result<&Path, SqliteError> {
        self.read_only_open_target
            .as_deref()
            .or(self.config.path.as_deref())
            .ok_or_else(|| {
                SqliteError::InvalidData(
                    "in-memory databases do not support standalone connections".to_string(),
                )
            })
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
        self.checkpoint_ownership
            .configure_wal_autocheckpoint(&conn)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        let wal_enabled =
            self.config.wal_mode && current_journal_mode(&conn)?.eq_ignore_ascii_case("wal");
        if wal_enabled {
            conn.pragma_update(
                None,
                "journal_size_limit",
                self.config.journal_size_limit_bytes,
            )?;
        }

        Ok(conn)
    }

    /// Effective `PRAGMA wal_autocheckpoint` for a writer-capable connection
    /// opened right now: `0` once a dedicated checkpoint owner has claimed
    /// the pool, the bounded fallback otherwise.
    #[cfg(test)]
    pub(crate) fn effective_wal_autocheckpoint_pages(&self) -> u32 {
        self.checkpoint_ownership.wal_autocheckpoint_pages()
    }

    /// Claim routine WAL-checkpoint ownership for this pool.
    ///
    /// Called by the scheduled checkpoint task at startup — the one caller
    /// that actually replaces SQLite's per-commit autocheckpoint with
    /// dedicated PASSIVE checkpointing (ADR-091 Amendment 10). The claim
    /// makes every subsequently opened writer-capable connection set
    /// `PRAGMA wal_autocheckpoint = 0`, and re-applies that pragma on the
    /// already-open pooled writer under the writer mutex. A writer task
    /// spawned before the claim keeps its own long-lived connection;
    /// [`Self::propagate_checkpoint_claim_to_writer_task`] reaches that one.
    ///
    /// Without a claim, writer-capable connections keep the bounded
    /// `FALLBACK_WAL_AUTOCHECKPOINT_PAGES` threshold, so a writable pool
    /// in a process that never runs the checkpoint task (embedded runtimes,
    /// one-shot CLI executions) retains SQLite's own WAL reclamation instead
    /// of growing its WAL without bound.
    ///
    /// Read-only pools record the claim but have no writer-capable
    /// connections to reconfigure. Writable pools publish the claim only after
    /// the pooled writer is configured successfully; a failed attempt keeps
    /// the bounded fallback active and remains retryable.
    pub fn claim_checkpoint_ownership(&self) -> Result<(), SqliteError> {
        if !self.checkpoint_ownership.begin_claim() {
            return Ok(());
        }
        let result = (|| {
            if !self.config.read_only {
                let writer = self.writer()?;
                writer.conn().pragma_update(None, "wal_autocheckpoint", 0)?;
            }
            Ok(())
        })();
        self.checkpoint_ownership.finish_claim(result.is_ok());
        result
    }

    /// Flip an already-running writer task's long-lived connection to the
    /// claimed-owner setting.
    ///
    /// Connections opened after [`Self::claim_checkpoint_ownership`] inherit
    /// `wal_autocheckpoint = 0` at open; only a writer task spawned before
    /// the claim still holds a connection on the bounded fallback. Returns
    /// `Ok(())` without side effects when the pool's write queue is
    /// disabled.
    pub async fn propagate_checkpoint_claim_to_writer_task(&self) -> Result<(), StorageError> {
        let Some(handle) = self.writer_task_handle()? else {
            return Ok(());
        };
        handle
            .send_top_level(|conn| {
                conn.pragma_update(None, "wal_autocheckpoint", 0)
                    .map_err(|e| StorageError::Pool {
                        operation: "claim_checkpoint_ownership".into(),
                        message: e.to_string(),
                    })
            })
            .await
    }

    /// Open a standalone read-only connection to the same file-backed database.
    ///
    /// Companion to `open_standalone_writer` for stores that also need an
    /// independent reader connection outside the pooled reader queue.
    pub fn open_standalone_reader(&self) -> Result<Connection, SqliteError> {
        let path = self.read_connection_path()?;

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        configure_reader_connection(&conn, &self.config)?;
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
    if config.path.is_some() && config.read_only {
        config.max_readers.max(1)
    } else if config.path.is_some() && config.wal_mode && wal_enabled {
        config.max_readers
    } else {
        0
    }
}

fn open_writer_connection(
    config: &PoolConfig,
    read_only_open_target: Option<&Path>,
) -> Result<Connection, SqliteError> {
    match config.path.as_ref() {
        Some(path) => {
            let flags = if config.read_only {
                writer_read_only_open_flags()
            } else {
                writer_open_flags()
            };
            let target = if config.read_only {
                read_only_open_target.ok_or_else(|| {
                    SqliteError::InvalidData(
                        "file-backed read-only pool has no canonical open target".to_string(),
                    )
                })?
            } else {
                path
            };
            Connection::open_with_flags(target, flags).map_err(Into::into)
        }
        None => Connection::open_in_memory().map_err(Into::into),
    }
}

/// Select the one case that may safely use SQLite's immutable URI contract: a
/// clean, checkpointed persistent-WAL snapshot with neither a shared-memory
/// index nor committed frames in `<db>-wal`. A normal read-only connection can
/// create fresh `-wal`/`-shm` files even for that clean database, while
/// `immutable=1` keeps the source directory untouched. We deliberately do not
/// apply `immutable=1` to:
///
/// - rollback-journal databases, which can read safely with normal locking and
///   should continue observing committed changes when an operator points a
///   read-only connection at a live database; or
/// - WAL databases with a read-only `-shm`, where ordinary read-only SQLite can
///   consume committed WAL frames without writing the frozen index; or
/// - WAL databases with a writable `-shm`, which are potentially live. Those
///   fail closed before SQLite is opened rather than mutating shared state or
///   suppressing change detection unsafely.
///
/// A non-empty WAL without `-shm` is also refused before open. Immutable SQLite
/// does not rebuild a missing WAL index: it ignores the WAL entirely, which can
/// make a committed row disappear from inspection. Ordinary read-only SQLite
/// would recover the frames but create `-shm`, violating the physical
/// read-only contract. The operator must provide the frozen read-only `-shm`
/// alongside that WAL (or checkpoint a writable copy first).
fn read_only_open_target(
    config: &PoolConfig,
    physical_path: Option<&Path>,
) -> Result<Option<PathBuf>, SqliteError> {
    if !config.read_only {
        return Ok(None);
    }
    let Some(path) = physical_path else {
        return Ok(None);
    };
    read_only_wal_open_target_for_path(path).map(Some)
}

fn read_only_wal_open_target_for_path(path: &Path) -> Result<PathBuf, SqliteError> {
    if !sqlite_header_uses_wal(path)? {
        return Ok(path.to_path_buf());
    }

    let shm = sqlite_sidecar_path(path, "-shm");
    match fs::metadata(&shm) {
        Ok(metadata) if metadata.permissions().readonly() => {
            let wal = sqlite_sidecar_path(path, "-wal");
            match fs::metadata(&wal) {
                Ok(_) => Ok(path.to_path_buf()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(SqliteError::InvalidData(format!(
                        "read-only WAL snapshot {} has a shared-memory sidecar {} but no WAL \
                         sidecar {}; refusing the inconsistent sidecar set before SQLite open",
                        path.display(),
                        shm.display(),
                        wal.display(),
                    )))
                }
                Err(error) => Err(SqliteError::Io(error)),
            }
        }
        Ok(_) => Err(SqliteError::InvalidData(format!(
            "read-only WAL snapshot {} has a writable WAL shared-memory sidecar {}; close every \
             live writer and remove the transient -shm file (or make a genuinely frozen snapshot) \
             before inspection",
            path.display(),
            shm.display(),
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let wal = sqlite_sidecar_path(path, "-wal");
            match fs::metadata(&wal) {
                Ok(metadata) if metadata.len() > 0 => Err(SqliteError::InvalidData(format!(
                    "read-only WAL snapshot {} has a non-empty WAL sidecar {} but no read-only \
                     shared-memory sidecar {}; refusing before SQLite open because immutable \
                     mode would omit committed WAL frames and ordinary read-only mode would \
                     create or mutate -shm; include the frozen read-only -shm beside this \
                     snapshot, or checkpoint a writable copy before inspection",
                    path.display(),
                    wal.display(),
                    shm.display(),
                ))),
                Ok(_) => sqlite_immutable_uri(path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    sqlite_immutable_uri(path)
                }
                Err(error) => Err(SqliteError::Io(error)),
            }
        }
        Err(error) => Err(SqliteError::Io(error)),
    }
}

pub(crate) fn open_read_only_snapshot_connection(path: &Path) -> Result<Connection, SqliteError> {
    let (_, physical_path) = mint_db_identity(path)?;
    let target = read_only_wal_open_target_for_path(&physical_path)?;
    Connection::open_with_flags(&target, reader_open_flags()).map_err(Into::into)
}

fn sqlite_header_uses_wal(path: &Path) -> Result<bool, SqliteError> {
    let mut file = fs::File::open(path)?;
    let mut header = [0_u8; 20];
    if let Err(error) = file.read_exact(&mut header) {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            return Ok(false);
        }
        return Err(SqliteError::Io(error));
    }
    Ok(&header[..16] == b"SQLite format 3\0" && header[18] == 2 && header[19] == 2)
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn sqlite_immutable_uri(path: &Path) -> Result<PathBuf, SqliteError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut uri = String::from("file:");

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        push_sqlite_uri_path(&mut uri, absolute.as_os_str().as_bytes());
    }

    #[cfg(not(unix))]
    {
        let path = absolute.to_str().ok_or_else(|| {
            SqliteError::InvalidData(format!(
                "read-only WAL snapshot path is not representable as a SQLite URI: {}",
                absolute.display()
            ))
        })?;
        let normalized = path.replace('\\', "/");
        if cfg!(windows) && !normalized.starts_with('/') {
            uri.push('/');
        }
        push_sqlite_uri_path(&mut uri, normalized.as_bytes());
    }

    uri.push_str("?mode=ro&immutable=1");
    Ok(PathBuf::from(uri))
}

fn push_sqlite_uri_path(uri: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            uri.push(byte as char);
        } else {
            uri.push('%');
            uri.push(HEX[(byte >> 4) as usize] as char);
            uri.push(HEX[(byte & 0x0f) as usize] as char);
        }
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
    // The pool's startup writer always opens before any checkpoint owner can
    // claim the pool, so it starts on the bounded fallback;
    // `claim_checkpoint_ownership` re-applies the pragma on this connection
    // under the writer mutex when a dedicated owner attaches.
    conn.pragma_update(
        None,
        "wal_autocheckpoint",
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
    )?;

    let wal_enabled = wants_wal && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");

    if wal_enabled {
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

    struct WarningCapture {
        messages: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for WarningCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Visitor(Option<String>);

            impl tracing::field::Visit for Visitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = Some(format!("{value:?}"));
                    }
                }
            }

            let mut visitor = Visitor(None);
            event.record(&mut visitor);
            if let Some(message) = visitor.0 {
                self.messages.lock().unwrap().push(message);
            }
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

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

    fn wal_autocheckpoint_pages(conn: &Connection) -> u32 {
        conn.pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .expect("read PRAGMA wal_autocheckpoint")
    }

    fn journal_size_limit_bytes(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "journal_size_limit", |row| row.get(0))
            .expect("read PRAGMA journal_size_limit")
    }

    #[test]
    fn read_only_rollback_journal_pool_keeps_a_dedicated_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read_only_delete_journal.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY);")
                .unwrap();
            let mode: String = conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "delete");
        }

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap();

        assert!(
            pool.max_readers() > 0,
            "a read-only rollback-journal snapshot must use a genuine read-only reader, not \
             alias reader() onto the query-only writer slot"
        );
        let reader = pool.reader().expect("dedicated read-only reader checkout");
        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        drop(reader);
        assert_eq!(
            pool.writer_acquisition_snapshot(),
            WriterAcquisitionSnapshot::default(),
            "constructing and reading a rollback-journal snapshot must never acquire the writer"
        );
    }

    fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        PathBuf::from(sidecar)
    }

    fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    /// A persistent-WAL snapshot can carry committed rows that exist only in
    /// `<db>-wal`. With no copied `-shm`, immutable SQLite silently ignores
    /// those frames while ordinary read-only SQLite creates a new `-shm`.
    /// Refuse before either open strategy can lose data or mutate the source.
    #[test]
    fn read_only_persistent_wal_without_shm_is_refused_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wal-source.db");
        let snapshot = dir.path().join("snapshot ?#%.db");
        let source_wal = sqlite_sidecar(&source, "-wal");
        let snapshot_wal = sqlite_sidecar(&snapshot, "-wal");
        let snapshot_shm = sqlite_sidecar(&snapshot, "-shm");

        let source_conn = Connection::open(&source).unwrap();
        let mode: String = source_conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        source_conn
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        source_conn
            .execute_batch(
                "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO snapshot_row(body) VALUES ('committed-only-in-wal');",
            )
            .unwrap();
        assert!(source_wal.exists(), "fixture must retain a WAL sidecar");

        std::fs::copy(&source, &snapshot).unwrap();
        std::fs::copy(&source_wal, &snapshot_wal).unwrap();
        assert!(
            !snapshot_shm.exists(),
            "fixture intentionally omits the transient shared-memory index"
        );

        let main_before = std::fs::read(&snapshot).unwrap();
        let wal_before = std::fs::read(&snapshot_wal).unwrap();
        let entries_before = directory_entries(dir.path());

        let error = match ConnectionPool::new(PoolConfig {
            path: Some(snapshot.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        }) {
            Ok(_) => panic!("a non-empty WAL without its frozen -shm must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("would omit committed WAL frames"),
            "diagnostic must explain why neither unsafe open mode is allowed: {error}"
        );

        assert_eq!(std::fs::read(&snapshot).unwrap(), main_before);
        assert_eq!(std::fs::read(&snapshot_wal).unwrap(), wal_before);
        assert_eq!(directory_entries(dir.path()), entries_before);
        assert!(
            !snapshot_shm.exists(),
            "read-only admission and every reader must keep the source free of -shm"
        );

        drop(source_conn);
    }

    /// A complete frozen WAL snapshot includes the WAL index. Once all three
    /// files are read-only, ordinary SQLite read-only mode consumes the
    /// committed WAL frames without changing the source. This is intentionally
    /// not `immutable=1`: immutable SQLite ignores WAL contents.
    #[test]
    fn read_only_persistent_wal_with_read_only_shm_reads_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wal-source.db");
        let snapshot = dir.path().join("frozen-wal-snapshot.db");
        let source_wal = sqlite_sidecar(&source, "-wal");
        let source_shm = sqlite_sidecar(&source, "-shm");
        let snapshot_wal = sqlite_sidecar(&snapshot, "-wal");
        let snapshot_shm = sqlite_sidecar(&snapshot, "-shm");

        let source_conn = Connection::open(&source).unwrap();
        let mode: String = source_conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        source_conn
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        source_conn
            .execute_batch(
                "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO snapshot_row(body) VALUES ('committed-only-in-wal');",
            )
            .unwrap();
        assert!(source_wal.exists() && source_shm.exists());

        std::fs::copy(&source, &snapshot).unwrap();
        std::fs::copy(&source_wal, &snapshot_wal).unwrap();
        std::fs::copy(&source_shm, &snapshot_shm).unwrap();

        let snapshot_paths = [&snapshot, &snapshot_wal, &snapshot_shm];
        let original_permissions =
            snapshot_paths.map(|path| std::fs::metadata(path).unwrap().permissions());
        for path in snapshot_paths {
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(path, permissions).unwrap();
        }

        let main_before = std::fs::read(&snapshot).unwrap();
        let wal_before = std::fs::read(&snapshot_wal).unwrap();
        let shm_before = std::fs::read(&snapshot_shm).unwrap();
        let entries_before = directory_entries(dir.path());

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(snapshot.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap();
        let reader = pool.reader().unwrap();
        let body: String = reader
            .query_row("SELECT body FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(body, "committed-only-in-wal");
        drop(reader);

        let standalone = pool.open_standalone_reader().unwrap();
        let count: i64 = standalone
            .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(standalone);
        drop(pool);

        assert_eq!(std::fs::read(&snapshot).unwrap(), main_before);
        assert_eq!(std::fs::read(&snapshot_wal).unwrap(), wal_before);
        assert_eq!(std::fs::read(&snapshot_shm).unwrap(), shm_before);
        assert_eq!(directory_entries(dir.path()), entries_before);

        for (path, permissions) in snapshot_paths.into_iter().zip(original_permissions) {
            std::fs::set_permissions(path, permissions).unwrap();
        }
        drop(source_conn);
    }

    /// The configured spelling must not decide which WAL sidecars SQLite sees.
    /// A symlinked snapshot is classified and opened through one canonical
    /// physical path so committed frames beside the target remain visible and
    /// no sidecars are ever derived beside the alias.
    #[cfg(unix)]
    #[test]
    fn read_only_frozen_wal_symlink_reads_target_frames_without_mutation() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wal-source.db");
        let snapshot = dir.path().join("frozen-target.db");
        let alias = dir.path().join("frozen-alias.db");
        let source_wal = sqlite_sidecar(&source, "-wal");
        let source_shm = sqlite_sidecar(&source, "-shm");
        let snapshot_wal = sqlite_sidecar(&snapshot, "-wal");
        let snapshot_shm = sqlite_sidecar(&snapshot, "-shm");
        let alias_wal = sqlite_sidecar(&alias, "-wal");
        let alias_shm = sqlite_sidecar(&alias, "-shm");

        let source_conn = Connection::open(&source).unwrap();
        let mode: String = source_conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        source_conn
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        source_conn
            .execute_batch(
                "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO snapshot_row(body) VALUES ('visible-through-target-wal');",
            )
            .unwrap();
        assert!(source_wal.exists() && source_shm.exists());

        std::fs::copy(&source, &snapshot).unwrap();
        std::fs::copy(&source_wal, &snapshot_wal).unwrap();
        std::fs::copy(&source_shm, &snapshot_shm).unwrap();
        symlink(&snapshot, &alias).unwrap();
        assert!(!alias_wal.exists() && !alias_shm.exists());

        let snapshot_paths = [&snapshot, &snapshot_wal, &snapshot_shm];
        let original_permissions =
            snapshot_paths.map(|path| std::fs::metadata(path).unwrap().permissions());
        for path in snapshot_paths {
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(path, permissions).unwrap();
        }

        let main_before = std::fs::read(&snapshot).unwrap();
        let wal_before = std::fs::read(&snapshot_wal).unwrap();
        let shm_before = std::fs::read(&snapshot_shm).unwrap();
        let entries_before = directory_entries(dir.path());

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(alias.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap();
        let reader = pool.reader().unwrap();
        let body: String = reader
            .query_row("SELECT body FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(body, "visible-through-target-wal");
        drop(reader);
        let standalone = pool.open_standalone_reader().unwrap();
        let count: i64 = standalone
            .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(standalone);
        drop(pool);

        assert_eq!(std::fs::read(&snapshot).unwrap(), main_before);
        assert_eq!(std::fs::read(&snapshot_wal).unwrap(), wal_before);
        assert_eq!(std::fs::read(&snapshot_shm).unwrap(), shm_before);
        assert_eq!(directory_entries(dir.path()), entries_before);
        assert!(!alias_wal.exists() && !alias_shm.exists());

        for (path, permissions) in snapshot_paths.into_iter().zip(original_permissions) {
            std::fs::set_permissions(path, permissions).unwrap();
        }
        drop(source_conn);
    }

    /// A clean persistent-WAL database has no committed frames outside the
    /// checkpointed main file. This is the narrow case where an encoded
    /// `immutable=1` URI is safe and necessary to prevent SQLite from creating
    /// fresh sidecars. Reserved URI bytes in the filesystem path must still
    /// resolve to the exact database.
    #[test]
    fn read_only_clean_wal_snapshot_is_sidecar_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clean snapshot ?#%.db");
        {
            let conn = Connection::open(&path).unwrap();
            let mode: String = conn
                .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
                .unwrap();
            assert_eq!(mode.to_ascii_lowercase(), "wal");
            conn.execute_batch(
                "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO snapshot_row(body) VALUES ('checkpointed');",
            )
            .unwrap();
        }
        let wal = sqlite_sidecar(&path, "-wal");
        let shm = sqlite_sidecar(&path, "-shm");
        assert!(!wal.exists() && !shm.exists());
        assert!(sqlite_header_uses_wal(&path).unwrap());

        let original_permissions = std::fs::metadata(&path).unwrap().permissions();
        let mut read_only_permissions = original_permissions.clone();
        read_only_permissions.set_readonly(true);
        std::fs::set_permissions(&path, read_only_permissions).unwrap();
        let main_before = std::fs::read(&path).unwrap();
        let entries_before = directory_entries(dir.path());

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap();
        let reader = pool.reader().unwrap();
        let body: String = reader
            .query_row("SELECT body FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(body, "checkpointed");
        drop(reader);
        let standalone = pool.open_standalone_reader().unwrap();
        let count: i64 = standalone
            .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(standalone);
        drop(pool);

        assert_eq!(std::fs::read(&path).unwrap(), main_before);
        assert_eq!(directory_entries(dir.path()), entries_before);
        assert!(!wal.exists() && !shm.exists());
        std::fs::set_permissions(&path, original_permissions).unwrap();
    }

    /// `immutable=1` is unsafe for a database that can still change and is not
    /// needed for rollback-journal reads. Keep ordinary SQLite locking/change
    /// detection there so an already-open read-only pool observes a later
    /// committed transaction from a live writer.
    #[test]
    fn read_only_live_rollback_journal_keeps_change_detection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live-delete-journal.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch("CREATE TABLE live_row(id INTEGER PRIMARY KEY);")
            .unwrap();

        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .unwrap();
        {
            let reader = pool.reader().unwrap();
            let count: i64 = reader
                .query_row("SELECT COUNT(*) FROM live_row", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }

        writer
            .execute("INSERT INTO live_row DEFAULT VALUES", [])
            .unwrap();
        let reader = pool.reader().unwrap();
        let count: i64 = reader
            .query_row("SELECT COUNT(*) FROM live_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "rollback-journal read-only connections must retain live change detection"
        );
    }

    /// A writable `-shm` beside a WAL database is evidence that the database is
    /// not a sidecar-free frozen snapshot (and may have a live writer). Refuse
    /// before opening SQLite rather than mutate the shared index or unsafely
    /// assert `immutable=1` over a live database.
    #[test]
    fn read_only_live_wal_with_writable_shm_is_refused_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live-wal.db");
        let wal = sqlite_sidecar(&path, "-wal");
        let shm = sqlite_sidecar(&path, "-shm");
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE live_row(id INTEGER PRIMARY KEY);\
                 INSERT INTO live_row DEFAULT VALUES;",
            )
            .unwrap();
        assert!(wal.exists() && shm.exists());

        let main_before = std::fs::read(&path).unwrap();
        let wal_before = std::fs::read(&wal).unwrap();
        let shm_before = std::fs::read(&shm).unwrap();
        let error = match ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        }) {
            Ok(_) => panic!("a live WAL database with writable -shm must fail closed"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("writable WAL shared-memory sidecar"),
            "diagnostic must explain how to freeze the snapshot: {error}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), main_before);
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
        assert_eq!(std::fs::read(&shm).unwrap(), shm_before);

        drop(writer);
    }

    /// A symlink cannot hide a writable target `-shm`. Admission inspects the
    /// canonical target sidecar set before SQLite opens any connection and
    /// therefore refuses a potentially live WAL without touching either
    /// target or alias-adjacent paths.
    #[cfg(unix)]
    #[test]
    fn read_only_live_wal_symlink_rejects_target_writable_shm_without_mutation() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("live-target.db");
        let alias = dir.path().join("live-alias.db");
        let target_wal = sqlite_sidecar(&target, "-wal");
        let target_shm = sqlite_sidecar(&target, "-shm");
        let alias_wal = sqlite_sidecar(&alias, "-wal");
        let alias_shm = sqlite_sidecar(&alias, "-shm");

        let writer = Connection::open(&target).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        writer
            .execute_batch(
                "CREATE TABLE live_row(id INTEGER PRIMARY KEY);\
                 INSERT INTO live_row DEFAULT VALUES;",
            )
            .unwrap();
        assert!(target_wal.exists() && target_shm.exists());
        symlink(&target, &alias).unwrap();
        assert!(!alias_wal.exists() && !alias_shm.exists());

        let main_before = std::fs::read(&target).unwrap();
        let wal_before = std::fs::read(&target_wal).unwrap();
        let shm_before = std::fs::read(&target_shm).unwrap();
        let entries_before = directory_entries(dir.path());

        let error = match ConnectionPool::new(PoolConfig {
            path: Some(alias),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        }) {
            Ok(_) => panic!("a symlink must not hide the target's writable -shm"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("writable WAL shared-memory sidecar"),
            "diagnostic must identify the canonical target's live sidecar: {error}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), main_before);
        assert_eq!(std::fs::read(&target_wal).unwrap(), wal_before);
        assert_eq!(std::fs::read(&target_shm).unwrap(), shm_before);
        assert_eq!(directory_entries(dir.path()), entries_before);
        assert!(!alias_wal.exists() && !alias_shm.exists());

        drop(writer);
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
            cfg.journal_size_limit_bytes,
            DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
        );
        assert_eq!(cfg.busy_timeout, Duration::from_secs(30));
        assert_eq!(cfg.checkout_timeout, Duration::from_secs(5));
    }

    #[test]
    #[serial]
    fn legacy_env_cannot_change_wal_autocheckpoint() {
        let _pool_env = clear_pool_env();
        std::env::set_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "8000");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy_autocheckpoint_env.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            ..PoolConfig::default()
        })
        .expect("pool open");
        {
            let writer = pool.writer().expect("writer");
            assert_eq!(
                wal_autocheckpoint_pages(writer.conn()),
                FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
                "the removed env override must not change the unclaimed fallback"
            );
        }
        pool.claim_checkpoint_ownership().expect("claim ownership");
        let writer = pool.writer().expect("writer after claim");
        assert_eq!(
            wal_autocheckpoint_pages(writer.conn()),
            0,
            "the removed env override must not change the claimed-owner setting"
        );
        std::env::remove_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES");
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

    /// A SET-but-non-Unicode `KHIVE_WRITE_QUEUE` value (invalid UTF-8 on
    /// unix) must count as SET — `Some(false)` ("any SET value other than
    /// 1/true means off"), never a fall-through to the file-backed default.
    /// That is why `PoolConfig::default()` reads `var_os`, not `var`.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn pool_config_env_override_write_queue_non_unicode_value_is_explicit_off() {
        use std::os::unix::ffi::OsStrExt;
        let _pool_env = clear_pool_env();
        std::env::set_var(
            "KHIVE_WRITE_QUEUE",
            std::ffi::OsStr::from_bytes(b"\xff\xfe"),
        );
        let cfg = PoolConfig::default();
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
    fn pool_config_invalid_journal_size_limit_falls_back_to_default() {
        std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
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

    #[test]
    fn standalone_wal_writer_uses_configured_journal_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standalone_wal_journal_limit.db");
        let configured_limit = 12_345_678;
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            journal_size_limit_bytes: configured_limit,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("WAL pool open");

        let standalone = pool
            .open_standalone_writer_untracked()
            .expect("standalone WAL writer open");
        assert_eq!(current_journal_mode(&standalone).unwrap(), "wal");
        assert_eq!(journal_size_limit_bytes(&standalone), configured_limit);
    }

    #[test]
    fn standalone_rollback_writer_keeps_sqlite_journal_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standalone_rollback_journal_limit.db");
        let sqlite_default = {
            let conn = Connection::open(&path).expect("seed rollback-journal database");
            assert_eq!(current_journal_mode(&conn).unwrap(), "delete");
            journal_size_limit_bytes(&conn)
        };
        let configured_limit = if sqlite_default == 12_345_678 {
            23_456_789
        } else {
            12_345_678
        };
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            wal_mode: false,
            journal_size_limit_bytes: configured_limit,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("rollback-journal pool open");

        let standalone = pool
            .open_standalone_writer_untracked()
            .expect("standalone rollback-journal writer open");
        assert_eq!(current_journal_mode(&standalone).unwrap(), "delete");
        assert_eq!(journal_size_limit_bytes(&standalone), sqlite_default);
    }

    #[test]
    fn writer_connections_follow_checkpoint_ownership_claim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writer_autocheckpoint.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("pool open");

        // Unclaimed: every writer-capable connection keeps the bounded
        // fallback, so a pool without a checkpoint task retains SQLite's own
        // WAL reclamation.
        {
            let writer = pool.writer().expect("pooled writer");
            assert_eq!(
                wal_autocheckpoint_pages(writer.conn()),
                FALLBACK_WAL_AUTOCHECKPOINT_PAGES
            );
        }
        let standalone = pool
            .open_standalone_writer()
            .expect("standalone writer opened before any claim");
        assert_eq!(
            wal_autocheckpoint_pages(&standalone),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES
        );
        drop(standalone);

        // Claimed: the already-open pooled writer is re-configured under the
        // writer mutex, and every later writer-capable open disables the
        // autocheckpoint entirely.
        pool.claim_checkpoint_ownership().expect("claim ownership");
        {
            let writer = pool.writer().expect("pooled writer after claim");
            assert_eq!(wal_autocheckpoint_pages(writer.conn()), 0);
        }
        let claimed_standalone = pool
            .open_standalone_writer()
            .expect("standalone writer opened after the claim");
        assert_eq!(wal_autocheckpoint_pages(&claimed_standalone), 0);
        drop(claimed_standalone);

        let later_infrastructure = pool
            .open_standalone_writer_untracked()
            .expect("later infrastructure writer");
        assert_eq!(wal_autocheckpoint_pages(&later_infrastructure), 0);

        let memory_pool = ConnectionPool::new(PoolConfig {
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("in-memory pool open");
        let memory_writer = memory_pool.writer().expect("in-memory writer");
        assert_eq!(
            wal_autocheckpoint_pages(memory_writer.conn()),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            "an unclaimed in-memory pool keeps the bounded fallback"
        );
    }

    #[test]
    fn standalone_writer_waits_for_checkpoint_claim_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint_claim_race.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                checkout_timeout: Duration::from_secs(5),
                write_queue_enabled: Some(false),
                ..PoolConfig::default()
            })
            .expect("pool open"),
        );

        let legacy_conn = pool.legacy_conn();
        let held_writer = legacy_conn.lock();
        let claim_start = Arc::new(std::sync::Barrier::new(2));
        let claim_pool = Arc::clone(&pool);
        let claim_thread_start = Arc::clone(&claim_start);
        let claim_thread = thread::spawn(move || {
            claim_thread_start.wait();
            claim_pool.claim_checkpoint_ownership()
        });
        claim_start.wait();

        {
            let mut state = pool.checkpoint_ownership.state.lock();
            while state.phase != CheckpointOwnership::Claiming {
                pool.checkpoint_ownership.changed.wait(&mut state);
            }
        }

        let open_start = Arc::new(std::sync::Barrier::new(2));
        let open_pool = Arc::clone(&pool);
        let open_thread_start = Arc::clone(&open_start);
        let open_thread = thread::spawn(move || {
            open_thread_start.wait();
            let conn = open_pool
                .open_standalone_writer()
                .expect("standalone writer after claim resolution");
            wal_autocheckpoint_pages(&conn)
        });
        open_start.wait();

        {
            let mut state = pool.checkpoint_ownership.state.lock();
            while state.connection_waiters == 0 {
                pool.checkpoint_ownership.changed.wait(&mut state);
            }
            assert_eq!(state.phase, CheckpointOwnership::Claiming);
        }

        drop(held_writer);
        claim_thread
            .join()
            .expect("claim thread joins")
            .expect("claim succeeds");
        assert_eq!(
            open_thread.join().expect("standalone-open thread joins"),
            0,
            "a writer open concurrent with a successful claim must inherit claimed ownership"
        );
    }

    #[test]
    fn standalone_fallback_application_linearizes_before_claim_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint_open_before_claim.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                checkout_timeout: Duration::from_secs(5),
                write_queue_enabled: Some(false),
                ..PoolConfig::default()
            })
            .expect("pool open"),
        );
        let pause = Arc::new(CheckpointConnectionConfigPause::new());
        *pool.checkpoint_ownership.connection_config_pause.lock() = Some(Arc::clone(&pause));

        let open_pool = Arc::clone(&pool);
        let open_thread = thread::spawn(move || {
            let conn = open_pool
                .open_standalone_writer()
                .expect("standalone writer opens");
            wal_autocheckpoint_pages(&conn)
        });
        pause.selected.wait();
        assert!(
            pool.checkpoint_ownership.state.try_lock().is_none(),
            "standalone selection must retain the ownership gate until its PRAGMA is applied"
        );

        let (claim_observed_tx, claim_observed_rx) = std::sync::mpsc::sync_channel(0);
        *pool.checkpoint_ownership.claim_lock_observed.lock() = Some(claim_observed_tx);
        let claim_pool = Arc::clone(&pool);
        let claim_thread = thread::spawn(move || claim_pool.claim_checkpoint_ownership());
        assert!(
            claim_observed_rx
                .recv()
                .expect("claim reports whether it observed gate contention"),
            "the claim must attempt the gate between fallback selection and PRAGMA application"
        );
        pause.resume.wait();

        assert_eq!(
            open_thread.join().expect("standalone-open thread joins"),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            "an open linearized before the claim keeps the fallback"
        );
        claim_thread
            .join()
            .expect("claim thread joins")
            .expect("claim succeeds after standalone configuration");
        assert_eq!(pool.effective_wal_autocheckpoint_pages(), 0);
    }

    #[test]
    fn failed_checkpoint_ownership_claim_keeps_fallback_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkpoint_claim_retry.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            checkout_timeout: Duration::from_millis(1),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("pool open");

        let legacy_conn = pool.legacy_conn();
        let held_writer = legacy_conn.lock();
        let error = pool
            .claim_checkpoint_ownership()
            .expect_err("the held pooled writer must make the claim time out");
        assert!(matches!(
            error,
            SqliteError::WriterPoolCheckoutTimeout { .. }
        ));
        assert_eq!(
            pool.effective_wal_autocheckpoint_pages(),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            "a failed claim must leave later writer connections fallback-safe"
        );

        let fallback_writer = pool
            .open_standalone_writer()
            .expect("standalone writer after failed claim");
        assert_eq!(
            wal_autocheckpoint_pages(&fallback_writer),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES
        );
        drop(fallback_writer);

        drop(held_writer);
        pool.claim_checkpoint_ownership()
            .expect("the ownership claim remains retryable");
        assert_eq!(pool.effective_wal_autocheckpoint_pages(), 0);
        let writer = pool.writer().expect("pooled writer after successful retry");
        assert_eq!(wal_autocheckpoint_pages(writer.conn()), 0);
    }

    #[test]
    fn threshold_crossing_commits_do_not_run_an_implicit_checkpoint_once_claimed() {
        const FORMER_AUTOCHECKPOINT_THRESHOLD_PAGES: i64 = FALLBACK_WAL_AUTOCHECKPOINT_PAGES as i64;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_implicit_checkpoint.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("pool open");
        pool.claim_checkpoint_ownership()
            .expect("claim ownership for the dedicated-owner posture");
        let writer = pool.writer().expect("pooled writer");
        writer
            .execute_batch("CREATE TABLE blobs (value BLOB NOT NULL)")
            .expect("create fixture table");

        let page_size: i64 = writer
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .expect("read page size");
        let payload_bytes = page_size * 32;
        for _ in 0..160 {
            writer
                .execute(
                    "INSERT INTO blobs (value) VALUES (zeroblob(?1))",
                    [payload_bytes],
                )
                .expect("autocommit fixture row");
        }

        let log_frames: i64 = writer
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(1))
            .expect("observe WAL frame count");
        assert!(
            log_frames > FORMER_AUTOCHECKPOINT_THRESHOLD_PAGES,
            "the commit sequence must retain more than the former automatic threshold; \
             observed {log_frames} frames"
        );
    }

    /// The other half of the ownership model: a writable pool that no
    /// checkpoint task ever claims must retain SQLite's own bounded WAL
    /// reclamation. The same commit sequence that retains >4,000 frames under
    /// a claimed owner must NOT accumulate them here — an implicit
    /// autocheckpoint fires on the threshold-crossing commit and drains the
    /// WAL, which is the regression guard against unbounded WAL growth (and
    /// eventual disk exhaustion) on embedded / one-shot writable pools.
    #[test]
    fn unclaimed_pool_retains_bounded_autocheckpoint_reclamation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded_fallback_reclamation.db");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("pool open");
        let writer = pool.writer().expect("pooled writer");
        writer
            .execute_batch("CREATE TABLE blobs (value BLOB NOT NULL)")
            .expect("create fixture table");

        let page_size: i64 = writer
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .expect("read page size");
        let payload_bytes = page_size * 32;
        for _ in 0..160 {
            writer
                .execute(
                    "INSERT INTO blobs (value) VALUES (zeroblob(?1))",
                    [payload_bytes],
                )
                .expect("autocommit fixture row");
        }

        // No PASSIVE pass here — read the frame count via wal_checkpoint's
        // log column only after the fixture, exactly as the claimed-owner
        // test does. With the bounded fallback live, the autocheckpoint that
        // fired on a threshold-crossing commit already drained the WAL, so
        // far fewer than the threshold's frames remain.
        let log_frames: i64 = writer
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(1))
            .expect("observe WAL frame count");
        assert!(
            log_frames < FALLBACK_WAL_AUTOCHECKPOINT_PAGES as i64,
            "an unclaimed pool must reclaim WAL frames via the bounded autocheckpoint; \
             observed {log_frames} retained frames"
        );
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
    #[serial]
    fn explicit_true_on_memory_pool_warns_but_false_and_none_do_not() {
        let _pool_env = clear_pool_env();
        let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = WarningCapture {
            messages: Arc::clone(&messages),
        };

        tracing::subscriber::with_default(subscriber, || {
            let _explicit_true = ConnectionPool::new(PoolConfig {
                path: None,
                write_queue_enabled: Some(true),
                ..PoolConfig::default()
            })
            .expect("in-memory pool should open");
            let _explicit_false = ConnectionPool::new(PoolConfig {
                path: None,
                write_queue_enabled: Some(false),
                ..PoolConfig::default()
            })
            .expect("in-memory pool should open");
            let _unset = ConnectionPool::new(PoolConfig {
                path: None,
                write_queue_enabled: None,
                ..PoolConfig::default()
            })
            .expect("in-memory pool should open");
        });

        let messages = messages.lock().unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.contains("write queue explicitly requested"))
                .count(),
            1,
            "only an explicit in-memory queue request should warn: {messages:?}"
        );
        let warning = messages
            .iter()
            .find(|message| message.contains("write queue explicitly requested"))
            .expect("explicit in-memory queue warning should be captured");
        assert!(
            warning.contains("in-memory pools cannot host a writer task"),
            "warning must explain why the request is inert: {messages:?}"
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

    /// #1847: strict store routing must preserve the typed missing-runtime
    /// failure instead of collapsing it into a direct-writer fallback.
    #[test]
    fn strict_writer_task_for_write_preserves_missing_runtime_error() {
        let dir = tempfile::tempdir().unwrap();
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("strict_writer_task_no_runtime.db")),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::default()
        })
        .expect("file-backed pool should open");

        let result = pool.writer_task_for_write(None, "strict_test_write");

        assert!(
            matches!(result, Err(StorageError::WriterTaskNoRuntime)),
            "strict routing must preserve WriterTaskNoRuntime, got {result:?}"
        );
        assert_eq!(pool.writer_task_spawn_count(), 0);
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
        assert!(!pool.writer_task_join_was_stored());
        pool.writer_task_handle()
            .expect("runtime is present")
            .expect("write queue enabled must spawn a writer task");
        assert!(pool.writer_task_join_was_stored());

        let join = pool
            .take_writer_task_join()
            .expect("the first take must return the spawned task's JoinHandle");
        assert!(
            pool.take_writer_task_join().is_none(),
            "the second take must return None — the handle is one-shot"
        );

        // Await the taken handle before the test exits instead of dropping
        // it detached. The writer task only exits once every
        // `WriterTaskHandle` clone (the mpsc senders) is gone, and the pool's
        // own `writer_task` OnceLock holds one, so the pool must drop first —
        // the same drop-then-await order the batch-ingest drain relies on.
        drop(pool);
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("the writer task must exit once every handle clone is dropped")
            .expect("the writer task must not panic");
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

    /// The at-most-once guard holds across the TAKEN state too: once the
    /// handle has been taken, the slot is empty, but a second store is still
    /// a construction bug and must trip the same debug_assert (the
    /// `writer_task_join_stored` flag remembers the first store).
    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "writer task JoinHandle stored twice")]
    async fn set_writer_task_join_second_store_after_take_trips_debug_assert() {
        let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool should open");
        pool.set_writer_task_join(tokio::spawn(async {}));
        assert!(pool.take_writer_task_join().is_some());
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

        #[cfg(unix)]
        let dir_symlink = dir.path().join("dir_link");
        #[cfg(unix)]
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

        #[cfg(unix)]
        let dir_symlink = dir.path().join("dir_link");
        #[cfg(unix)]
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
