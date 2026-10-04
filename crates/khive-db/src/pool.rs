//! Connection pool for SQLite: one exclusive writer, N concurrent readers.
#[path = "pool/writer_acquisition.rs"]
mod writer_acquisition;

use crossbeam_queue::ArrayQueue;
use parking_lot::{Condvar, Mutex};
use rusqlite::hooks::{AuthContext, Authorization};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read as _;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

use crate::database_owner_identity::{DatabaseOwnerIdentity, DatabaseOwnerIdentityError};
use crate::error::SqliteError;
#[cfg(windows)]
use crate::file_identity::sqlite_opened_file_identity;
#[cfg(any(unix, windows))]
use crate::file_identity::{database_file_identity, DatabaseFileIdentity};
use crate::writer_task::WriterTaskHandle;
use khive_storage::error::StorageError;
use khive_storage::tx_registry::{DbIdentity, TxOrigin};
use khive_storage::StorageCapability;

const CACHE_SIZE_KIB: &str = "-65536";
const MMAP_SIZE_BYTES: &str = "1073741824";
const DEFAULT_READER_CAP: usize = 8;

const DEFAULT_JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864; // 64 MiB
const DEFAULT_WRITE_QUEUE_CAPACITY: usize = 256;
const DB_FREE_SPACE_FLOOR_ENV: &str = "KHIVE_DB_FREE_SPACE_FLOOR_BYTES";
const DEFAULT_DB_FREE_SPACE_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;
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

#[cfg(test)]
type SpaceProbe = dyn Fn(&Path) -> std::io::Result<u64> + Send + Sync;

#[cfg(test)]
thread_local! {
    static STARTUP_SPACE_PROBE: std::cell::RefCell<Option<(u64, Arc<SpaceProbe>)>> =
        const { std::cell::RefCell::new(None) };
}

/// The SQLite write reserve is sampled at each operation admission. SQLite
/// does not expose the size of an arbitrary upcoming transaction, so the
/// reserve is a warning boundary, not a guarantee that a single very large
/// transaction cannot consume more than the remaining headroom.
pub(crate) struct WriteAdmission {
    volume: Option<PathBuf>,
    floor_bytes: u64,
    #[cfg(test)]
    space_probe: Mutex<Option<Arc<SpaceProbe>>>,
}

impl WriteAdmission {
    fn new(volume: Option<PathBuf>, floor_bytes: u64) -> Self {
        #[cfg(test)]
        let (floor_bytes, space_probe) = STARTUP_SPACE_PROBE.with(|probe| {
            probe
                .borrow()
                .as_ref()
                .map(|(floor, probe)| (*floor, Some(Arc::clone(probe))))
                .unwrap_or((floor_bytes, None))
        });
        Self {
            volume,
            floor_bytes,
            #[cfg(test)]
            space_probe: Mutex::new(space_probe),
        }
    }

    fn available_space(&self, volume: &Path) -> std::io::Result<u64> {
        #[cfg(test)]
        if let Some(probe) = self.space_probe.lock().as_ref() {
            return probe(volume);
        }
        fs4::available_space(volume)
    }

    pub(crate) fn check(&self) -> Result<(), SqliteError> {
        let Some(volume) = self.volume.as_deref() else {
            return Ok(());
        };
        if self.floor_bytes == 0 {
            return Ok(());
        }
        let available = self.available_space(volume)?;
        // SQL does not tell admission how many bytes the next transaction
        // will append. At equality, even its first byte would cross the floor.
        if available <= self.floor_bytes {
            return Err(SqliteError::CapacityFloor {
                volume: volume.display().to_string(),
                available_bytes: available,
                floor_bytes: self.floor_bytes,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn set_test_space_probe(
        &self,
        probe: impl Fn(&Path) -> std::io::Result<u64> + Send + Sync + 'static,
    ) {
        *self.space_probe.lock() = Some(Arc::new(probe));
    }
}

fn db_free_space_floor_from_env() -> Result<u64, SqliteError> {
    let Some(value) = std::env::var_os(DB_FREE_SPACE_FLOOR_ENV) else {
        return Ok(DEFAULT_DB_FREE_SPACE_FLOOR_BYTES);
    };
    let parsed = value
        .to_str()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            SqliteError::InvalidConfig(format!(
                "{DB_FREE_SPACE_FLOOR_ENV} must be a nonnegative byte count"
            ))
        })?;
    Ok(parsed)
}

struct OpenPoolIdentity {
    count: usize,
    basename: String,
    suffix: String,
}

/// Only final file names and the first eight SHA-256 hex digits enter errors.
/// Canonical paths remain internal to the live-pool registry.
#[derive(Default)]
struct PoolIdentityRegistry {
    paths: HashMap<PathBuf, OpenPoolIdentity>,
}

fn pool_identity_registry() -> &'static Mutex<PoolIdentityRegistry> {
    static REGISTRY: OnceLock<Mutex<PoolIdentityRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PoolIdentityRegistry::default()))
}

/// SHA-256 over raw Unix path bytes, or Windows UTF-16 code units in little
/// endian order. Encoding and digest are explicit so toolchain upgrades and
/// process restarts cannot change a given canonical path's suffix.
fn pool_identity_suffix(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    };
    #[cfg(not(any(unix, windows)))]
    let bytes = path.to_string_lossy().as_bytes().to_vec();
    let digest = Sha256::digest(&bytes);
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

struct PoolIdentityRegistration(PathBuf);

impl PoolIdentityRegistration {
    fn new(path: &Path) -> Self {
        let mut registry = pool_identity_registry().lock();
        if let Some(entry) = registry.paths.get_mut(path) {
            entry.count += 1;
        } else {
            let basename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let suffix = pool_identity_suffix(path);
            registry.paths.insert(
                path.to_path_buf(),
                OpenPoolIdentity {
                    count: 1,
                    basename,
                    suffix,
                },
            );
        }
        Self(path.to_path_buf())
    }

    fn label(&self) -> String {
        let registry = pool_identity_registry().lock();
        let entry = &registry.paths[&self.0];
        let collides = registry
            .paths
            .iter()
            .any(|(path, other)| path != &self.0 && other.basename == entry.basename);
        if collides {
            format!("{}#{}", entry.basename, entry.suffix)
        } else {
            entry.basename.clone()
        }
    }
}

impl Drop for PoolIdentityRegistration {
    fn drop(&mut self) {
        let mut registry = pool_identity_registry().lock();
        if let Some(entry) = registry.paths.get_mut(&self.0) {
            entry.count -= 1;
            if entry.count == 0 {
                registry.paths.remove(&self.0);
            }
        }
    }
}

/// Runtime-owned SQL transactions that share the store write-routing policy.
#[derive(Clone, Copy, Debug)]
pub enum RuntimeWriteOperation {
    MergeEntity,
    MergeNote,
    UpdateSymmetricEdge,
}

impl RuntimeWriteOperation {
    fn operation(self) -> &'static str {
        match self {
            Self::MergeEntity => "merge_entity",
            Self::MergeNote => "merge_note",
            Self::UpdateSymmetricEdge => "update_edge",
        }
    }

    fn fallback_site(self) -> crate::timeout_sink::Site {
        match self {
            Self::MergeEntity => crate::timeout_sink::Site::DirectRouteRuntimeMergeEntity,
            Self::MergeNote => crate::timeout_sink::Site::DirectRouteRuntimeMergeNote,
            Self::UpdateSymmetricEdge => {
                crate::timeout_sink::Site::DirectRouteRuntimeUpdateSymmetricEdge
            }
        }
    }
}

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

/// Where the effective WAL ceiling byte value was configured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WalCeilingSource {
    BackendField,
    Environment,
    #[default]
    Default,
}

/// Resolved WAL-extent policy for one SQLite backend. A zero-byte policy is
/// explicitly disabled; it remains visible in diagnostics and config identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WalCeilingPolicy {
    pub bytes: u64,
    pub source: WalCeilingSource,
}

impl WalCeilingPolicy {
    /// Validate checks that do not need SQLite's page-size observation.
    /// Read-only backends retain configured metadata but enforce no writer
    /// policy. Invalid offset arithmetic is rejected in either mode.
    pub fn validate_static(
        self,
        file_backed: bool,
        wal_mode: bool,
        read_only: bool,
    ) -> Result<(), SqliteError> {
        if self.bytes == 0 {
            return Ok(());
        }
        if i64::try_from(self.bytes).is_err() {
            return Err(SqliteError::WalCeilingOffsetOverflow { bytes: self.bytes });
        }
        if read_only {
            return Ok(());
        }
        if !file_backed {
            return Err(SqliteError::WalCeilingUnsupported {
                bytes: self.bytes,
                backend_kind: "in-memory backend",
            });
        }
        if !wal_mode {
            return Err(SqliteError::WalCeilingUnsupported {
                bytes: self.bytes,
                backend_kind: "non-WAL backend",
            });
        }
        Ok(())
    }

