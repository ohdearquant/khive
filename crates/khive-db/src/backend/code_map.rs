//! Dedicated code-map store construction and its core-schema migration hook.

use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use super::{StorageBackend, StoreSchemaGate};
use crate::error::SqliteError;
use crate::pool::{ConnectionPool, PoolConfig};
use crate::stores::blob::DatabaseGcOwnerGuard;
use rusqlite::{Connection, OpenFlags};

impl StorageBackend {
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn active_code_map_handles_for_test(path: impl AsRef<Path>) -> usize {
        crate::code_map_vfs::active_main_handle_count_for(path.as_ref())
    }

    /// Open a registered rollback-mode map through the handle-proving VFS.
    /// This opens an existing file read-only and does not run migrations.
    pub fn open_code_map_read_only(
        path: impl AsRef<Path>,
        expected_identity: crate::file_identity::DatabaseFileIdentity,
        protected_databases: &[std::path::PathBuf],
    ) -> Result<Connection, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let path = path.as_ref().to_path_buf();
        let protected = protected_databases
            .iter()
            .map(|path| crate::code_map_vfs::ProductionBase {
                path: path.clone(),
                kind: crate::code_map_vfs::ProductionKind::Main,
            })
            .collect();
        let vfs = crate::code_map_vfs::register_read_only_rollback(
            path.clone(),
            expected_identity,
            protected,
        )
        .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
        let connection = crate::code_map_vfs::naming_refusal(&vfs, || {
            Connection::open_with_flags_and_vfs(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                vfs.as_str(),
            )
            .map_err(Into::into)
        })?;
        connection.pragma_update(None, "query_only", true)?;
        Ok(connection)
    }

    /// Open the dedicated code-map store through the native handle-proving
    /// VFS. `protected_main` and `protected_events` are configured production
    /// names; the guard also samples each name's SQLite companions. Disk policy
    /// and the volume-lock directory come from the process environment, with the
    /// per-user default lock directory of [`crate::default_volume_lock_dir`];
    /// callers with captured configuration use [`Self::sqlite_code_map_with_policies`].
    pub fn sqlite_code_map(
        path: impl AsRef<Path>,
        protected_main: &[std::path::PathBuf],
        protected_events: &[std::path::PathBuf],
    ) -> Result<Self, SqliteError> {
        let policy = crate::migrations::MigrationWritePolicy::from_environment()?;
        Self::sqlite_code_map_with_policies(
            path,
            protected_main,
            protected_events,
            policy.disk_guard_config(),
            policy.volume_lock_dir().to_path_buf(),
        )
    }

    /// Open a guarded code-map store using the host's captured disk policy and
    /// absolute shared volume-lock directory. Neither value rereads environment.
    pub fn sqlite_code_map_with_policies(
        path: impl AsRef<Path>,
        protected_main: &[std::path::PathBuf],
        protected_events: &[std::path::PathBuf],
        disk_guard_config: crate::EffectiveDiskGuardConfig,
        volume_lock_dir: std::path::PathBuf,
    ) -> Result<Self, SqliteError> {
        let policy =
            crate::migrations::MigrationWritePolicy::new(disk_guard_config, volume_lock_dir)?;
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let protected: Vec<_> = protected_main
            .iter()
            .map(|path| crate::code_map_vfs::ProductionBase {
                path: path.clone(),
                kind: crate::code_map_vfs::ProductionKind::Main,
            })
            .chain(
                protected_events
                    .iter()
                    .map(|path| crate::code_map_vfs::ProductionBase {
                        path: path.clone(),
                        kind: crate::code_map_vfs::ProductionKind::Events,
                    }),
            )
            .collect();
        crate::code_map_vfs::prepare_rollback_target(resolved.clone(), protected.clone()).map_err(
            |error| {
                let detail = if error.is_busy() {
                    format!("{error}; retry after other SQLite clients release the target")
                } else {
                    error.to_string()
                };
                SqliteError::InvalidData(detail)
            },
        )?;
        let vfs_name = crate::code_map_vfs::register_rollback(resolved.clone(), protected)
            .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(resolved.clone()),
            code_map_vfs: Some(vfs_name),
            wal_mode: false,
            disk_guard_config: Some(policy.disk_guard_config()),
            volume_lock_dir: Some(policy.volume_lock_dir().to_path_buf()),
            ..PoolConfig::default()
        })?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
            path: Some(resolved),
            vector_tables_ready: Default::default(),
            text_tables_ready: Default::default(),
            notes_seq_repair_runs: AtomicUsize::new(0),
            store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
        })
    }

    /// Run the core-schema migrations through this backend's pooled writer,
    /// one admitted unit per migration transaction.
    pub(super) fn run_core_migrations(
        &self,
        owner: &DatabaseGcOwnerGuard,
    ) -> Result<u32, SqliteError> {
        let admission = self.pool.write_admission();
        let run = || {
            crate::migrations::run_migrations_with_database_gc_owner(
                &mut self.pool.migration_transactions(),
                owner,
                &admission,
            )
        };
        match self.pool.config().code_map_vfs.as_deref() {
            // A guarded journal or main open refused mid-migration
            // surfaces as SQLITE_CANTOPEN; name the refusal.
            Some(vfs) => crate::code_map_vfs::naming_refusal(vfs, run),
            None => run(),
        }
    }
}
