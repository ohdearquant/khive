//! Canonical database paths and persistent opened-file identity checks.

use std::fs;
use std::path::{Path, PathBuf};

use khive_storage::tx_registry::DbIdentity;
use rusqlite::Connection;

use crate::error::SqliteError;
#[cfg(windows)]
use crate::file_identity::sqlite_opened_file_identity;
#[cfg(any(unix, windows))]
use crate::file_identity::{database_file_identity, DatabaseFileIdentity};

use super::{WriteAdmission, DATABASE_ID_TABLE};

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
pub(super) fn mint_db_identity(
    configured_path: &Path,
) -> Result<(DbIdentity, PathBuf), SqliteError> {
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
pub(super) fn database_file_identity_if_exists(
    path: &Path,
) -> Result<Option<DatabaseFileIdentity>, SqliteError> {
    match database_file_identity(path) {
        Ok(identity) => Ok(Some(identity)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(any(unix, windows))]
pub(crate) fn opened_sqlite_file_identity(
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
pub(super) fn read_database_id(conn: &Connection) -> Result<Option<uuid::Uuid>, SqliteError> {
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

#[cfg(test)]
pub(super) fn initialize_database_id(conn: &mut Connection) -> Result<uuid::Uuid, SqliteError> {
    initialize_database_id_with_admission(conn, None)
}

pub(super) fn initialize_database_id_with_admission(
    conn: &mut Connection,
    admission: Option<&WriteAdmission>,
) -> Result<uuid::Uuid, SqliteError> {
    if let Some(id) = read_database_id(conn)? {
        return Ok(id);
    }
    let transaction = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(admission) = admission {
        if let Err(error) = admission.check() {
            let rollback = transaction.rollback();
            return Err(crate::migrations::capacity_refusal_after_rollback(
                conn,
                rollback,
                error,
                "database identity bootstrap",
            ));
        }
    }
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