    /// Bytes of writer policy that could be enforced on this backend.
    pub fn effective_bytes(self, read_only: bool) -> u64 {
        if read_only {
            0
        } else {
            self.bytes
        }
    }
}

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
    /// ADR-194 WAL active-extent ceiling and its resolved configuration source.
    /// Zero explicitly disables this independent policy.
    pub wal_ceiling: WalCeilingPolicy,
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
    /// single-writer guarantee, and the strict default is still evidence-gated.
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
    /// Maximum age an explicit cached-reader read transaction
    /// (`sql_bridge`'s `BEGIN`-then-reuse path) may reach before its next use
    /// is refused and it is rolled back instead of extending its WAL
    /// snapshot further (#1846). Shares `KHIVE_TX_MAX_AGE_SECS` with the
    /// ADR-091 Plank 1 visibility sweep in `checkpoint.rs` so one knob
    /// governs both when an operator is warned about a stale reader and when
    /// that reader's snapshot is actually released.
    ///
    /// Overridable via `KHIVE_TX_MAX_AGE_SECS`. Default: 120 seconds.
    pub read_tx_max_age: Duration,
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
            wal_ceiling: WalCeilingPolicy::default(),
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
            read_tx_max_age: crate::checkpoint::tx_age_thresholds_from_env(
                Duration::from_secs(30),
                Duration::from_secs(120),
            )
            .1,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl PoolConfig {
    /// A small concurrent pool for private test databases.
    ///
    /// Tests of reader admission or production sizing should set their required
    /// count explicitly. Ordinary fixtures need not reserve a CPU-sized pool.
    pub fn for_test() -> Self {
        Self {
            max_readers: 2,
            ..Self::default()
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
    /// Pool-scoped reader route, saturation, and hold-lifecycle counters.
    /// Instrumentation lives at the acquisition boundary so every typed
    /// store and raw-SQL caller inherits it without per-verb bookkeeping
    /// (ADR-165 Slice 2 / ADR-166 G2).
    reader_acquisition_counters: ReaderAcquisitionCounters,
    /// ADR-166 G4/G5 process-lifetime mechanism counters for this physical
    /// backend. Backend IDs remain separate even when aliases share a pool.
    search_dispatches: Mutex<BTreeMap<String, BTreeMap<String, u64>>>,
    note_candidate_hydration_rows: AtomicU64,
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

enum ReaderLease<'pool> {
    Pooled(Connection),
    Shared(parking_lot::MutexGuard<'pool, Connection>),
}

/// A value-extraction view of one row from a reader lease.
///
/// It deliberately exposes neither the prepared statement nor its connection.
///
/// ```compile_fail
/// fn statement(row: &khive_db::ReaderRow<'_, '_>) {
///     let _: &rusqlite::Statement<'_> = row.as_ref();
/// }
/// ```
pub struct ReaderRow<'row, 'statement> {
    row: &'row rusqlite::Row<'statement>,
}

impl ReaderRow<'_, '_> {
    /// Extract a value by zero-based column index or column name.
    pub fn get<I: rusqlite::RowIndex, T: rusqlite::types::FromSql>(
        &self,
        index: I,
    ) -> rusqlite::Result<T> {
        self.row.get(index)
    }

    /// Borrow a SQLite value without exposing statement metadata or execution.
    pub fn get_ref<I: rusqlite::RowIndex>(
        &self,
        index: I,
    ) -> rusqlite::Result<rusqlite::types::ValueRef<'_>> {
        self.row.get_ref(index)
    }
}

/// One public query owns this lease's connection-global progress handler.
struct ReaderQueryInProgress<'a>(&'a Cell<bool>);

impl Drop for ReaderQueryInProgress<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// One pool-wide reader admission permit acquired before any connection is
/// selected, with the instant its checkout wait began so a single
/// `checkout_timeout` bounds both the permit wait and the connection pick.
/// Hand it to [`ConnectionPool::reader_with_admission`], which moves the permit
/// into the resulting [`ReaderGuard`].
pub(crate) struct ReaderAdmission {
    slot: tokio::sync::OwnedSemaphorePermit,
    started: Instant,
}

/// A reader connection checked out from the pool.
/// Returns the connection to the pool on drop.
pub struct ReaderGuard<'pool> {
    lease: Option<ReaderLease<'pool>>,
    /// One permit from the pool-wide reader budget, shared with the explicit
    /// raw-SQL transaction exception. Returned only after the connection has
    /// been reset/replaced and made reusable.
    admission_slot: Option<tokio::sync::OwnedSemaphorePermit>,
    pool: &'pool ConnectionPool,
    reusable: Cell<bool>,
    query_in_progress: Cell<bool>,
    checked_out_at: Instant,
    /// Set by [`Self::mark_dirty`] whenever this checkout ran a `SqlReader`
    /// raw-SQL statement (`sql_bridge`'s `run_pool_reader_query`), never by a
    /// typed store read. Gates whether `Drop` pays the TEMP-object,
    /// attachment, and connection-setting pristine scan on return —
    /// `reader_connection_state_is_pristine` and
    /// `reader_connection_settings_match_baseline` never touch the hot path
    /// of an ordinary typed checkout.
    dirty: Cell<bool>,
    /// Names the typed-store operation this checkout was resolved for, set
    /// once by [`ConnectionPool::resolve_reader_checkout`]. `None` means the
    /// checkout never passed that route (the pool-internal and raw-SQL
    /// callers), and the diagnostics maximum reports it as unattributed
    /// rather than guessing.
    operation: Option<&'static str>,
}

impl<'pool> ReaderGuard<'pool> {
    /// Access the connection from within `khive-db`. Every internal caller
    /// is either a typed store (proven read-only by construction) or a
    /// raw-SQL route that already calls [`Self::mark_dirty`] itself, so this
    /// stays crate-private: it is the untracked half of the pristine-return
    /// contract, and a caller outside the crate has no way to pay into that
    /// contract by calling `mark_dirty` (also crate-private). A raw
    /// `&Connection` is never handed to a caller outside `khive-db` — use
    /// [`Self::query_row`] instead, which admits only read-shaped SQL.
    pub(crate) fn conn(&self) -> &Connection {
        match self
            .lease
            .as_ref()
            .expect("reader guard missing connection")
        {
            ReaderLease::Pooled(conn) => conn,
            ReaderLease::Shared(guard) => guard,
        }
    }

    /// Run one read-only statement against this reader lease and map the
    /// first resulting row, for callers outside `khive-db`.
    ///
    /// Unlike a raw `Connection`, this never hands out a capability that can
    /// change connection-local or database state: `sql` is checked against
    /// the same allow-listed read-shape classifier
    /// (`SELECT`/`WITH ... SELECT`/`VALUES`/`EXPLAIN`/a fixed read-only
    /// `PRAGMA` set) the pooled `SqlReader` surface admits raw SQL through,
    /// and anything else — `BEGIN`, DML, DDL, a setting `PRAGMA`, `ATTACH`
    /// — is refused before it ever reaches SQLite. An admitted statement
    /// still marks the checkout dirty unconditionally, so `Drop` always pays
    /// the pristine-state scan (or, in degraded shared-lease mode, the
    /// settings/rollback verification) on return.
    ///
    /// SQL stepping cooperatively observes the current request's cancellation
    /// and original absolute deadline. Synchronous mapper/native callback code
    /// cannot be forcibly preempted; a post-check refuses a successful result
    /// if the request stopped while that code ran.
    pub fn query_row<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T, SqliteError>
    where
        P: rusqlite::Params,
        F: FnOnce(&ReaderRow<'_, '_>) -> rusqlite::Result<T>,
    {
        crate::sql_bridge::reader_capability_admits(sql).map_err(SqliteError::InvalidData)?;
        if !self.reusable.get() {
            return Err(SqliteError::InvalidData(
                "reader lease is quarantined after failed read cleanup".into(),
            ));
        }
        // Params may call user-provided ToSql before stepping, and the mapper
        // also runs user code. Neither may replace this query's progress handler
        // by recursively querying the same lease.
        if self.query_in_progress.replace(true) {
            return Err(SqliteError::InvalidData(
                "reader lease is already executing a query".into(),
            ));
        }
        let _in_progress = ReaderQueryInProgress(&self.query_in_progress);
        self.mark_dirty();
        crate::read_cancellation::run_borrowed_reader(self, |conn, admission| {
            conn.query_row(sql, params, |row| {
                // Parameter conversion runs before any stepping, and a query
                // cheap enough not to trip the progress handler never polls at
                // all, so this is the only point that can keep the documented
                // guarantee: a cancelled request does not enter the mapper.
                // SQLITE_INTERRUPT is the code the scope already recognises, so
                // the refusal converts to the same timeout error as a
                // cancellation observed during stepping.
                if !admission.admits() {
                    return Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                        Some("request stopped before the mapper".into()),
                    ));
                }
                f(&ReaderRow { row })
            })
            .map_err(|error| {
                StorageError::driver(StorageCapability::Sql, "reader_guard.query_row", error)
            })
        })
        .map_err(|error| {
            self.pool.record_reader_query_error(&error);
            match error {
                StorageError::Driver {
                    capability,
                    operation,
                    source,
                } => match source.downcast::<rusqlite::Error>() {
                    Ok(error) => SqliteError::Rusqlite(*error),
                    Err(source) => SqliteError::RequestReadStopped(StorageError::Driver {
                        capability,
                        operation,
                        source,
                    }),
                },
                other => SqliteError::RequestReadStopped(other),
            }
        })
    }

    /// Fail closed when connection-global state could not be restored after
    /// a read. A pooled reader is closed and replaced on drop; a degraded
    /// shared-writer reader is quarantined for the lifetime of the pool.
    pub(crate) fn discard(&self) {
        self.reusable.set(false);
    }

    /// Mark this checkout as having run a raw-SQL statement, so `Drop` pays
    /// the pristine-state scan on return instead of skipping it.
    pub(crate) fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    /// Name the typed-store operation this checkout serves, so a long hold
    /// can be attributed in diagnostics instead of arriving as a bare
    /// maximum with no next step (#2793).
    pub(crate) fn label_operation(&mut self, operation: &'static str) {
        self.operation = Some(operation);
    }
}

impl<'pool> Drop for ReaderGuard<'pool> {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };

        match lease {
            ReaderLease::Pooled(conn) if self.reusable.get() => {
                self.pool.return_reader(conn, self.dirty.get())
            }
            ReaderLease::Pooled(conn) => {
                close_connection_quietly(conn);
                self.pool.replace_discarded_reader_slot();
            }
            ReaderLease::Shared(guard) if !self.reusable.get() => {
                self.pool.retire_pooled_writer(&guard);
            }
            ReaderLease::Shared(guard) => {
                // The shared lease IS the pool's writer connection (degraded
                // `max_readers == 0` mode) — there is no separate reader
                // connection to close and replace. A dirty return that
                // cannot be verifiably restored poisons the whole pool via
                // the same terminal-fault path a writer transaction fault
                // uses, since reuse and replacement are equally unavailable
                // here.
                if self.dirty.get() && !restore_shared_reader_state(&guard, &self.pool.config) {
                    self.pool.retire_pooled_writer(&guard);
                }
            }
        }

        // A checkout remains active until reset/replacement returned the
        // connection to service (or the degraded writer guard was released),
        // not merely until the caller's query closure returned.
        drop(self.admission_slot.take());
        self.pool
            .reader_acquisition_counters
            .record_checkout_completed(self.checked_out_at.elapsed(), self.operation);
    }
}

