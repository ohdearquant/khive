//! Connection pool for SQLite: one exclusive writer, N concurrent readers.
mod acquisition;
mod connection_setup;
mod construction;
#[cfg(test)]
mod external_writer_fixture;
mod guards;
mod identity;
mod pool_impl;
mod reader_counters;
mod standalone_checkpoint;
#[cfg(test)]
pub(crate) use external_writer_fixture::open_external_writer_fixture;

pub(crate) use connection_setup::open_read_only_snapshot_connection;
#[cfg(feature = "namespace-trigram-proto")]
use connection_setup::register_namespace_trigram;
#[cfg(test)]
use connection_setup::sqlite_header_uses_wal;
use connection_setup::{
    close_connection_quietly, configure_reader_connection, configure_writer_connection,
    current_journal_mode, effective_reader_count, open_reader_connection, open_writer_connection,
    pool_exhausted_error, read_only_open_target, reader_connection_is_healthy,
    reset_reader_connection, restore_shared_reader_state, validate_wal_ceiling_at_open,
    writer_open_flags, writer_read_only_open_flags,
};
#[cfg(any(unix, windows))]
use identity::database_file_identity_if_exists;
#[cfg(test)]
use identity::initialize_database_id;
#[cfg(any(unix, windows))]
pub(crate) use identity::opened_sqlite_file_identity;
use identity::{initialize_database_id_with_admission, mint_db_identity, read_database_id};

use guards::{PooledReader, ReaderLease};
pub(crate) use guards::{ReaderAdmission, SharedReaderTransactionGuard};
pub use guards::{ReaderGuard, ReaderRow};
use reader_counters::ReaderAcquisitionCounters;
pub use reader_counters::ReaderAcquisitionSnapshot;
pub(crate) use reader_counters::StandaloneReaderPurpose;
pub(crate) use standalone_checkpoint::CheckpointFile;
pub use standalone_checkpoint::StandaloneCheckpoint;

mod code_map;
mod sql_functions;
#[cfg(test)]
pub(crate) use sql_functions::register_rfc3339_key;
use sql_functions::{register_read_functions, register_writer_clock};
pub(crate) use sql_functions::{rfc3339_instant_key, strict_rfc3339_key};
#[path = "pool/identity_registry.rs"]
mod identity_registry;
#[cfg(any(test, feature = "test-support"))]
#[path = "pool/test_volume_lock_dir.rs"]
mod test_volume_lock_dir;
#[path = "pool/write_units.rs"]
mod write_units;
#[path = "pool/writer_acquisition.rs"]
mod writer_acquisition;
#[cfg(test)]
use identity_registry::pool_identity_suffix;
use identity_registry::PoolIdentityRegistration;
#[cfg(any(test, feature = "test-support"))]
use test_volume_lock_dir::test_volume_lock_dir;
#[cfg(test)]
use write_units::STARTUP_SPACE_PROBE;
pub use write_units::{CheckpointGuard, CheckpointResult, WriterAcquisitionSnapshot, WriterGuard};
pub(crate) use write_units::{
    PooledAutocommitWriteUnit, PooledTransactionWriteUnit, StandaloneWriteUnit, WriteAdmission,
    WriterAcquisitionCounters,
};

use crossbeam_queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use rusqlite::hooks::{AuthContext, Authorization};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

#[cfg(test)]
use crate::database_owner_identity::DatabaseOwnerIdentityError;
use crate::disk_guard::{LeaseRefusal, VolumeIdentity, VolumeLease};
use crate::disk_guard_config::{EffectiveDiskGuardConfig, DEFAULT_DISK_GUARD_DEADLINE_MS};
use crate::error::SqliteError;
#[cfg(all(test, any(unix, windows)))]
use crate::file_identity::database_file_identity;
#[cfg(any(unix, windows))]
use crate::file_identity::DatabaseFileIdentity;
use crate::writer_task::{execute_wrapped_transaction, WriterTaskHandle};
use khive_storage::error::StorageError;
use khive_storage::tx_registry::TxOrigin;
use khive_storage::CapacityUnavailablePhase;
use khive_storage::StorageCapability;

mod claimed_file_identity;

#[cfg(unix)]
mod claimed_file_observer;

#[cfg(unix)]
pub use claimed_file_observer::initialize as initialize_claimed_file_observer;

#[cfg(all(test, unix))]
#[path = "pool/claimed_file_identity_tests.rs"]
mod claimed_file_identity_tests;

