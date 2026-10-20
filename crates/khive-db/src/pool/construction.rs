//! Pool construction and initial connection admission.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, OnceLock};

use crossbeam_queue::ArrayQueue;
use khive_storage::tx_registry::TxOrigin;
use khive_storage::CapacityUnavailablePhase;
use parking_lot::Mutex;
use rusqlite::Connection;
use tokio::sync::Semaphore;

use crate::disk_guard_config::{resolve_disk_guard_config, DEFAULT_DISK_GUARD_DEADLINE_MS};
use crate::error::SqliteError;
#[cfg(any(unix, windows))]
use crate::file_identity::database_file_identity;

#[cfg(any(unix, windows))]
use super::{claimed_file_identity, database_file_identity_if_exists, opened_sqlite_file_identity};
use super::{
    code_map, configure_writer_connection, deny_retired_writer, effective_reader_count,
    initialize_database_id_with_admission, mint_db_identity, open_writer_connection,
    read_database_id, read_only_open_target, refuse_home_data_store_in_tests,
    validate_wal_ceiling_at_open, validate_write_admission_deadline, CheckpointOwnershipGate,
    ConnectionPool, PoolConfig, PoolIdentityRegistration, ReaderAcquisitionCounters,
    WriteAdmission, WriterAcquisitionCounters,
};
#[cfg(test)]
use super::{run_identity_open_hook, IdentityOpenStage};