/// Owned, `'static` counterpart to [`ReaderGuard`]'s degraded-mode
/// (`max_readers == 0`) lease. `ReaderGuard` borrows `&'pool ConnectionPool`
/// and therefore cannot be retained across the separate `.await` points
/// between a caller's `SqlReader` trait-method calls — every ordinary call
/// through it draws a fresh checkout and returns it before the next call
/// begins. That is fine for an ordinary read, but wrong for the explicit
/// multi-call deferred read transaction (ADR-005/ADR-091): a `BEGIN
/// DEFERRED` checked out and returned this way releases the pool's one
/// physical connection to any concurrent caller — including a writer —
/// before its own matching `COMMIT`/`ROLLBACK` runs, and an abandoned span
/// (an error or cancellation between the two) leaves that connection sitting
/// in the pool inside an open transaction with nothing left holding it. This
/// type owns an `Arc`-rooted mutex guard instead of borrowing one, so it can
/// be held by the caller across those `.await` points and give the whole
/// span real, exclusive ownership of the one connection.
pub(crate) struct SharedReaderTransactionGuard {
    conn: parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>,
    admission_slot: Option<tokio::sync::OwnedSemaphorePermit>,
    pool: Arc<ConnectionPool>,
    checked_out_at: Instant,
    /// Set once a statement executed against this connection could not
    /// prove its own cleanup ran (SQLite progress-handler removal failure).
    /// `Drop` then treats the connection exactly like a rollback failure:
    /// poison the pool rather than let possibly-corrupted connection state
    /// return to service.
    poison: Cell<bool>,
}

impl SharedReaderTransactionGuard {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Force poisoning on drop regardless of the connection's own
    /// autocommit/pristine state.
    pub(crate) fn poison(&self) {
        self.poison.set(true);
    }
}

impl Drop for SharedReaderTransactionGuard {
    fn drop(&mut self) {
        // An abandoned span must never let this connection return to
        // service while still inside an open transaction — the next reader
        // or writer to draw the pool's one physical connection would
        // silently inherit it. Roll back first; only a verified return to
        // autocommit, followed by the same pristine-state scan an ordinary
        // dirty degraded checkout pays, allows reuse.
        let mut restored = !self.poison.get();
        if restored && !self.conn.is_autocommit() {
            restored = self.conn.execute_batch("ROLLBACK").is_ok() && self.conn.is_autocommit();
        }
        if restored {
            restored = restore_shared_reader_state(&self.conn, &self.pool.config);
        }
        if !restored {
            self.pool.retire_pooled_writer(&self.conn);
        }
        drop(self.admission_slot.take());
        self.pool
            .reader_acquisition_counters
            .record_checkout_completed(
                self.checked_out_at.elapsed(),
                Some("explicit_sql_read_transaction"),
            );
    }
}

impl ConnectionPool {
    /// Check out the degraded-mode (`max_readers == 0`) pool's single
    /// physical connection as an owned [`SharedReaderTransactionGuard`]
    /// instead of a borrowed [`ReaderGuard`]. Mirrors the admission and
    /// mutex-acquisition loop in [`Self::reader_until`]'s degraded branch —
    /// the only difference is the guard type returned, so a caller can
    /// retain this one across several separate `.await` points.
    ///
    /// Exists only for the in-memory backend's explicit deferred-read-
    /// transaction bypass (`sql_bridge::PoolBackedReader`); ordinary reads
    /// use [`Self::reader_until`].
    pub(crate) fn checkout_shared_reader_transaction(
        self: &Arc<Self>,
        should_stop: impl Fn() -> bool,
    ) -> Result<Option<SharedReaderTransactionGuard>, SqliteError> {
        debug_assert_eq!(
            self.max_readers, 0,
            "the owned shared-reader-transaction guard exists only for the degraded, \
             single-connection backend"
        );
        self.ensure_pooled_writer_active()?;
        let started = Instant::now();
        let admission_slot = loop {
            if should_stop() {
                return Ok(None);
            }
            match Arc::clone(&self.sql_bridge_reader_slots).try_acquire_owned() {
                Ok(slot) => break slot,
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(SqliteError::InvalidData(
                        "reader admission semaphore is closed".to_string(),
                    ));
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {}
            }
            if started.elapsed() >= self.config.checkout_timeout {
                self.reader_acquisition_counters.record_checkout_timeout();
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }
            thread::yield_now();
        };

        loop {
            if should_stop() {
                return Ok(None);
            }
            let remaining = self
                .config
                .checkout_timeout
                .saturating_sub(started.elapsed());
            if remaining.is_zero() {
                self.reader_acquisition_counters.record_checkout_timeout();
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }
            if let Some(conn) = self
                .writer
                .try_lock_arc_for(remaining.min(Duration::from_millis(2)))
            {
                self.ensure_pooled_writer_active()?;
                self.reader_acquisition_counters.record_pooled_checkout();
                return Ok(Some(SharedReaderTransactionGuard {
                    conn,
                    admission_slot: Some(admission_slot),
                    pool: Arc::clone(self),
                    checked_out_at: Instant::now(),
                    poison: Cell::new(false),
                }));
            }
        }
    }
}

/// Why a caller is allowed to bypass the pooled reader queue.
///
/// This is deliberately a closed, crate-private list. Ordinary request reads
/// have no variant: they must use [`ConnectionPool::reader`] and surface its
/// bounded admission timeout without falling back to a fresh connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // The closed ADR list includes infrastructure paths not instantiated today.
pub(crate) enum StandaloneReaderPurpose {
    /// ADR-005/ADR-091's explicit, multi-call deferred raw-SQL transaction.
    ExplicitSqlReadTransaction,
    /// Boot-time schema/model-registry inspection before a runtime pool can
    /// own the read. Kept separate from request traffic in diagnostics.
    BootSchemaProbe,
    /// An operator diagnostic that requires a physically independent
    /// snapshot. Kept separate from request traffic in diagnostics.
    DiagnosticsIndependentSnapshot,
}

impl StandaloneReaderPurpose {
    fn is_infrastructure(self) -> bool {
        matches!(
            self,
            Self::BootSchemaProbe | Self::DiagnosticsIndependentSnapshot
        )
    }
}

/// Process-local reader acquisition and hold lifecycle since one
/// [`ConnectionPool`] was constructed.
///
/// Counters are monotonic and reset only when the pool is reconstructed.
/// `active_pooled_checkouts` is the point-in-time number of live
/// [`ReaderGuard`] values. Hold duration includes connection reset or
/// replacement on return, because the slot is not reusable before then.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderAcquisitionSnapshot {
    /// Configured process-local admission budget shared by pooled readers and
    /// the explicit raw-SQL read-transaction exception.
    pub reader_admission_capacity: usize,
    /// Point-in-time permits not held by either pooled readers or explicit
    /// raw-SQL read transactions.
    pub available_reader_admission_slots: usize,
    /// Successful request-path reader acquisitions (pooled plus the explicit
    /// raw-SQL transaction exception; infrastructure opens excluded).
    pub acquisitions: u64,
    /// Successful bounded pooled-reader checkouts.
    pub pooled_checkouts: u64,
    /// Successful request-path standalone opens from the closed exception
    /// list (currently explicit raw-SQL deferred transactions only).
    pub standalone_opens: u64,
    /// Successful boot/diagnostic standalone opens, attributed separately so
    /// request traffic cannot be inferred from infrastructure activity.
    pub infrastructure_standalone_opens: u64,
    /// Reader-admission waits that exhausted `checkout_timeout` before any
    /// query began. Covers pooled checkout and the closed raw-SQL exception;
    /// cooperative request cancellation is intentionally excluded.
    pub checkout_timeouts: u64,
    /// Queries on a checked-out pooled reader that SQLite refused with
    /// `SQLITE_BUSY` after the connection's busy handler gave up (typed-store
    /// reads, pooled raw-SQL reads, and [`ReaderGuard::query_row`]). Counted
    /// after checkout succeeded, so it never overlaps `checkout_timeouts`;
    /// `SQLITE_LOCKED` and cooperative cancellation are excluded. Writer
    /// refusals are not counted here; the writer task's are in
    /// [`WriterAcquisitionSnapshot::writer_task_begin_busy`].
    pub busy_timeouts: u64,
    /// Pooled checkouts live at the instant this snapshot was taken.
    pub active_pooled_checkouts: u64,
    /// High-water mark of concurrent pooled checkouts.
    pub peak_active_pooled_checkouts: u64,
    /// Pooled guards that completed their full return/reset lifecycle.
    pub completed_pooled_checkouts: u64,
    /// Longest completed checkout hold, including return/reset, in
    /// microseconds. Diagnostic evidence only; never a test timing gate.
    pub max_completed_hold_micros: u64,
    /// The typed-store operation that held the checkout reported in
    /// `max_completed_hold_micros`. `None` when that hold came from a route
    /// that carries no operation name, which is itself the answer rather
    /// than a missing reading (#2793).
    pub max_completed_hold_operation: Option<&'static str>,
    /// A disqualified pooled-reader return (reset/pristine-check failure)
    /// whose replacement connection then also failed to open, permanently
    /// shrinking the physical pool by one slot below `max_readers`. Logged at
    /// `warn` when it happens; this counter makes the shrink observable in a
    /// snapshot too, since the pool itself never re-grows on its own.
    pub reader_replacement_open_failures: u64,
}

/// The longest completed pooled-reader hold and the operation that held it,
/// kept under one lock so a snapshot cannot pair one checkout's duration with
/// another's name.
#[derive(Debug, Default, Clone, Copy)]
struct LongestCompletedHold {
    micros: u64,
    operation: Option<&'static str>,
}

#[derive(Debug, Default)]
struct ReaderAcquisitionCounters {
    pooled_checkouts: AtomicU64,
    standalone_opens: AtomicU64,
    infrastructure_standalone_opens: AtomicU64,
    checkout_timeouts: AtomicU64,
    busy_timeouts: AtomicU64,
    active_pooled_checkouts: AtomicU64,
    peak_active_pooled_checkouts: AtomicU64,
    completed_pooled_checkouts: AtomicU64,
    longest_completed_hold: parking_lot::Mutex<LongestCompletedHold>,
    reader_replacement_open_failures: AtomicU64,
}