/// Default SQLite page cache per pooled connection, in KiB.
///
/// Every connection keeps its own page cache on the process heap, so a
/// process's ceiling is this value times its open connections, and a daemon
/// serving several databases holds dozens of them. Pages the cache drops are
/// re-read from the operating system's file cache, which the kernel reclaims
/// on its own. `KHIVE_SQLITE_CACHE_KIB` overrides it within
/// `SQLITE_CACHE_KIB_RANGE`.
pub(crate) const DEFAULT_SQLITE_CACHE_KIB: i64 = 8 * 1024;
pub(crate) const SQLITE_CACHE_KIB_RANGE: std::ops::RangeInclusive<i64> = 512..=1_048_576;
const SQLITE_CACHE_KIB_ENV: &str = "KHIVE_SQLITE_CACHE_KIB";

/// Default SQLite memory-map window per pooled connection, in bytes.
///
/// Mapped pages are clean and file-backed: the kernel drops them under memory
/// pressure without writing them anywhere. Reads past the window go through
/// the page cache above instead. `KHIVE_SQLITE_MMAP_BYTES` overrides it
/// within `SQLITE_MMAP_BYTES_RANGE`; `0` turns memory-mapped reads off.
pub(crate) const DEFAULT_SQLITE_MMAP_BYTES: i64 = 1 << 30;
pub(crate) const SQLITE_MMAP_BYTES_RANGE: std::ops::RangeInclusive<i64> = 0..=(1 << 40);
const SQLITE_MMAP_BYTES_ENV: &str = "KHIVE_SQLITE_MMAP_BYTES";
const DEFAULT_READER_CAP: usize = 8;

/// The `cache_size` pragma value every pooled connection is configured with
/// (negative, so SQLite reads it as KiB), resolved once per process.
pub(crate) fn sqlite_cache_size_pragma() -> i64 {
    static KIB: OnceLock<i64> = OnceLock::new();
    -*KIB.get_or_init(|| {
        sqlite_size_setting(
            SQLITE_CACHE_KIB_ENV,
            sqlite_size_env(SQLITE_CACHE_KIB_ENV).as_deref(),
            DEFAULT_SQLITE_CACHE_KIB,
            &SQLITE_CACHE_KIB_RANGE,
        )
    })
}

/// The `mmap_size` pragma value every pooled connection is configured with,
/// resolved once per process.
pub(crate) fn sqlite_mmap_size_pragma() -> i64 {
    static BYTES: OnceLock<i64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        sqlite_size_setting(
            SQLITE_MMAP_BYTES_ENV,
            sqlite_size_env(SQLITE_MMAP_BYTES_ENV).as_deref(),
            DEFAULT_SQLITE_MMAP_BYTES,
            &SQLITE_MMAP_BYTES_RANGE,
        )
    })
}

/// Reads a size setting with `var_os`, not `var`: a set value that is not valid
/// Unicode still counts as set, so it reaches the range check and is logged
/// rather than silently taking the default.
fn sqlite_size_env(name: &str) -> Option<String> {
    std::env::var_os(name).map(|raw| raw.to_string_lossy().into_owned())
}

/// An unset setting takes the default. A set value that is not an integer
/// inside `range` also takes the default, and says so in the log, because a
/// typo in a memory setting must not stop the store from opening.
pub(crate) fn sqlite_size_setting(
    name: &str,
    raw: Option<&str>,
    default: i64,
    range: &std::ops::RangeInclusive<i64>,
) -> i64 {
    let Some(raw) = raw else {
        return default;
    };
    match raw.parse::<i64>() {
        Ok(value) if range.contains(&value) => value,
        _ => {
            tracing::warn!(
                setting = name,
                value = raw,
                default,
                min = *range.start(),
                max = *range.end(),
                "ignoring a SQLite size setting outside its range; using the default"
            );
            default
        }
    }
}

const DEFAULT_JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864; // 64 MiB
const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 256;
const DATABASE_ID_TABLE: &str = "_khive_database_identity";
static NEXT_MAIN_POOL_GENERATION: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdentityOpenStage {
    AfterMainOpenBeforeFirstStat,
    AfterInitialIdentityWrite,
    BeforeStandaloneOpen,
    AfterStandaloneOpen,
}

#[cfg(test)]
type IdentityOpenHook = Box<dyn Fn(&Path, IdentityOpenStage, Option<&Connection>)>;

#[cfg(test)]
thread_local! {
    static IDENTITY_OPEN_HOOK: std::cell::RefCell<Option<IdentityOpenHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn run_identity_open_hook(path: &Path, stage: IdentityOpenStage, conn: Option<&Connection>) {
    IDENTITY_OPEN_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(path, stage, conn);
        }
    });
}

