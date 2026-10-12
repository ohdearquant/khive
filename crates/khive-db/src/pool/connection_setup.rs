//! Connection configuration, read-only targets, and reader recycling checks.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::error::SqliteError;

#[cfg(doc)]
use super::ReaderGuard;
use super::{
    claimed_file_identity, code_map, mint_db_identity, register_read_functions,
    register_writer_clock, sqlite_cache_size_pragma, sqlite_mmap_size_pragma, PoolConfig,
    FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
};

pub(super) fn effective_reader_count(config: &PoolConfig, wal_enabled: bool) -> usize {
    if config.path.is_some() && (config.read_only || config.code_map_vfs.is_some()) {
        config.max_readers.max(1)
    } else if config.path.is_some() && config.wal_mode && wal_enabled {
        config.max_readers
    } else {
        0
    }
}

pub(super) fn open_writer_connection(
    config: &PoolConfig,
    read_only_open_target: Option<&Path>,
    identity_path: Option<&Path>,
) -> Result<Connection, SqliteError> {
    claimed_file_identity::open_writer(config, read_only_open_target, identity_path)
}

/// Validate the one-frame reset floor using this backend connection's own
/// page size. This runs before writer configuration changes journal mode or
/// performs any schema work. The WAL I/O limiter arrives in a later slice, so
/// a valid nonzero policy still refuses to open rather than running uncovered.
pub(super) fn validate_wal_ceiling_at_open(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<(), SqliteError> {
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
pub(super) fn read_only_open_target(
    config: &PoolConfig,
    physical_path: Option<&Path>,
) -> Result<Option<PathBuf>, SqliteError> {
    if !config.read_only || config.code_map_vfs.is_some() {
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

pub(super) fn sqlite_header_uses_wal(path: &Path) -> Result<bool, SqliteError> {
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

pub(super) fn open_reader_connection(
    path: &Path,
    config: &PoolConfig,
) -> Result<Connection, SqliteError> {
    let conn = claimed_file_identity::open_connection(
        config,
        path,
        reader_open_flags(),
        config.path.as_deref(),
    )?;
    configure_reader_connection(&conn, config)?;
    Ok(conn)
}

pub(super) fn writer_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// Read-only writer-slot open flags: no `SQLITE_OPEN_CREATE`, so a missing
/// path is rejected rather than silently created.
pub(super) fn writer_read_only_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn reader_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

#[cfg(feature = "namespace-trigram-proto")]
pub(super) fn register_namespace_trigram(conn: &Connection) -> Result<(), SqliteError> {
    crate::namespace_trigram_proto::register(conn).map_err(SqliteError::InvalidData)
}

pub(super) fn configure_writer_connection(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<bool, SqliteError> {
    #[cfg(feature = "namespace-trigram-proto")]
    register_namespace_trigram(conn)?;
    register_writer_clock(conn)?;
    register_read_functions(conn)?;
    if config.read_only {
        // Read-only writer slot: skip write-intent PRAGMAs (journal_mode,
        // wal_autocheckpoint, journal_size_limit all require write access to
        // change) and lock the connection down with query_only instead.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(config.busy_timeout)?;
        conn.pragma_update(None, "cache_size", sqlite_cache_size_pragma())?;
        conn.pragma_update(None, "mmap_size", sqlite_mmap_size_pragma())?;
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
    code_map::require_delete_journal(conn, config)?;

    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", sqlite_cache_size_pragma())?;
    conn.pragma_update(None, "mmap_size", sqlite_mmap_size_pragma())?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // The pool's startup writer always opens before any checkpoint owner can
    // claim the pool, so it starts on the bounded fallback;
    // `claim_checkpoint_ownership` re-applies the pragma on this connection
    // under the writer mutex when a dedicated owner attaches.
    if config.code_map_vfs.is_none() {
        conn.pragma_update(
            None,
            "wal_autocheckpoint",
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
        )?;
    }

    let wal_enabled = wants_wal && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");

    if wal_enabled {
        conn.pragma_update(None, "journal_size_limit", config.journal_size_limit_bytes)?;
    }

    Ok(wal_enabled)
}

pub(super) fn configure_reader_connection(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<(), SqliteError> {
    #[cfg(feature = "namespace-trigram-proto")]
    register_namespace_trigram(conn)?;
    register_read_functions(conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", sqlite_cache_size_pragma())?;
    conn.pragma_update(None, "mmap_size", sqlite_mmap_size_pragma())?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

pub(super) fn current_journal_mode(conn: &Connection) -> Result<String, SqliteError> {
    conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
        .map(|mode| mode.to_ascii_lowercase())
        .map_err(Into::into)
}

pub(super) fn reset_reader_connection(conn: &Connection, dirty: bool, config: &PoolConfig) -> bool {
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
    let expected_cache_size: i64 = sqlite_cache_size_pragma();
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
pub(super) fn restore_shared_reader_state(conn: &Connection, config: &PoolConfig) -> bool {
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
    conn.pragma_update(None, "cache_size", sqlite_cache_size_pragma())?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "read_uncommitted", 0)?;
    conn.pragma_update(None, "defer_foreign_keys", 0)?;
    Ok(())
}

pub(super) fn reader_connection_is_healthy(conn: &Connection) -> bool {
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

pub(super) fn close_connection_quietly(conn: Connection) {
    match conn.close() {
        Ok(()) => {}
        Err((conn, _)) => drop(conn),
    }
}

pub(super) fn pool_exhausted_error(timeout: Duration, max_readers: usize) -> SqliteError {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(format!(
            "Pool exhausted: no reader available after {timeout:?} (max_readers={max_readers})"
        )),
    )
    .into()
}