impl ReaderAcquisitionCounters {
    fn record_pooled_checkout(&self) {
        self.pooled_checkouts.fetch_add(1, Ordering::Relaxed);
        let active = self
            .active_pooled_checkouts
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.peak_active_pooled_checkouts
            .fetch_max(active, Ordering::Relaxed);
    }

    fn record_checkout_timeout(&self) {
        self.checkout_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    fn record_busy_timeout(&self) {
        self.busy_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    fn record_reader_replacement_open_failure(&self) {
        self.reader_replacement_open_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_standalone_open(&self, purpose: StandaloneReaderPurpose) {
        if purpose.is_infrastructure() {
            self.infrastructure_standalone_opens
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.standalone_opens.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_checkout_completed(&self, hold: Duration, operation: Option<&'static str>) {
        let previous = self.active_pooled_checkouts.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "reader active-checkout counter underflow");
        self.completed_pooled_checkouts
            .fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(hold.as_micros()).unwrap_or(u64::MAX);
        // The maximum and the name of what held it are one reading: taken
        // apart they can report a duration from one checkout beside a label
        // from another, which is worse than no label at all.
        let mut longest = self.longest_completed_hold.lock();
        if micros > longest.micros {
            longest.micros = micros;
            longest.operation = operation;
        }
    }

    fn snapshot(
        &self,
        reader_admission_capacity: usize,
        available_reader_admission_slots: usize,
    ) -> ReaderAcquisitionSnapshot {
        let pooled_checkouts = self.pooled_checkouts.load(Ordering::Relaxed);
        let standalone_opens = self.standalone_opens.load(Ordering::Relaxed);
        let longest_completed_hold = *self.longest_completed_hold.lock();
        ReaderAcquisitionSnapshot {
            reader_admission_capacity,
            available_reader_admission_slots,
            acquisitions: pooled_checkouts.saturating_add(standalone_opens),
            pooled_checkouts,
            standalone_opens,
            infrastructure_standalone_opens: self
                .infrastructure_standalone_opens
                .load(Ordering::Relaxed),
            checkout_timeouts: self.checkout_timeouts.load(Ordering::Relaxed),
            busy_timeouts: self.busy_timeouts.load(Ordering::Relaxed),
            active_pooled_checkouts: self.active_pooled_checkouts.load(Ordering::Relaxed),
            peak_active_pooled_checkouts: self.peak_active_pooled_checkouts.load(Ordering::Relaxed),
            completed_pooled_checkouts: self.completed_pooled_checkouts.load(Ordering::Relaxed),
            max_completed_hold_micros: longest_completed_hold.micros,
            max_completed_hold_operation: longest_completed_hold.operation,
            reader_replacement_open_failures: self
                .reader_replacement_open_failures
                .load(Ordering::Relaxed),
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

/// A zero-wait checkout that can run only the fixed checkpoint recovery
/// pragmas. The connection remains private: exposing it would let a caller
/// execute logical writes without disk-reserve admission (ADR-154 §5).
///
/// Ordinary SQL is deliberately unavailable through this capability:
/// ```compile_fail
/// use khive_db::{ConnectionPool, PoolConfig};
/// let pool = ConnectionPool::new(PoolConfig::default()).unwrap();
/// pool.try_checkpoint_nowait().unwrap().execute_batch("CREATE TABLE bypass (id INTEGER)");
/// ```
pub struct CheckpointGuard<'pool> {
    guard: parking_lot::MutexGuard<'pool, Connection>,
}

/// SQLite's three-column result from a fixed WAL checkpoint pragma.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointResult {
    /// Whether SQLite reported a busy checkpoint.
    pub busy: i64,
    /// WAL frames observed by SQLite (`-1` when there is no WAL).
    pub log_frames: i64,
    /// WAL frames copied back into the database.
    pub checkpointed_frames: i64,
}

impl CheckpointGuard<'_> {
    /// Run a PASSIVE checkpoint without disk-reserve admission.
    pub fn passive(&self) -> Result<CheckpointResult, SqliteError> {
        self.guard
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok(CheckpointResult {
                    busy: row.get(0)?,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
            .map_err(Into::into)
    }

    /// Run a TRUNCATE checkpoint without disk-reserve admission.
    pub fn truncate(&self) -> Result<CheckpointResult, SqliteError> {
        self.guard
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok(CheckpointResult {
                    busy: row.get(0)?,
                    log_frames: row.get(1)?,
                    checkpointed_frames: row.get(2)?,
                })
            })
            .map_err(Into::into)
    }
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
    /// Instrumented direct executions whose final returned error retains SQLite's
    /// primary DatabaseBusy code, once per operation after its busy handler.
    /// Excludes LOCKED, checkout/open/admission failures, readers, writer tasks,
    /// infrastructure probes and uninstrumented raw connection escapes.
    pub direct_busy_refusals: u64,
    /// Every writer-task `BEGIN IMMEDIATE` attempt refused busy or locked,
    /// including refusals a subsequent bounded retry went on to absorb.
    /// Counted separately from `timeouts` because that counter names the
    /// pool-mutex checkout stage; folding the two would mislabel the stage.
    pub writer_task_begin_busy: u64,
    /// Subset of `writer_task_begin_busy` that a subsequent bounded retry
    /// absorbed before the request closure ran, so the refusal never
    /// reached the caller. `writer_task_begin_busy - writer_task_begin_busy_absorbed`
    /// is the count of refusals a caller actually observed.
    pub writer_task_begin_busy_absorbed: u64,
    /// Writer-task `BEGIN IMMEDIATE` attempts that failed for a reason other
    /// than busy or locked, and so surface as `StorageError::Pool`.
    pub writer_task_begin_errors: u64,
    /// Dequeued writer-task requests that reached the writer seam (executed
    /// or attempted to execute their operation) and terminated in error,
    /// counted once per request regardless of the specific terminal state.
    pub writer_task_request_failures: u64,
    /// Subset of `writer_task_request_failures` whose terminal state was
    /// `WriterTaskRequestState::SideEffectsUnknown` — the commit or rollback
    /// outcome could not be established, so the request's side effects on
    /// the database are unknown.
    pub writer_task_side_effects_unknown: u64,
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
    direct_busy_refusals: AtomicU64,
    writer_task_begin_busy: AtomicU64,
    writer_task_begin_busy_absorbed: AtomicU64,
    writer_task_begin_errors: AtomicU64,
    writer_task_request_failures: AtomicU64,
    writer_task_side_effects_unknown: AtomicU64,
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
        config.wal_ceiling.validate_static(
            config.path.is_some(),
            config.wal_mode,
            config.read_only,
        )?;

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
        let write_admission = if !config.read_only {
            if let Some(volume) = identity_path.as_deref().and_then(Path::parent) {
                Arc::new(WriteAdmission::new(
                    Some(volume.to_path_buf()),
                    db_free_space_floor_from_env()?,
                ))
            } else {
                Arc::new(WriteAdmission::new(None, 0))
            }
        } else {
            Arc::new(WriteAdmission::new(None, 0))
        };
        let read_only_open_target = read_only_open_target(&config, identity_path.as_deref())?;
        #[cfg(any(unix, windows))]
        let identity_before_open = identity_path
            .as_deref()
            .map(database_file_identity_if_exists)
            .transpose()?
            .flatten();
        let mut writer = open_writer_connection(
            &config,
            read_only_open_target.as_deref(),
            identity_path.as_deref(),
        )?;
        validate_wal_ceiling_at_open(&writer, &config)?;
        // The identity bootstrap can take a write lock before the remaining
        // connection pragmas are configured. Honor the caller's wait bound.
        writer.busy_timeout(config.busy_timeout)?;
        // A read of main.sqlite_master forces SQLite's main file open without
        // changing either database. Reject an already-swapped target before
        // installing a nonce into a legacy or initially empty database.
        let initial_database_id = if identity_path.is_some() {
            read_database_id(&writer)?
        } else {
            None
        };
        #[cfg(test)]
        if let Some(path) = identity_path.as_deref() {
            run_identity_open_hook(
                path,
                IdentityOpenStage::AfterMainOpenBeforeFirstStat,
                Some(&writer),
            );
        }
        #[cfg(any(unix, windows))]
        let identity_before_write = identity_path
            .as_deref()
            .map(database_file_identity)
            .transpose()?;
        #[cfg(any(unix, windows))]
        if identity_before_open.is_some() && identity_before_open != identity_before_write {
            return Err(SqliteError::InvalidData(
                "database file identity changed while opening the pool".to_string(),
            ));
        }
        #[cfg(any(unix, windows))]
        if let Some(path) = identity_path.as_deref() {
            let opened = opened_sqlite_file_identity(&writer, path)?;
            if identity_before_write != Some(opened) {
                return Err(SqliteError::InvalidData(
                    "database file identity changed while opening the pool".to_string(),
                ));
            }
        }
        let opened_database_id =
            if identity_path.is_some() && !config.read_only && initial_database_id.is_none() {
                match write_admission.check() {
                    Ok(()) => Some(initialize_database_id(&mut writer)?),
                    // Recovery must be able to open this pool and its checkpoint
                    // connection below the reserve. Physical file pinning still
                    // applies; a later pool open can install the nonce.
                    Err(SqliteError::CapacityFloor { .. }) => None,
                    Err(error) => return Err(error),
                }
            } else {
                initial_database_id
            };
        #[cfg(test)]
        if let Some(path) = identity_path.as_deref() {
            run_identity_open_hook(
                path,
                IdentityOpenStage::AfterInitialIdentityWrite,
                Some(&writer),
            );
        }
        #[cfg(any(unix, windows))]
        let opened_file_identity = identity_path
            .as_deref()
            .map(|path| opened_sqlite_file_identity(&writer, path))
            .transpose()?;
        #[cfg(any(unix, windows))]
        if identity_before_write != opened_file_identity {
            return Err(SqliteError::InvalidData(
                "database file identity changed while opening the pool".to_string(),
            ));
        }
        let wal_enabled = configure_writer_connection(&writer, &config)?;
        let max_readers = effective_reader_count(&config, wal_enabled);

        let readers = ArrayQueue::new(max_readers.max(1));

        #[cfg(any(test, feature = "test-support"))]
        let statement_observer = crate::statement_observer::StatementObserverHub::new()?;
        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(&writer, &statement_observer)?;

        let mut pool = Self {
            writer: Arc::new(Mutex::new(writer)),
            #[cfg(any(test, feature = "test-support"))]
            statement_observer,
            main_pool_generation: OnceLock::new(),
            checkpoint_ownership: CheckpointOwnershipGate::new(),
            pooled_writer_retired: AtomicBool::new(false),
            writer_acquisition_counters: Arc::new(WriterAcquisitionCounters::default()),
            write_admission,
            reader_acquisition_counters: ReaderAcquisitionCounters::default(),
            search_dispatches: Mutex::new(BTreeMap::new()),
            note_candidate_hydration_rows: AtomicU64::new(0),
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
            #[cfg(any(unix, windows))]
            opened_file_identity,
            opened_database_id,
            identity_registration: None,
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

        pool.identity_registration = pool.canonical_path().map(PoolIdentityRegistration::new);
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
        let started = Instant::now();
        let mut admission_attempt = 0u32;
        let admission_slot = loop {
            if should_stop() {
                return Ok(None);
            }
            match Arc::clone(&self.sql_bridge_reader_slots).try_acquire_owned() {
                Ok(slot) => break slot,
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(SqliteError::InvalidData(
                        "reader admission semaphore is closed".to_string(),
                    ));
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {}
            }
            if started.elapsed() >= self.config.checkout_timeout {
                self.reader_acquisition_counters.record_checkout_timeout();
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }
            match admission_attempt {
                0..=7 => {
                    let spins = 1usize << admission_attempt;
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
                    let sleep =
                        Duration::from_micros(50 * (1u64 << (admission_attempt - 16).min(6)));
                    thread::sleep(sleep.min(remaining).min(Duration::from_millis(2)));
                }
            }
            admission_attempt = admission_attempt.saturating_add(1);
        };

        self.reader_with_admission(
            ReaderAdmission {
                slot: admission_slot,
                started,
            },
            should_stop,
        )
    }

    /// Wait for one pool-wide reader admission permit on the async side, so a
    /// read that has to queue costs a task and not a blocking-pool thread.
    ///
    /// The wait is bounded by `checkout_timeout` and by the current request's
    /// cancellation and deadline, and it yields the same `Ok(Some)` / `Ok(None)`
    /// / `Err` outcomes the permit loop in [`Self::reader_until`] does, resolved
    /// through the same refusal mapping as [`Self::resolve_reader_checkout`].
    /// Dropping the future abandons the wait without taking a permit.
    pub(crate) async fn acquire_reader_admission(
        &self,
        capability: StorageCapability,
        operation: &'static str,
    ) -> Result<ReaderAdmission, StorageError> {
        let context = khive_storage::capture_request_read_context();
        let started = Instant::now();
        let stopped = context.stop_reason().is_some();
        let outcome: Result<Option<ReaderAdmission>, SqliteError> = if stopped {
            Ok(None)
        } else {
            tokio::select! {
                biased;
                _ = context.wait_for_stop() => Ok(None),
                waited = tokio::time::timeout(
                    self.config.checkout_timeout,
                    Arc::clone(&self.sql_bridge_reader_slots).acquire_owned(),
                ) => match waited {
                    Ok(Ok(slot)) => Ok(Some(ReaderAdmission { slot, started })),
                    Ok(Err(_closed)) => Err(SqliteError::InvalidData(
                        "reader admission semaphore is closed".to_string(),
                    )),
                    Err(_elapsed) => {
                        self.reader_acquisition_counters.record_checkout_timeout();
                        Err(pool_exhausted_error(
                            self.config.checkout_timeout,
                            self.max_readers,
                        ))
                    }
                },
            }
        };
        match outcome {
            Ok(Some(admission)) => Ok(admission),
            Ok(None) => Err(self.reader_checkout_refusal(capability, operation, None)),
            Err(error) => Err(self.reader_checkout_refusal(capability, operation, Some(error))),
        }
    }

    /// Select the reader connection for an admission permit already held.
    ///
    /// This is [`Self::reader_until`] after its permit loop: the same
    /// `should_stop` polling, the same `Ok(Some)` / `Ok(None)` / `Err`
    /// outcomes, and the remainder of the one `checkout_timeout` that began
    /// when the permit wait did.
    pub(crate) fn reader_with_admission<C>(
        &self,
        admission: ReaderAdmission,
        should_stop: C,
    ) -> Result<Option<ReaderGuard<'_>>, SqliteError>
    where
        C: Fn() -> bool,
    {
        let ReaderAdmission {
            slot: admission_slot,
            started,
        } = admission;

        if self.max_readers == 0 {
            self.ensure_pooled_writer_active()?;
            loop {
                if should_stop() {
                    return Ok(None);
                }
                let remaining = self
                    .config
                    .checkout_timeout
                    .saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    self.reader_acquisition_counters.record_checkout_timeout();
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
                    self.reader_acquisition_counters.record_pooled_checkout();
                    return Ok(Some(ReaderGuard {
                        lease: Some(ReaderLease::Shared(guard)),
                        admission_slot: Some(admission_slot),
                        pool: self,
                        reusable: Cell::new(true),
                        query_in_progress: Cell::new(false),
                        checked_out_at: Instant::now(),
                        dirty: Cell::new(false),
                        operation: None,
                    }));
                }
            }
        }

        let mut attempt = 0u32;

        loop {
            if should_stop() {
                return Ok(None);
            }
            if let Some(conn) = self.readers.pop() {
                self.reader_acquisition_counters.record_pooled_checkout();
                return Ok(Some(ReaderGuard {
                    lease: Some(ReaderLease::Pooled(conn)),
                    admission_slot: Some(admission_slot),
                    pool: self,
                    reusable: Cell::new(true),
                    query_in_progress: Cell::new(false),
                    checked_out_at: Instant::now(),
                    dirty: Cell::new(false),
                    operation: None,
                }));
            }

            if started.elapsed() >= self.config.checkout_timeout {
                self.reader_acquisition_counters.record_checkout_timeout();
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
        self.write_admission.check()?;
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

    pub(crate) fn writer_until<C>(
        &self,
        should_stop: C,
    ) -> Result<Option<WriterGuard<'_>>, SqliteError>
    where
        C: Fn() -> bool,
    {
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
            if let Some(guard) = self
                .writer
                .try_lock_for(remaining.min(Duration::from_millis(2)))
            {
                // Cancellation may have arrived during the final wait slice.
                // Once this guard is returned, constructor DDL is not interrupted.
                if should_stop() {
                    return Ok(None);
                }
                self.ensure_pooled_writer_active()?;
                self.write_admission.check()?;
                self.writer_acquisition_counters
                    .pooled_acquisitions
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(Some(WriterGuard {
                    guard,
                    origin: self.origin(),
                }));
            }
            if started.elapsed() >= self.config.checkout_timeout {
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
            }
        }
    }

    /// Zero-wait checkpoint checkout for recovery maintenance.
    ///
    /// Uses `try_lock()` (no timeout, no spin) — returns `Err` immediately when
    /// any other caller holds the writer Mutex. The scheduled checkpoint task
    /// uses its own dedicated connection (ADR-091 Amendment 5); this optional
    /// pooled capability remains available for zero-wait recovery callers.
    ///
    /// It bypasses the disk-reserve floor because checkpoints can recover WAL
    /// space at or below that floor (ADR-154 §5). The returned guard exposes
    /// only fixed PASSIVE and TRUNCATE checkpoint operations.
    ///
    /// The former unrestricted checkout must not return:
    /// ```compile_fail
    /// use khive_db::{ConnectionPool, PoolConfig};
    /// let pool = ConnectionPool::new(PoolConfig::default()).unwrap();
    /// pool.try_writer_nowait().unwrap().execute_batch("CREATE TABLE bypass (id INTEGER)");
    /// ```
    pub fn try_checkpoint_nowait(&self) -> Result<CheckpointGuard<'_>, SqliteError> {
        self.ensure_pooled_writer_active()?;
        let guard = self.writer.try_lock().ok_or_else(|| {
            SqliteError::InvalidData(
                "writer connection busy (checkpoint skipped this tick)".to_string(),
            )
        })?;
        self.ensure_pooled_writer_active()?;
        Ok(CheckpointGuard { guard })
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

    /// Count a coordinator call only after served-kind filtering selects this
    /// backend. The canonical requested kind is the granular kind when one was
    /// supplied, or the entity/note substrate otherwise.
    pub fn record_search_dispatch(&self, backend_id: &str, requested_kind: &str) {
        let mut dispatches = self.search_dispatches.lock();
        let count = dispatches
            .entry(backend_id.to_owned())
            .or_default()
            .entry(requested_kind.to_owned())
            .or_default();
        *count = count.saturating_add(1);
    }

    /// Count one actual candidate note row returned at the post-fusion
    /// hydration seam, including rows later filtered as deleted. Absent rows
    /// and later result metadata are excluded.
    pub fn record_note_candidate_hydration_row(&self) {
        let _ = self.note_candidate_hydration_rows.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| Some(current.saturating_add(1)),
        );
    }

    /// Snapshot ADR-166 G4/G5 counters. Values reset only with this pool.
    pub fn search_mechanism_snapshot(&self) -> SearchMechanismSnapshot {
        SearchMechanismSnapshot {
            dispatches_by_backend_and_kind: self.search_dispatches.lock().clone(),
            note_candidate_hydration_rows: self
                .note_candidate_hydration_rows
                .load(Ordering::Relaxed),
        }
    }

    /// Snapshot reader acquisition, saturation, and hold lifecycle outcomes
    /// since this pool was constructed. Counters reset only with pool
    /// reconstruction.
    pub fn reader_acquisition_snapshot(&self) -> ReaderAcquisitionSnapshot {
        self.reader_acquisition_counters.snapshot(
            self.max_readers.max(1),
            self.sql_bridge_reader_slots.available_permits(),
        )
    }

    /// Record one pool-wide reader-admission wait that exhausted the configured
    /// checkout timeout outside [`Self::reader_until`] (currently the explicit
    /// raw-SQL read-transaction exception and reads on a standalone writer).
    pub(crate) fn record_reader_admission_timeout(&self) {
        self.reader_acquisition_counters.record_checkout_timeout();
    }

    /// Count a query error from an already checked-out pooled reader when it is
    /// SQLite's `SQLITE_BUSY` surfacing after the busy handler gave up. Every
    /// other error, including `SQLITE_LOCKED`, is ignored. Checkout exhaustion
    /// is counted by [`Self::reader_until`] before any query runs, so the two
    /// classes cannot overlap.
    pub(crate) fn record_reader_query_error(&self, error: &StorageError) {
        if crate::read_cancellation::storage_error_sqlite_code(error)
            == Some(rusqlite::ErrorCode::DatabaseBusy)
        {
            self.reader_acquisition_counters.record_busy_timeout();
        }
    }

    /// Clone the pool-scoped counter set for the lifetime-owned writer task.
    pub(crate) fn writer_acquisition_counters(&self) -> Arc<WriterAcquisitionCounters> {
        Arc::clone(&self.writer_acquisition_counters)
    }

    pub(crate) fn write_admission(&self) -> Arc<WriteAdmission> {
        Arc::clone(&self.write_admission)
    }

    #[cfg(test)]
    pub(crate) fn set_test_write_admission(
        &mut self,
        floor_bytes: u64,
        probe: impl Fn(&Path) -> std::io::Result<u64> + Send + Sync + 'static,
    ) {
        let volume = if self.config.read_only {
            None
        } else {
            self.canonical_path()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        };
        let admission = Arc::new(WriteAdmission::new(volume, floor_bytes));
        admission.set_test_space_probe(probe);
        self.write_admission = admission;
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

    /// Observe actual SQLite statement starts on this private test pool.
    ///
    /// The limit bounds retained SQL records. Failed steps count as attempts;
    /// preparation alone does not count. The guard observes every pool-owned
    /// connection, including the queued writer. Do not run unrelated background
    /// work on the fixture pool; see the guard documentation for limitations.
    /// Connection setup runs before observation begins on each connection and
    /// is never recorded, including for opens during an active observation.
    /// Reader connection opens use the existing reader acquisition counters instead.
    #[cfg(any(test, feature = "test-support"))]
    pub fn observe_test_statement_starts(
        &self,
        limit: usize,
    ) -> Result<crate::statement_observer::StatementStartObservation, SqliteError> {
        self.statement_observer.observe(limit)
    }

    /// Identify this pool's counter window when it is designated as main.
    /// Repeated runtime handles and diagnostics reads reuse the same generation;
    /// constructing secondary pools does not consume main-pool generations.
    pub fn main_pool_generation(&self) -> u64 {
        *self.main_pool_generation.get_or_init(|| {
            NEXT_MAIN_POOL_GENERATION
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .expect("main pool generation exhausted")
        })
    }

    /// The typed admission failure for a pooled reader checkout that
    /// exhausted `checkout_timeout`: no reader was acquired, so the
    /// operation never started and a retry cannot duplicate a side effect.
    pub(crate) fn reader_admission_timeout(&self, operation: &'static str) -> StorageError {
        StorageError::AdmissionTimeout {
            operation: operation.into(),
            timeout_ms: u64::try_from(self.config.checkout_timeout.as_millis()).unwrap_or(u64::MAX),
            pool_identity: Some(
                self.identity_registration
                    .as_ref()
                    .map(PoolIdentityRegistration::label)
                    .unwrap_or_else(|| ":memory:".to_string()),
            ),
        }
    }

    /// Resolve a [`Self::reader_until`] outcome into a checked-out guard or
    /// the canonical refusal. This is the single home of the checkout
    /// tri-state; call sites must not re-derive any arm of it:
    ///
    /// - `Ok(Some)` — a reader was checked out.
    /// - `Ok(None)` — `should_stop()` fired: the request was cancelled or hit
    ///   its deadline before checkout. NOT an admission wait, so it maps to
    ///   the non-retryable [`StorageError::Timeout`] — emitting the retryable
    ///   `AdmissionTimeout` here would invite an immediate retry of a request
    ///   its caller already abandoned, into a possibly saturated pool.
    /// - `Err` carrying the pool's own `SQLITE_BUSY` — `reader_until` executes
    ///   no SQL, so the only `SQLITE_BUSY` it can produce is
    ///   [`pool_exhausted_error`], raised when `checkout_timeout` elapses with
    ///   no reader available. A genuine admission wait that ended before any
    ///   work began maps to the retryable [`StorageError::AdmissionTimeout`].
    /// - any other `Err` (e.g. [`Self::ensure_pooled_writer_active`] returning
    ///   `InvalidData` for a retired pooled writer) — an opaque driver failure
    ///   under the caller's capability, non-retryable.
    pub(crate) fn resolve_reader_checkout<'p>(
        &self,
        capability: StorageCapability,
        operation: &'static str,
        outcome: Result<Option<ReaderGuard<'p>>, SqliteError>,
    ) -> Result<ReaderGuard<'p>, StorageError> {
        match outcome {
            Ok(Some(mut guard)) => {
                guard.label_operation(operation);
                Ok(guard)
            }
            Ok(None) => Err(self.reader_checkout_refusal(capability, operation, None)),
            Err(error) => Err(self.reader_checkout_refusal(capability, operation, Some(error))),
        }
    }

    /// The refusal for a reader checkout that produced nothing, shared by
    /// [`Self::resolve_reader_checkout`] and [`Self::acquire_reader_admission`]
    /// so the arms documented on the former have one home. `None` is the
    /// stopped-request arm; `Some` carries the failed checkout's error.
    fn reader_checkout_refusal(
        &self,
        capability: StorageCapability,
        operation: &'static str,
        error: Option<SqliteError>,
    ) -> StorageError {
        let Some(error) = error else {
            return StorageError::Timeout {
                operation: operation.into(),
            };
        };
        let is_pool_exhausted = matches!(
            &error,
            SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        );
        if is_pool_exhausted {
            self.reader_admission_timeout(operation)
        } else {
            StorageError::driver(capability, operation, error)
        }
    }

    /// Pool-wide admission permits shared by pooled readers, the explicit
    /// raw-SQL read-transaction exception, and reads on standalone writers.
    pub(crate) fn sql_bridge_reader_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sql_bridge_reader_slots)
    }

    /// Pool-wide permit for a file-backed raw-SQL writer handle.
    pub(crate) fn sql_bridge_writer_slots(&self) -> Arc<Semaphore> {
        Arc::clone(&self.sql_bridge_writer_slots)
    }

    /// Current writer holds that prevent voluntary daemon retirement.
    ///
    /// The raw-SQL writer permit remains handle-scoped even in autocommit.
    /// This read acquires no connection and changes no admission policy.
    pub fn retirement_writer_holds(&self) -> usize {
        usize::from(self.writer.is_locked())
            + usize::from(self.sql_bridge_writer_slots.available_permits() == 0)
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

    /// Unix file identity retained for callers using the original tuple API.
    #[cfg(unix)]
    pub fn opened_file_identity(&self) -> Option<(u64, u64)> {
        self.opened_file_identity
            .map(DatabaseFileIdentity::unix_parts)
    }

    /// Physical file identity pinned to SQLite's opened main file. Construction
    /// compares it with the path on both sides of the open before admission.
    #[cfg(any(unix, windows))]
    pub fn opened_file_identity_record(&self) -> Option<DatabaseFileIdentity> {
        self.opened_file_identity
    }

    /// Ownership evidence captured through the database SQLite opened.
    /// This does not select the topology's main backend or establish a root binding.
    /// Legacy read-only and low-space opens without a stored UUID must reopen
    /// after installation; this accessor never installs or re-mints an identity.
    pub fn database_owner_identity(
        &self,
    ) -> Result<DatabaseOwnerIdentity, DatabaseOwnerIdentityError> {
        if self.identity_path.is_none() {
            return Err(DatabaseOwnerIdentityError::InMemory);
        }
        #[cfg(any(unix, windows))]
        {
            let durable_id = self
                .opened_database_id
                .ok_or(DatabaseOwnerIdentityError::DurableIdentityUnavailable)?;
            let file_identity = self
                .opened_file_identity
                .ok_or(DatabaseOwnerIdentityError::PhysicalIdentityUnavailable)?;
            Ok(DatabaseOwnerIdentity {
                durable_id,
                file_identity,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(DatabaseOwnerIdentityError::UnsupportedPlatform)
        }
    }

    /// Require both this opened database's UUID and physical file to match.
    pub fn verify_database_owner(
        &self,
        expected: &DatabaseOwnerIdentity,
    ) -> Result<(), DatabaseOwnerIdentityError> {
        self.database_owner_identity()?.verify_owner(expected)
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

    /// Resolve a runtime-owned transaction through the same strict/compatibility
    /// policy as store writes. `None` permits the caller's direct transaction and
    /// records its compatibility fallback when the file-backed queue is enabled.
    /// A strict refusal returns before any direct-writer acquisition or telemetry.
    pub fn writer_task_for_runtime_write(
        &self,
        operation: RuntimeWriteOperation,
    ) -> Result<Option<WriterTaskHandle>, StorageError> {
        let handle = self.writer_task_for_write(None, operation.operation())?;
        if handle.is_none() {
            self.record_direct_route(operation.fallback_site());
        }
        Ok(handle)
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
        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_opened_file_identity(identity_path)?;
        }
        let conn = open_reader_connection(path, &self.config)?;
        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_connection_file_identity(&conn, identity_path)?;
        }
        self.verify_opened_database_id(&conn)?;
        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(&conn, &self.statement_observer)?;
        Ok(conn)
    }

    fn read_connection_path(&self) -> Result<&Path, SqliteError> {
        self.read_only_open_target
            .as_deref()
            .or(self.identity_path.as_deref())
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
        self.write_admission.check()?;
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
        let path = self.identity_path.as_deref().ok_or_else(|| {
            SqliteError::InvalidData(
                "in-memory databases do not support standalone connections".to_string(),
            )
        })?;

        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "database is read-only: standalone write connections are not permitted".to_string(),
            ));
        }

        // The configured spelling may be a symlink that changed since this
        // pool opened. Use its pinned target, and refuse replacement of that
        // target before SQLite can execute the diagnostics PASSIVE checkpoint.
        #[cfg(any(unix, windows))]
        self.verify_opened_file_identity(path)?;

        #[cfg(test)]
        run_identity_open_hook(path, IdentityOpenStage::BeforeStandaloneOpen, None);

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        #[cfg(test)]
        run_identity_open_hook(path, IdentityOpenStage::AfterStandaloneOpen, Some(&conn));
        #[cfg(any(unix, windows))]
        self.verify_connection_file_identity(&conn, path)?;
        self.verify_opened_database_id(&conn)?;
        #[cfg(feature = "namespace-trigram-proto")]
        register_namespace_trigram(&conn)?;
        register_writer_clock(&conn)?;
        // Expression indexes over these keys are maintained by every writer
        // (the write-queue task and per-store standalone writers included), so
        // each of them needs the same functions the pooled writer registers.
        register_rfc3339_key(&conn)?;
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

        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(&conn, &self.statement_observer)?;
        Ok(conn)
    }

