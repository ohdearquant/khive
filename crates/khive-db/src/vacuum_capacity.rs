//! Copy-sized admission headroom for an on-disk SQLite `VACUUM`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use khive_storage::CapacityUnavailablePhase;

use crate::error::SqliteError;

// SQLite permits pages up to 64 KiB. A newly created or empty database still
// needs room for a temporary database and its transaction journal.
const MIN_COPY_BYTES: u64 = 64 * 1024;

/// Estimate additional free space needed by a plain `VACUUM` before it starts.
///
/// SQLite copies the database into a temporary file and copies it back through
/// a rollback journal or WAL. Its documentation allows up to twice the source
/// database size in free space. An uncheckpointed WAL can hold pages absent
/// from the main file, so count both current file lengths in that source size.
/// This is an admission estimate, not a promise that another process cannot
/// consume the sampled headroom after admission.
pub(crate) fn estimate_vacuum_headroom(db_path: &Path) -> Result<u64, SqliteError> {
    estimate_with_lengths(db_path, file_len)
}

fn estimate_with_lengths(
    db_path: &Path,
    mut read_len: impl FnMut(&Path, bool) -> io::Result<Option<u64>>,
) -> Result<u64, SqliteError> {
    let db_bytes = read_len(db_path, false)
        .map_err(|error| metadata_error("main database", db_path, error))?
        .ok_or_else(|| metadata_error("main database", db_path, "file is missing"))?;

    let wal_path = wal_path(db_path);
    let wal_bytes = read_len(&wal_path, true)
        .map_err(|error| metadata_error("WAL", &wal_path, error))?
        .unwrap_or(0);

    db_bytes
        .checked_add(wal_bytes)
        .map(|bytes| bytes.max(MIN_COPY_BYTES))
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| {
            metadata_error(
                "database and WAL",
                db_path,
                "copy-sized headroom overflows u64",
            )
        })
}

fn wal_path(db_path: &Path) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push("-wal");
    PathBuf::from(path)
}

fn file_len(path: &Path, missing_allowed: bool) -> io::Result<Option<u64>> {
    // A dangling symlink must not masquerade as an absent WAL, and a live
    // symlink could point the estimate at a different capacity pool.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular SQLite file",
        )),
        Err(error) if missing_allowed && error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn metadata_error(which: &str, path: &Path, error: impl std::fmt::Display) -> SqliteError {
    SqliteError::CapacityUnavailable {
        phase: CapacityUnavailablePhase::Probe,
        message: format!(
            "cannot estimate VACUUM headroom from {which} at {}: {error}",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_probe_error(error: SqliteError) -> bool {
        matches!(
            error,
            SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Probe,
                ..
            }
        )
    }

    #[test]
    fn measures_main_and_wal_without_allocating_a_copy() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.sqlite");
        fs::write(&db_path, vec![0; 131_072]).unwrap();
        fs::write(wal_path(&db_path), vec![0; 32_768]).unwrap();
        assert_eq!(estimate_vacuum_headroom(&db_path).unwrap(), 327_680);
    }

    #[test]
    fn absent_wal_is_zero_but_an_empty_main_gets_one_page_of_copy_headroom() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.sqlite");
        fs::write(&db_path, b"").unwrap();
        assert_eq!(estimate_vacuum_headroom(&db_path).unwrap(), 131_072);

        fs::write(&db_path, vec![0; 131_072]).unwrap();
        assert_eq!(estimate_vacuum_headroom(&db_path).unwrap(), 262_144);
    }

    #[test]
    fn missing_or_non_regular_main_and_wal_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.sqlite");
        assert!(is_probe_error(
            estimate_vacuum_headroom(&db_path).unwrap_err()
        ));

        fs::create_dir(&db_path).unwrap();
        assert!(is_probe_error(
            estimate_vacuum_headroom(&db_path).unwrap_err()
        ));
        fs::remove_dir(&db_path).unwrap();

        fs::write(&db_path, b"db").unwrap();
        fs::create_dir(wal_path(&db_path)).unwrap();
        assert!(is_probe_error(
            estimate_vacuum_headroom(&db_path).unwrap_err()
        ));
    }

    #[test]
    fn metadata_failure_and_arithmetic_overflow_are_typed_probe_failures() {
        let db_path = Path::new("store.sqlite");
        let error = estimate_with_lengths(db_path, |_, is_wal| {
            if is_wal {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            } else {
                Ok(Some(65_536))
            }
        })
        .unwrap_err();
        assert!(is_probe_error(error));

        let error = estimate_with_lengths(db_path, |_, is_wal| {
            Ok(Some(if is_wal { 1 } else { u64::MAX }))
        })
        .unwrap_err();
        assert!(is_probe_error(error));

        let error = estimate_with_lengths(db_path, |_, is_wal| {
            Ok(Some(if is_wal { 0 } else { u64::MAX / 2 + 1 }))
        })
        .unwrap_err();
        assert!(is_probe_error(error));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_wal_symlink_is_not_treated_as_absent() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.sqlite");
        fs::write(&db_path, b"db").unwrap();
        symlink(dir.path().join("missing-target"), wal_path(&db_path)).unwrap();
        assert!(is_probe_error(
            estimate_vacuum_headroom(&db_path).unwrap_err()
        ));
    }
}
