//! Verify an external file claim before the pool's first mutating statement.
use super::*;

#[cfg(any(unix, windows))]
pub(super) fn verify_before_open(
    config: &PoolConfig,
    at_path: Option<DatabaseFileIdentity>,
) -> Result<(), SqliteError> {
    if let Some(expected) = config.expected_file_identity {
        if Some(expected) != at_path {
            return Err(SqliteError::InvalidData(
                "database file identity differs from the caller's held claim before SQLite open"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn verify_opened(
    config: &PoolConfig,
    writer: &Connection,
    path: Option<&Path>,
) -> Result<(), SqliteError> {
    if let Some(expected) = config.expected_file_identity {
        let path = path.ok_or_else(|| {
            SqliteError::InvalidConfig("a claimed file identity requires a file-backed pool".into())
        })?;
        if opened_sqlite_file_identity(writer, path)? != expected {
            return Err(SqliteError::InvalidData(
                "opened SQLite file identity differs from the caller's held claim".into(),
            ));
        }
    }
    Ok(())
}

/// A claimed target already exists, so SQLite must not create a replacement.
/// On Unix the synchronous open is bounded by the process-startup fstat observer;
/// on Windows SQLite exposes its actual main-file handle directly.
pub(super) fn open_writer(
    config: &PoolConfig,
    read_only_open_target: Option<&Path>,
    identity_path: Option<&Path>,
) -> Result<Connection, SqliteError> {
    let Some(_) = config.path.as_ref() else {
        return Connection::open_in_memory().map_err(Into::into);
    };
    let flags = if config.read_only {
        writer_read_only_open_flags()
    } else {
        writer_open_flags()
    };
    let target = if config.read_only {
        read_only_open_target.ok_or_else(|| {
            SqliteError::InvalidData(
                "file-backed read-only pool has no canonical open target".into(),
            )
        })?
    } else {
        identity_path.ok_or_else(|| {
            SqliteError::InvalidData(
                "file-backed writable pool has no canonical open target".into(),
            )
        })?
    };
    open_connection(config, target, flags, identity_path)
}

/// Shared pre-SQL identity admission for all connections of a claimed pool.
pub(super) fn open_connection(
    config: &PoolConfig,
    target: &Path,
    flags: OpenFlags,
    identity_path: Option<&Path>,
) -> Result<Connection, SqliteError> {
    #[cfg(any(unix, windows))]
    let flags = if config.expected_file_identity.is_some() {
        flags - OpenFlags::SQLITE_OPEN_CREATE
    } else {
        flags
    };
    #[cfg(not(windows))]
    let _ = identity_path;
    #[cfg(unix)]
    let observation = config
        .expected_file_identity
        .map(super::claimed_file_observer::begin)
        .transpose()?;
    #[cfg(all(test, unix))]
    BEFORE_NATIVE_OPEN.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let writer = Connection::open_with_flags(target, flags)?;
    #[cfg(unix)]
    if let Some(observation) = observation {
        // This precedes every PRAGMA, schema read, nonce write and WAL setup.
        observation.finish()?;
    }
    #[cfg(windows)]
    verify_opened(config, &writer, identity_path)?;
    Ok(writer)
}

#[cfg(all(test, unix))]
thread_local! {
    pub(super) static BEFORE_NATIVE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}