    #[cfg(any(unix, windows))]
    fn verify_opened_file_identity(&self, path: &Path) -> Result<(), SqliteError> {
        let Some(expected) = self.opened_file_identity else {
            return Err(SqliteError::InvalidData(
                "file-backed pool has no opened database file identity".to_string(),
            ));
        };
        let current = database_file_identity(path).ok();
        if current != Some(expected) {
            return Err(SqliteError::InvalidData(
                "pool database file identity changed since the first open; refusing standalone connection"
                    .to_string(),
            ));
        }
        Ok(())
    }

    #[cfg(any(unix, windows))]
    fn verify_connection_file_identity(
        &self,
        conn: &Connection,
        path: &Path,
    ) -> Result<(), SqliteError> {
        let opened = opened_sqlite_file_identity(conn, path)?;
        if self.opened_file_identity != Some(opened) {
            return Err(SqliteError::InvalidData(
                "pool database file identity changed since the first open; refusing standalone connection"
                    .to_string(),
            ));
        }
        Ok(())
    }

    fn verify_opened_database_id(&self, conn: &Connection) -> Result<(), SqliteError> {
        // A read-only pool, or a writable pool opened below the space reserve,
        // may precede installation of the nonce by another process. Without a
        // nonce to pin, use the existing file identity checks where available.
        let Some(expected) = self.opened_database_id else {
            return Ok(());
        };
        if self.identity_path.is_some() && read_database_id(conn)? != Some(expected) {
            return Err(SqliteError::InvalidData(
                "pool database identity changed since the first open; refusing standalone connection"
                    .to_string(),
            ));
        }
        Ok(())
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

    /// Open a standalone read-only connection for one enumerated structural
    /// exception to pooled-reader routing.
    ///
    /// There is intentionally no public/generic standalone-reader fallback.
    /// Ordinary request reads use [`Self::reader`] and surface bounded pool
    /// exhaustion. Adding a purpose variant or call site is therefore a
    /// deliberate, scoped architecture change (ADR-165 Slice 2), not a
    /// routine extension.
    pub(crate) fn open_standalone_reader(
        &self,
        purpose: StandaloneReaderPurpose,
    ) -> Result<Connection, SqliteError> {
        let path = self.read_connection_path()?;

        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_opened_file_identity(identity_path)?;
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        #[cfg(any(unix, windows))]
        if let Some(identity_path) = self.identity_path.as_deref() {
            self.verify_connection_file_identity(&conn, identity_path)?;
        }
        self.verify_opened_database_id(&conn)?;
        configure_reader_connection(&conn, &self.config)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        self.reader_acquisition_counters
            .record_standalone_open(purpose);
        #[cfg(any(test, feature = "test-support"))]
        crate::statement_observer::install(&conn, &self.statement_observer)?;
        Ok(conn)
    }

    fn return_reader(&self, conn: Connection, dirty: bool) {
        if self.max_readers == 0 {
            return;
        }

        if reset_reader_connection(&conn, dirty, &self.config)
            && reader_connection_is_healthy(&conn)
        {
            self.enqueue_reader_slot(conn);
            return;
        }

        close_connection_quietly(conn);
        self.replace_discarded_reader_slot();
    }

    /// Push a connection back onto the physical reader queue, discarding it
    /// (rather than growing the queue past its configured capacity) if the
    /// queue is already full.
    fn enqueue_reader_slot(&self, conn: Connection) {
        if let Err(conn) = self.readers.push(conn) {
            eprintln!("[sqlite-pool] reader pool queue full, discarding replacement connection");
            close_connection_quietly(conn);
        }
    }

    /// Open a fresh connection to refill one physical reader slot after its
    /// previous occupant was closed and discarded — either disqualified on
    /// an ordinary return ([`Self::return_reader`]) or abandoned by a
    /// non-reusable checkout ([`ReaderGuard`]'s `Drop`). Both call sites
    /// share this so a failed replacement is recorded and logged identically
    /// either way, instead of one path silently shrinking the pool.
    fn replace_discarded_reader_slot(&self) {
        match self.open_reader_connection() {
            Ok(conn) => self.enqueue_reader_slot(conn),
            Err(error) => {
                self.reader_acquisition_counters
                    .record_reader_replacement_open_failure();
                tracing::warn!(
                    %error,
                    "sqlite-pool: reader replacement connection failed to open; the physical \
                     pool permanently shrinks by one slot below max_readers"
                );
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

#[cfg(any(unix, windows))]
fn database_file_identity_if_exists(
    path: &Path,
) -> Result<Option<DatabaseFileIdentity>, SqliteError> {
    match database_file_identity(path) {
        Ok(identity) => Ok(Some(identity)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(any(unix, windows))]
fn opened_sqlite_file_identity(
    conn: &Connection,
    path: &Path,
) -> Result<DatabaseFileIdentity, SqliteError> {
    #[cfg(unix)]
    {
        verify_sqlite_opened_file_still_at_path(conn)?;
        Ok(database_file_identity(path)?)
    }
    #[cfg(windows)]
    {
        let opened = sqlite_opened_file_identity(conn)?;
        if database_file_identity(path).ok() != Some(opened) {
            return Err(SqliteError::InvalidData(
                "database file identity changed while SQLite held the opened file".to_string(),
            ));
        }
        Ok(opened)
    }
}

/// The nonce lives in the main database and is read through the connection
/// SQLite actually opened. A pathname stat alone can observe a different file
/// when another process renames entries during `sqlite3_open_v2`.
fn read_database_id(conn: &Connection) -> Result<Option<uuid::Uuid>, SqliteError> {
    let table_exists: bool = conn.query_row(
        "SELECT count(*) != 0 FROM main.sqlite_master WHERE type = 'table' AND name = ?1",
        [DATABASE_ID_TABLE],
        |row| row.get(0),
    )?;
    if !table_exists {
        // Older read-only snapshots cannot be initialized here. Their Unix
        // file-control and inode checks still apply; writable opens backfill.
        return Ok(None);
    }
    let id: String = conn.query_row(
        &format!("SELECT id FROM main.{DATABASE_ID_TABLE} WHERE singleton = 1"),
        [],
        |row| row.get(0),
    )?;
    let id = uuid::Uuid::parse_str(&id).map_err(|error| {
        SqliteError::InvalidData(format!("invalid stored database identity: {error}"))
    })?;
    Ok(Some(id))
}

fn initialize_database_id(conn: &mut Connection) -> Result<uuid::Uuid, SqliteError> {
    if let Some(id) = read_database_id(conn)? {
        return Ok(id);
    }
    let transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS main.{DATABASE_ID_TABLE} (\
             singleton INTEGER PRIMARY KEY CHECK (singleton = 1), \
             id TEXT NOT NULL\
         )"
    ))?;
    transaction.execute(
        &format!("INSERT OR IGNORE INTO main.{DATABASE_ID_TABLE} (singleton, id) VALUES (1, ?1)"),
        [uuid::Uuid::new_v4().to_string()],
    )?;
    let id: String = transaction.query_row(
        &format!("SELECT id FROM main.{DATABASE_ID_TABLE} WHERE singleton = 1"),
        [],
        |row| row.get(0),
    )?;
    let id = uuid::Uuid::parse_str(&id).map_err(|error| {
        SqliteError::InvalidData(format!("invalid stored database identity: {error}"))
    })?;
    transaction.commit()?;
    Ok(id)
}

#[cfg(unix)]
fn verify_sqlite_opened_file_still_at_path(conn: &Connection) -> Result<(), SqliteError> {
    let mut moved: std::ffi::c_int = 0;
    // SAFETY: `conn` remains alive and exclusively borrowed for this call;
    // the `main` C string and writable integer out-parameter remain valid.
    // SQLite documents SQLITE_FCNTL_HAS_MOVED as querying the opened file,
    // and the bundled Unix VFS implements it using its retained inode.
    // https://www.sqlite.org/c3ref/c_fcntl_begin_atomic_write.html
    let result = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut std::ffi::c_int).cast(),
        )
    };
    if result != rusqlite::ffi::SQLITE_OK {
        return Err(SqliteError::InvalidData(format!(
            "cannot verify opened database file identity (SQLite file control {result})"
        )));
    }
    if moved != 0 {
        return Err(SqliteError::InvalidData(
            "database file identity changed while SQLite held the opened file".to_string(),
        ));
    }
    Ok(())
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
    identity_path: Option<&Path>,
) -> Result<Connection, SqliteError> {
    match config.path.as_ref() {
        Some(_) => {
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
                identity_path.ok_or_else(|| {
                    SqliteError::InvalidData(
                        "file-backed writable pool has no canonical open target".to_string(),
                    )
                })?
            };
            Connection::open_with_flags(target, flags).map_err(Into::into)
        }
        None => Connection::open_in_memory().map_err(Into::into),
    }
}

/// Validate the one-frame reset floor using this backend connection's own
/// page size. This runs before writer configuration changes journal mode or
/// performs any schema work. The WAL I/O limiter arrives in a later slice, so
/// a valid nonzero policy still refuses to open rather than running uncovered.
fn validate_wal_ceiling_at_open(conn: &Connection, config: &PoolConfig) -> Result<(), SqliteError> {
    let bytes = config.wal_ceiling.effective_bytes(config.read_only);
    if bytes == 0 {
        return Ok(());
    }
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    let page_size = u64::try_from(page_size).map_err(|_| {
        SqliteError::InvalidData("SQLite reported a negative page size".to_string())
    })?;
    let minimum_bytes = page_size.checked_add(56).ok_or_else(|| {
        SqliteError::InvalidData("SQLite page size overflowed the WAL frame floor".to_string())
    })?;
    if bytes < minimum_bytes {
        return Err(SqliteError::WalCeilingBelowMinimum {
            bytes,
            page_size,
            minimum_bytes,
        });
    }
    Err(SqliteError::WalCapacityUnavailable {
        bytes,
        capability: "WAL I/O limiter",
    })
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
    let conn = Connection::open_with_flags(&target, reader_open_flags())?;
    #[cfg(feature = "namespace-trigram-proto")]
    register_namespace_trigram(&conn)?;
    Ok(conn)
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

#[cfg(feature = "namespace-trigram-proto")]
fn register_namespace_trigram(conn: &Connection) -> Result<(), SqliteError> {
    crate::namespace_trigram_proto::register(conn).map_err(SqliteError::InvalidData)
}

fn register_writer_clock(conn: &Connection) -> Result<(), SqliteError> {
    // Evaluated by SQLite at statement execution, never deterministic: stream
    // observation deadlines use the same UTC microsecond source as note stamps.
    conn.create_scalar_function(
        "khive_now_micros",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |_| Ok(chrono::Utc::now().timestamp_micros()),
    )?;
    Ok(())
}

/// Order-preserving UTC key across Chrono's signed timestamp range, with
/// nanoseconds kept after the sign-adjusted epoch seconds.
pub(crate) fn rfc3339_instant_key(instant: chrono::DateTime<chrono::Utc>) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&((instant.timestamp() as u64) ^ (1_u64 << 63)).to_be_bytes());
    key.extend_from_slice(&instant.timestamp_subsec_nanos().to_be_bytes());
    key
}