enum ReadOnlyOpenMode {
    FrozenSnapshot,
    Live,
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
        Self::new_with_read_only_open_mode(config, ReadOnlyOpenMode::FrozenSnapshot)
    }

    pub(crate) fn new_read_only_live(mut config: PoolConfig) -> Result<Self, SqliteError> {
        config.read_only = true;
        config.write_queue_enabled = Some(false);
        Self::new_with_read_only_open_mode(config, ReadOnlyOpenMode::Live)
    }

    fn new_with_read_only_open_mode(
        config: PoolConfig,
        read_only_mode: ReadOnlyOpenMode,
    ) -> Result<Self, SqliteError> {
        refuse_home_data_store_in_tests(&config)?;
        validate_write_admission_deadline(config.write_admission_deadline_ms)?;
        if let Some(policy) = config.disk_guard_config {
            policy.validate()?;
        }
        config.wal_ceiling.validate_static(
            config.path.is_some(),
            config.wal_mode,
            config.read_only,
        )?;
        code_map::validate_pool(&config)?;
        if config.path.is_some() && !config.read_only {
            let lock_dir =
                crate::disk_guard_config::require_volume_lock_dir(config.volume_lock_dir.clone())?;
            if !lock_dir.is_absolute() {
                return Err(SqliteError::CapacityUnavailable {
                    phase: CapacityUnavailablePhase::Lock,
                    message: "configured volume-lock directory is not absolute".to_string(),
                });
            }
        }

        // Resolve "no preference" (`None`) now that `path` is known: on for
        // file-backed pools, off for in-memory ones. An explicit `Some(_)`
        // preference is left untouched and always wins.
        let mut config = config;
        let inert_memory_queue_request =
            config.path.is_none() && config.write_queue_enabled == Some(true);
        config.write_queue_enabled =
            Some(config.write_queue_enabled.unwrap_or(config.path.is_some()));
        if inert_memory_queue_request {
            tracing::warn!(target: "khive_db::pool",
                "write queue explicitly requested for an in-memory pool; it is inert because \
                 in-memory pools cannot host a writer task"
            );
        }

        // Mint the physical identity before WAL classification or SQLite open.
        // Every read-only connection below uses this same canonical path (or
        // an immutable URI derived from it), so a symlink cannot split main-file
        // resolution from sidecar resolution.
        let (origin, identity_path) = match config.path.as_ref() {
            Some(path) if config.code_map_vfs.is_some() => code_map::guarded_identity(path),
            Some(path) => {
                let (identity, canonical) = mint_db_identity(path)?;
                (TxOrigin::Database(identity), Some(canonical))
            }
            None => (TxOrigin::Memory, None),
        };
        let (write_admission, disk_guard_config) = if !config.read_only {
            if identity_path.as_deref().and_then(Path::parent).is_some() {
                let disk_guard = match config.disk_guard_config {
                    Some(policy) => policy,
                    None => resolve_disk_guard_config(None, None)?,
                };
                disk_guard.validate()?;
                if disk_guard.legacy_environment_present {
                    tracing::warn!(target: "khive_db::pool",
                        "legacy SQLite reserve setting is deprecated; \
                         use KHIVE_SQLITE_DISK_RESERVE_BYTES"
                    );
                }
                if disk_guard.reserve_bytes == 0 {
                    tracing::warn!(target: "khive_db::pool",
                        "SQLite disk reserve is explicitly zero; \
                         new logical writes will not be floor-refused"
                    );
                }
                (
                    Arc::new(WriteAdmission::new(
                        identity_path.clone(),
                        disk_guard.reserve_bytes,
                        disk_guard.guard_deadline_ms,
                        config.volume_lock_dir.clone(),
                    )?),
                    Some(disk_guard),
                )
            } else {
                (
                    Arc::new(WriteAdmission::new(
                        None,
                        0,
                        DEFAULT_DISK_GUARD_DEADLINE_MS,
                        None,
                    )?),
                    None,
                )
            }
        } else {
            (
                Arc::new(WriteAdmission::new(
                    None,
                    0,
                    DEFAULT_DISK_GUARD_DEADLINE_MS,
                    None,
                )?),
                None,
            )
        };
        let read_only_open_target = match read_only_mode {
            ReadOnlyOpenMode::FrozenSnapshot => {
                read_only_open_target(&config, identity_path.as_deref())?
            }
            // Keep ordinary SQLite locking and change detection for a live WAL.
            // Read-only flags and connection configuration still apply to every slot.
            ReadOnlyOpenMode::Live => identity_path.clone(),
        };
        #[cfg(any(unix, windows))]
        let identity_before_open = identity_path
            .as_deref()
            .map(database_file_identity_if_exists)
            .transpose()?
            .flatten();
        #[cfg(any(unix, windows))]
        claimed_file_identity::verify_before_open(&config, identity_before_open)?;
        let retirement_connection = Connection::open_in_memory()?;
        retirement_connection.authorizer(Some(deny_retired_writer))?;
        // Declared first so a failed bootstrap closes SQLite before releasing its lease.
        let mut initialization_lease = None;
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
        // A newly-created file must still belong to the volume captured from
        // its parent before open, and an existing file must retain that volume.
        write_admission.verify_current_volume()?;
        let opened_database_id = if identity_path.is_some()
            && !config.read_only
            && initial_database_id.is_none()
        {
            match write_admission.check() {
                Ok(()) => {
                    initialization_lease = write_admission.acquire()?;
                    match initialize_database_id_with_admission(&mut writer, Some(&write_admission))
                    {
                        Ok(id) => Some(id),
                        Err(SqliteError::CapacityFloor { .. }) => None,
                        Err(error) => return Err(error),
                    }
                }
                // Recovery must be able to open this pool and its checkpoint
                // connection below the reserve. Physical file pinning still
                // applies; a later pool open can install the nonce.
                Err(SqliteError::CapacityFloor { .. }) => None,
                Err(error) => return Err(error),
            }
        } else {
            initial_database_id
        };
        drop(initialization_lease.take());
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
            retirement_connection: Mutex::new(Some(retirement_connection)),
            #[cfg(any(test, feature = "test-support"))]
            statement_observer,
            main_pool_generation: OnceLock::new(),
            checkpoint_ownership: CheckpointOwnershipGate::new(),
            pooled_writer_retired: AtomicBool::new(false),
            writer_acquisition_counters: Arc::new(WriterAcquisitionCounters::default()),
            write_admission,
            disk_guard_config,
            reader_acquisition_counters: ReaderAcquisitionCounters::default(),
            reader_checkouts: Mutex::new(BTreeMap::new()),
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
            let conn = pool.open_pooled_reader()?;
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
}
