use super::*;

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
    /// Registered native code-map VFS name; set only by the code-map constructor.
    pub code_map_vfs: Option<String>,
    /// File identity pinned by the caller before this pool opens SQLite.
    /// A mismatch is refused before identity initialization or WAL setup.
    #[cfg(any(unix, windows))]
    pub expected_file_identity: Option<DatabaseFileIdentity>,
    /// Number of reader connections (default: min(num_cpus, 8)).
    pub max_readers: usize,
    /// Retire a dedicated reader on return when its connection age exceeds
    /// this duration. `KHIVE_READER_MAX_AGE_SECS`, default 300 seconds.
    /// Does not expire an outstanding lease or recycle the shared writer.
    pub reader_max_age: Duration,
    /// Retire a dedicated reader on return after more than this many
    /// successful checkouts. `KHIVE_READER_MAX_OPS`, default 5000.
    /// Counts leases, not SQL statements; zero retires every returned lease.
    pub reader_max_ops: u64,
    /// WAL mode (must be true for pooling to work; default: true).
    pub wal_mode: bool,
    /// Busy timeout per connection (default: 30s).
    ///
    /// Overridable via `KHIVE_BUSY_TIMEOUT_SECS`.
    pub busy_timeout: Duration,
    /// Time to wait for a reader connection before returning an error (default: 5s).
    ///
    /// For a writer it bounds only the pool-mutex wait. A writable file-backed
    /// pool takes the volume lease first, bounded by the guard deadline
    /// (`disk_guard_deadline_ms`, default 2000 ms), so the effective writer
    /// wait under contention is the guard deadline, then `checkout_timeout`:
    /// a 50 ms value here can still wait about 2 s, and a wait that ends at
    /// the lease returns `CapacityUnavailable` with phase `lock` rather than a
    /// checkout timeout. Diagnostics report both bounds and their sum.
    ///
    /// Overridable via `KHIVE_CHECKOUT_TIMEOUT_SECS`.
    pub checkout_timeout: Duration,
    /// Warn once per observed episode while an outstanding pooled checkout
    /// exceeds this age. Captured from `KHIVE_READER_CHECKOUT_WARN_SECS`
    /// (default: 10 seconds); this never cancels or closes a reader.
    pub reader_checkout_warn_after: Duration,
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
    /// SQLite disk-reserve and guard-deadline policy for this pool. `None`
    /// resolves the process environment when the pool opens.
    pub disk_guard_config: Option<EffectiveDiskGuardConfig>,
    /// Shared per-user directory for the volume advisory lock files.
    /// [`PoolConfig::default`] resolves it with [`crate::default_volume_lock_dir`]
    /// and leaves `None` when no directory can be resolved.
    pub volume_lock_dir: Option<PathBuf>,
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
            code_map_vfs: None,
            #[cfg(any(unix, windows))]
            expected_file_identity: None,
            max_readers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(1, DEFAULT_READER_CAP),
            reader_max_age: Duration::from_secs(crate::env::env_parse_or(
                "KHIVE_READER_MAX_AGE_SECS",
                300,
            )),
            reader_max_ops: crate::env::env_parse_or("KHIVE_READER_MAX_OPS", 5000),
            wal_mode: true,
            busy_timeout: Duration::from_secs(crate::env::env_parse_or(
                "KHIVE_BUSY_TIMEOUT_SECS",
                30,
            )),
            checkout_timeout: Duration::from_secs(crate::env::env_parse_or(
                "KHIVE_CHECKOUT_TIMEOUT_SECS",
                5,
            )),
            reader_checkout_warn_after: Duration::from_secs(crate::env::env_parse_or(
                "KHIVE_READER_CHECKOUT_WARN_SECS",
                10,
            )),
            journal_size_limit_bytes: crate::env::env_parse_or(
                "KHIVE_JOURNAL_SIZE_LIMIT_BYTES",
                DEFAULT_JOURNAL_SIZE_LIMIT_BYTES,
            ),
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
            write_admission_deadline_ms: crate::env::env_parse_or(
                "KHIVE_WRITE_ADMISSION_DEADLINE_MS",
                DEFAULT_WRITE_ADMISSION_DEADLINE_MS,
            ),
            disk_guard_config: None,
            #[cfg(test)]
            volume_lock_dir: Some(test_volume_lock_dir()),
            #[cfg(not(test))]
            volume_lock_dir: crate::default_volume_lock_dir().ok(),
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
            volume_lock_dir: Some(test_volume_lock_dir()),
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
pub(super) fn refuse_home_data_store_in_tests(config: &PoolConfig) -> Result<(), SqliteError> {
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
pub(super) fn validate_write_admission_deadline(deadline_ms: u64) -> Result<(), SqliteError> {
    if WRITE_ADMISSION_DEADLINE_MS_RANGE.contains(&deadline_ms) {
        return Ok(());
    }
    Err(SqliteError::InvalidConfig(format!(
        "write_admission_deadline_ms must be in [{}, {}] ms, got {deadline_ms}",
        WRITE_ADMISSION_DEADLINE_MS_RANGE.start(),
        WRITE_ADMISSION_DEADLINE_MS_RANGE.end()
    )))
}