/// The outbox deadline grammar is stricter than the general timestamp filter.
/// This is shared by app-maintained stored keys, V44 backfill, and the read
/// residual; no schema expression calls an application-defined function.
pub(crate) fn strict_rfc3339_key(text: &str) -> Option<Vec<u8>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|instant| rfc3339_instant_key(instant.with_timezone(&chrono::Utc)))
}

/// Register timestamp-key functions for read filters on pooled connections.
pub(crate) fn register_rfc3339_key(conn: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    use rusqlite::types::ValueRef;

    conn.create_scalar_function(
        "khive_rfc3339_key",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let text = match ctx.get_raw(0) {
                ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
                _ => None,
            };
            let key = text
                .and_then(|text| text.parse::<chrono::DateTime<chrono::Utc>>().ok())
                .map(rfc3339_instant_key);
            Ok(key)
        },
    )?;
    // The outbox's legacy retry predicate used parse_from_rfc3339, while the
    // general key above accepts Chrono's relaxed DateTime FromStr grammar.
    // Keep the strict grammar separate so a relaxed-only future value still
    // fails open as malformed, instead of postponing the message forever.
    conn.create_scalar_function(
        "khive_rfc3339_strict_key",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let text = match ctx.get_raw(0) {
                ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
                _ => None,
            };
            let key = text.and_then(strict_rfc3339_key);
            Ok(key)
        },
    )?;
    Ok(())
}