#[path = "pool/admission.rs"]
mod admission;
use admission::CheckpointOwnershipGate;
pub use admission::RuntimeWriteOperation;
pub(crate) use admission::FALLBACK_WAL_AUTOCHECKPOINT_PAGES;
#[cfg(test)]
use admission::{CheckpointConnectionConfigPause, CheckpointOwnership};

fn deny_retired_writer(_context: AuthContext<'_>) -> Authorization {
    Authorization::Deny
}

mod config;

pub(crate) use config::TEST_HARNESS_ENV;
use config::{refuse_home_data_store_in_tests, validate_write_admission_deadline};
pub use config::{PoolConfig, WalCeilingPolicy, WalCeilingSource};

/// Pool-scoped counters for ADR-166's search mechanism guards.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct SearchMechanismSnapshot {
    /// Coordinator calls actually issued to each registered backend, keyed by
    /// the request's canonical kind. A backend skipped by served-kind routing
    /// has no entry for that kind.
    pub dispatches_by_backend_and_kind: BTreeMap<String, BTreeMap<String, u64>>,
    /// Candidate note rows fetched after the text/vector fusion and fresh-tail
    /// merge, including rows later filtered as deleted. Result metadata fetched
    /// later by the KG handler is excluded.
    pub note_candidate_hydration_rows: u64,
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
    // Prepared before admission so terminal cleanup cannot fail while allocating a replacement.
    retirement_connection: Mutex<Option<Connection>>,
    #[cfg(any(test, feature = "test-support"))]
    statement_observer: Arc<crate::statement_observer::StatementObserverHub>,
    main_pool_generation: OnceLock<u64>,
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
    /// Shared with the long-lived writer task so it can resample at every
    /// dequeued request rather than only when its connection is opened.
    write_admission: Arc<WriteAdmission>,
    /// Effective source and values captured at open for diagnostic reporting;
    /// the live volume identifier is deliberately resolved separately.
    disk_guard_config: Option<EffectiveDiskGuardConfig>,
    /// Pool-scoped reader route, saturation, and hold-lifecycle counters.
    /// Instrumentation lives at the acquisition boundary so every typed
    /// store and raw-SQL caller inherits it without per-verb bookkeeping
    /// (ADR-165 Slice 2 / ADR-166 G2).
    reader_acquisition_counters: ReaderAcquisitionCounters,
    reader_checkouts: Mutex<BTreeMap<Instant, usize>>,
    /// ADR-166 G4/G5 process-lifetime mechanism counters for this physical
    /// backend. Backend IDs remain separate even when aliases share a pool.
    search_dispatches: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
    note_candidate_hydration_rows: AtomicU64,
    readers: ArrayQueue<Box<PooledReader>>,
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
    /// The physical file SQLite opened, checked against the canonical path.
    /// Later reader and standalone opens must retain this identity.
    #[cfg(any(unix, windows))]
    opened_file_identity: Option<DatabaseFileIdentity>,
    /// A persistent nonce read through SQLite's opened main database, rather
    /// than through the pathname that may have been replaced during open.
    opened_database_id: Option<uuid::Uuid>,
    /// Registered only after every connection opens successfully. RAII removes
    /// the path when the last pool for it drops, including failed construction.
    identity_registration: Option<PoolIdentityRegistration>,
    /// Test-only instrumentation: counts how many times the writer-task
    /// init closure actually ran. Must never exceed 1 per pool no matter how
    /// many stores are constructed over it — that is the invariant
    /// `OnceLock::get_or_init` exists to guarantee, and what
    /// `pool.rs`'s and `entity_tests.rs`'s one-writer-per-pool tests assert.
    #[cfg(test)]
    writer_task_spawn_count: std::sync::atomic::AtomicUsize,
}

impl Drop for ConnectionPool {
    /// Close every read-only reader before the fields below it drop in
    /// declaration order (`writer` first, `readers` well before
    /// `writer_task`). A read-only connection cannot take the EXCLUSIVE lock
    /// SQLite needs to checkpoint on close, so if a reader were left to close
    /// last, WAL mode would leave `-wal`/`-shm` behind. Draining `readers`
    /// here, before that field-order drop runs, makes the writable `writer`
    /// connection close after every reader instead of before it.
    fn drop(&mut self) {
        while let Some(conn) = self.readers.pop() {
            drop(conn);
        }
    }
}

#[cfg(test)]
#[path = "runtime_write_routing_tests.rs"]
mod runtime_write_routing_tests;

#[cfg(test)]
#[path = "database_owner_identity_pool_tests.rs"]
mod database_owner_identity_pool_tests;

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pool/recycling_tests.rs"]
mod recycling_tests;

#[cfg(all(test, any(unix, windows)))]
#[path = "pool_identity_admission_tests.rs"]
mod identity_admission_tests;
