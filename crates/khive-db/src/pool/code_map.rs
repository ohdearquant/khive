//! Pool support for the code-map database's guarded native VFS.
//!
//! A code-map pool runs in rollback DELETE mode, which supports concurrent
//! readers: its writes take an exclusive lock only for the commit interval.
//! Like a read-only pool it therefore keeps its bounded reader slots, which
//! code-map rebase and diagnostics need.

use std::path::{Path, PathBuf};

use khive_storage::tx_registry::{DbIdentity, TxOrigin};
use rusqlite::{Connection, OpenFlags};

use super::{current_journal_mode, PoolConfig};
use crate::error::SqliteError;

impl PoolConfig {
    /// Open a file-backed connection through the registered code-map VFS when
    /// this pool has one, and through SQLite's default VFS otherwise.
    pub(super) fn open_file_connection(
        &self,
        path: &Path,
        flags: OpenFlags,
    ) -> Result<Connection, SqliteError> {
        match self.code_map_vfs.as_deref() {
            Some(vfs) => crate::code_map_vfs::naming_refusal(vfs, || {
                Connection::open_with_flags_and_vfs(path, flags, vfs).map_err(Into::into)
            }),
            None => Connection::open_with_flags(path, flags).map_err(Into::into),
        }
    }
}

/// A code-map pool is file-backed and runs in rollback DELETE mode, never WAL.
pub(super) fn validate_pool(config: &PoolConfig) -> Result<(), SqliteError> {
    if config.code_map_vfs.is_some() && (config.path.is_none() || config.wal_mode) {
        return Err(SqliteError::InvalidConfig(
            "native code-map VFS requires a file-backed rollback DELETE pool".into(),
        ));
    }
    Ok(())
}

/// The identity of a guarded pool's target. The native guard receives this
/// exact absolute spelling; `mint_db_identity` would follow an explicit
/// symlink before SQLite's first guarded open could refuse it.
pub(super) fn guarded_identity(path: &Path) -> (TxOrigin, Option<PathBuf>) {
    (
        TxOrigin::Database(DbIdentity::new(path.as_os_str().to_os_string())),
        Some(path.to_path_buf()),
    )
}

/// A guarded code-map writer must be in rollback DELETE mode.
pub(super) fn require_delete_journal(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<(), SqliteError> {
    if config.code_map_vfs.is_some() && !current_journal_mode(conn)?.eq_ignore_ascii_case("delete")
    {
        return Err(SqliteError::InvalidData(
            "guarded code-map writer is not in rollback DELETE mode".into(),
        ));
    }
    Ok(())
}