fn configure_writer_connection(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<bool, SqliteError> {
    #[cfg(feature = "namespace-trigram-proto")]
    register_namespace_trigram(conn)?;
    register_writer_clock(conn)?;
    register_rfc3339_key(conn)?;
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
    #[cfg(feature = "namespace-trigram-proto")]
    register_namespace_trigram(conn)?;
    register_rfc3339_key(conn)?;
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

fn reset_reader_connection(conn: &Connection, dirty: bool, config: &PoolConfig) -> bool {
    if !conn.is_autocommit() {
        match conn.execute_batch("ROLLBACK") {
            Ok(()) => {}
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
            }
            Err(_) => return false,
        }
        if !conn.is_autocommit() {
            return false;
        }
    }

    if !dirty {
        return true;
    }

    reader_connection_state_is_pristine(conn)
        && reader_connection_settings_match_baseline(conn, config, 0)
}

/// A pooled reader must never carry connection-local state across logical
/// checkouts. Raw-SQL reads run arbitrary caller SQL against the shared
/// pooled connection (`sql_bridge`'s `run_pool_reader_query`), so a
/// `CREATE TEMP TABLE` or `ATTACH DATABASE` from one checkout would
/// otherwise stay visible to whichever later caller draws the same
/// connection back out of the pool. Dropping those objects individually is
/// order-sensitive (triggers and indexes depend on their tables), so their
/// presence is instead treated as reuse-disqualifying: the caller closes
/// the connection and opens a fresh replacement.
///
/// Called only when [`ReaderGuard::dirty`] is set (`reset_reader_connection`'s
/// `dirty` gate) — a typed store read never runs raw caller SQL and returns
/// without paying this scan; only a checkout that executed a `SqlReader`
/// raw-SQL statement (`sql_bridge`'s `run_pool_reader_query`) does.
fn reader_connection_state_is_pristine(conn: &Connection) -> bool {
    let has_temp_objects: bool = match conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_temp_master)",
        [],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(_) => return false,
    };
    if has_temp_objects {
        return false;
    }

    let attached_databases: i64 = match conn.query_row(
        "SELECT COUNT(*) FROM pragma_database_list WHERE name NOT IN ('main', 'temp')",
        [],
        |row| row.get(0),
    ) {
        Ok(v) => v,
        Err(_) => return false,
    };
    attached_databases == 0
}

