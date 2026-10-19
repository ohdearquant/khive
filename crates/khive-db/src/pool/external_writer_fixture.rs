//! Deliberately uncooperative native connections for corruption/lock fixtures.
//! This module is absent from production and does not call the pool's opener.

use super::{Connection, ConnectionPool, OpenFlags, SqliteError};

/// Open a fixture-owned connection when a test must bypass cooperative write
/// admission (for example an external SQLite lock holder). Admission, physical
/// identity and acquisition-counter tests must use the real typed pool API.
pub(crate) fn open_external_writer_fixture(
    pool: &ConnectionPool,
) -> Result<Connection, SqliteError> {
    let path = pool.canonical_path().ok_or_else(|| {
        SqliteError::InvalidData("external file fixture requires a file-backed pool".into())
    })?;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    pool.configure_standalone_connection(&conn)?;
    Ok(conn)
}