/// The observable connection-local settings a reader capability could in
/// principle influence, compared against this pool's configured baseline.
/// Called only on a dirty return, alongside [`reader_connection_state_is_pristine`]
/// — the reader-capability admission gate (`sql_bridge::reader_capability_admits`)
/// refuses every raw-SQL form that could change these today, so this is
/// defense in depth against a gap in that gate, not the primary boundary.
///
/// `expected_query_only` is the caller's expected baseline for `query_only`:
/// pooled reader connections never set it explicitly (`configure_reader_connection`
/// does not touch it) regardless of `config.read_only`, so pooled-reader callers
/// pass `0`; the degraded shared-writer-as-reader lease (`max_readers == 0`)
/// inherits whatever `configure_writer_connection` set, which does depend on
/// `config.read_only`.
fn reader_connection_settings_match_baseline(
    conn: &Connection,
    config: &PoolConfig,
    expected_query_only: i64,
) -> bool {
    let expected_busy_timeout_ms =
        i64::try_from(config.busy_timeout.as_millis()).unwrap_or(i64::MAX);
    let expected_cache_size: i64 = CACHE_SIZE_KIB.parse().unwrap_or(-65536);
    let checks: [(&str, i64); 8] = [
        ("query_only", expected_query_only),
        ("writable_schema", 0),
        ("foreign_keys", 1),
        ("busy_timeout", expected_busy_timeout_ms),
        ("cache_size", expected_cache_size),
        ("temp_store", 2),
        ("read_uncommitted", 0),
        ("defer_foreign_keys", 0),
    ];
    checks.iter().all(|(pragma, expected)| {
        conn.pragma_query_value(None, pragma, |row| row.get::<_, i64>(0))
            .map(|actual| actual == *expected)
            .unwrap_or(false)
    })
}

/// Best-effort recovery for a dirty shared reader-writer lease
/// (`max_readers == 0` degraded mode): there is no separate connection to
/// close and replace, so a disqualifying state is instead undone in place —
/// DETACH every non-main/non-temp database, drop every TEMP object in
/// dependency order (views and triggers before the indexes and tables they
/// depend on), then reapply the pool's baseline connection settings. Returns
/// `true` only if the connection verifiably passes the same pristine/settings
/// checks afterward; the caller poisons the lease on `false`.
fn restore_shared_reader_state(conn: &Connection, config: &PoolConfig) -> bool {
    if !detach_non_main_databases(conn) {
        return false;
    }
    if !drop_temp_objects(conn) {
        return false;
    }
    let expected_query_only = i64::from(config.read_only);
    if reset_observable_settings(conn, config, expected_query_only).is_err() {
        return false;
    }
    reader_connection_state_is_pristine(conn)
        && reader_connection_settings_match_baseline(conn, config, expected_query_only)
}

fn detach_non_main_databases(conn: &Connection) -> bool {
    loop {
        let name: Option<String> = match conn.query_row(
            "SELECT name FROM pragma_database_list WHERE name NOT IN ('main', 'temp') LIMIT 1",
            [],
            |row| row.get(0),
        ) {
            Ok(name) => Some(name),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(_) => return false,
        };
        let Some(name) = name else {
            return true;
        };
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        if conn
            .execute_batch(&format!("DETACH DATABASE {quoted}"))
            .is_err()
        {
            return false;
        }
    }
}

fn drop_temp_objects(conn: &Connection) -> bool {
    // Views and triggers depend on tables/indexes but are never depended on
    // themselves; dropping them first means every later DROP TABLE/INDEX
    // never fails on a dangling dependent.
    for (kind, ddl_keyword) in [
        ("view", "VIEW"),
        ("trigger", "TRIGGER"),
        ("index", "INDEX"),
        ("table", "TABLE"),
    ] {
        loop {
            let name: Option<String> = match conn.query_row(
                "SELECT name FROM sqlite_temp_master WHERE type = ?1 LIMIT 1",
                [kind],
                |row| row.get(0),
            ) {
                Ok(name) => Some(name),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(_) => return false,
            };
            let Some(name) = name else {
                break;
            };
            let quoted = format!("\"{}\"", name.replace('"', "\"\""));
            if conn
                .execute_batch(&format!("DROP {ddl_keyword} IF EXISTS temp.{quoted}"))
                .is_err()
            {
                return false;
            }
        }
    }
    true
}

fn reset_observable_settings(
    conn: &Connection,
    config: &PoolConfig,
    expected_query_only: i64,
) -> Result<(), rusqlite::Error> {
    conn.pragma_update(None, "query_only", expected_query_only)?;
    conn.pragma_update(None, "writable_schema", 0)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "read_uncommitted", 0)?;
    conn.pragma_update(None, "defer_foreign_keys", 0)?;
    Ok(())
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
#[path = "runtime_write_routing_tests.rs"]
mod runtime_write_routing_tests;

#[cfg(test)]
#[path = "database_owner_identity_pool_tests.rs"]
mod database_owner_identity_pool_tests;

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
