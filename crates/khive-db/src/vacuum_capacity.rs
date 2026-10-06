//! Copy-sized admission headroom for an on-disk SQLite `VACUUM`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use khive_storage::CapacityUnavailablePhase;

use crate::error::SqliteError;

// SQLite permits pages up to 64 KiB. A newly created or empty database still
// needs room for a temporary database and its transaction journal.
const MIN_COPY_BYTES: u64 = 64 * 1024;
// Each copied page also costs a record header and an index entry: 24 bytes per
// WAL frame plus 8 bytes of wal-index (one 32 KiB block indexes 4096 frames),
// or 8 bytes per rollback-journal page. Counting 32 bytes for every 512 bytes,
// the smallest page size, bounds both modes at any page size.
const MIN_PAGE_BYTES: u64 = 512;
const PAGE_RECORD_BYTES: u64 = 32;
// Costs that do not scale with the copy, each at its largest. A rollback-journal
// header is padded to the sector size, at most 64 KiB; without powersafe
// overwrite, a WAL commit instead repeats its last frame up to the next sector
// boundary, at most one sector plus one 64 KiB frame. The WAL header is 32
// bytes. The wal-index grows in whole 32 KiB blocks: one partly filled block,
// the first block's header, and a second block per mapping on an OS with
// 64 KiB pages. The temporary database's journal holds only its 512-byte header.
const SECTOR_BYTES: u64 = 64 * 1024;
const MAX_FRAME_BYTES: u64 = 64 * 1024 + 24;
const WAL_HEADER_BYTES: u64 = 32;
const WAL_INDEX_ROUNDING_BYTES: u64 = 3 * 32 * 1024;
const TEMP_JOURNAL_HEADER_BYTES: u64 = 512;
const FIXED_BYTES: u64 = SECTOR_BYTES
    + MAX_FRAME_BYTES
    + WAL_HEADER_BYTES
    + WAL_INDEX_ROUNDING_BYTES
    + TEMP_JOURNAL_HEADER_BYTES;

/// Estimate additional free space needed by a plain `VACUUM` before it starts.
///
/// SQLite copies the database into a temporary file and copies it back through
/// a rollback journal or WAL. Its documentation allows up to twice the source
/// database size in free space; the copy back also writes a record header and
/// a wal-index entry per page and fixed headers, so all of them are added to
/// keep the estimate an upper bound. An uncheckpointed WAL can hold pages
/// absent from the main file, so count both current file lengths in that source
/// size. SQLite resolves symlinks in the database path and keeps the WAL beside
/// the resolved file, so the estimate reads the same files.
///
/// The bound assumes the 64 MiB page cache khive's connections configure. In a
/// rollback journal every cache spill that needs a sync starts a new
/// sector-aligned journal header; a connection with a much smaller cache
/// spills often enough to exceed the estimate.
/// This is an admission estimate, not a promise that another process cannot
/// consume the sampled headroom after admission.
pub(crate) fn estimate_vacuum_headroom(db_path: &Path) -> Result<u64, SqliteError> {
    let db_path = fs::canonicalize(db_path)
        .map_err(|error| metadata_error("main database", db_path, error))?;
    estimate_with_lengths(&db_path, file_len)
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
        .and_then(|source| {
            let page_records = source
                .div_ceil(MIN_PAGE_BYTES)
                .checked_mul(PAGE_RECORD_BYTES)?;
            source
                .checked_mul(2)?
                .checked_add(page_records)?
                .checked_add(FIXED_BYTES)
        })
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
        assert_eq!(estimate_vacuum_headroom(&db_path).unwrap(), 567_864);
    }

    #[test]
    fn absent_wal_is_zero_but_an_empty_main_gets_one_page_of_copy_headroom() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.sqlite");
        fs::write(&db_path, b"").unwrap();
        assert_eq!(estimate_vacuum_headroom(&db_path).unwrap(), 365_112);

        fs::write(&db_path, vec![0; 131_072]).unwrap();
        assert_eq!(estimate_vacuum_headroom(&db_path).unwrap(), 500_280);
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
    fn a_main_file_missing_at_read_time_is_not_measured_as_empty() {
        // canonicalize rejects a missing main file first; this covers one that
        // disappears between canonicalize and the length read.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("store.sqlite");
        assert!(is_probe_error(
            estimate_with_lengths(&missing, file_len).unwrap_err()
        ));
        assert_eq!(file_len(&missing, true).unwrap(), None);
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

    #[cfg(unix)]
    #[test]
    fn symlinked_database_is_measured_at_its_target_with_the_targets_wal() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("data")).unwrap();
        let target = dir.path().join("data").join("store.sqlite");
        fs::write(&target, vec![0; 131_072]).unwrap();
        fs::write(wal_path(&target), vec![0; 32_768]).unwrap();
        let link = dir.path().join("store.sqlite");
        symlink(&target, &link).unwrap();
        assert_eq!(estimate_vacuum_headroom(&link).unwrap(), 567_864);
    }

    #[derive(Clone, Copy, Debug)]
    enum VacuumJournal {
        Wal { uncheckpointed_frames: bool },
        Persist,
    }

    fn file_len_or_zero(path: &Path) -> u64 {
        match fs::metadata(path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }

    // Bytes VACUUM added on disk, measured on a real file. WAL, wal-index and
    // PERSIST-mode journal lengths only grow during the VACUUM (no checkpoint
    // runs, and PERSIST keeps the journal's length after commit), so their
    // final lengths are their peaks. The temporary database is unlinked when
    // opened, so it is counted at its largest possible length: VACUUM copies it
    // back page for page, and the result's page count is its page count.
    // Without powersafe overwrite, SQLite pads a WAL commit to a 4 KiB sector
    // boundary and sizes rollback-journal headers to 4 KiB sectors.
    fn vacuum_peak_and_estimate(
        page_size: u32,
        journal: VacuumJournal,
        powersafe_overwrite: bool,
        cache_size: &str,
    ) -> (u64, u64) {
        use rusqlite::OpenFlags;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("store.sqlite");
        let shm_path = dir.path().join("store.sqlite-shm");
        let journal_path = dir.path().join("store.sqlite-journal");
        let conn = rusqlite::Connection::open_with_flags(
            format!(
                "file:{}?psow={}",
                db_path.display(),
                u8::from(powersafe_overwrite)
            ),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        conn.execute_batch(&format!(
            "PRAGMA page_size = {page_size}; PRAGMA temp_store = FILE;
             PRAGMA cache_size = {cache_size};
             CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT NOT NULL, body BLOB NOT NULL);
             CREATE INDEX t_k ON t(k);"
        ))
        .unwrap();
        let insert = |conn: &rusqlite::Connection, ids: std::ops::Range<i64>| {
            conn.execute_batch("BEGIN").unwrap();
            for id in ids {
                let key = format!("{:016x}", id.wrapping_mul(0x1e37_79b9_7f4a_7c15));
                conn.execute(
                    "INSERT INTO t(id, k, body) VALUES (?1, ?2, zeroblob(160))",
                    rusqlite::params![id, key],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        };
        insert(&conn, 0..60_000);
        // A packed file is the worst case: the copy is as large as the source.
        conn.execute_batch("VACUUM").unwrap();
        match journal {
            VacuumJournal::Wal {
                uncheckpointed_frames,
            } => {
                let mode: String = conn
                    .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(mode, "wal");
                conn.execute_batch("PRAGMA wal_autocheckpoint = 0").unwrap();
                if uncheckpointed_frames {
                    insert(&conn, 60_000..70_000);
                    assert!(file_len_or_zero(&wal_path(&db_path)) > 0);
                }
            }
            VacuumJournal::Persist => {
                let mode: String = conn
                    .query_row("PRAGMA journal_mode = PERSIST", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(mode, "persist");
                assert_eq!(file_len_or_zero(&journal_path), 0);
            }
        }

        let paths = [db_path.clone(), wal_path(&db_path), shm_path, journal_path];
        let before = paths.each_ref().map(|path| file_len_or_zero(path));
        let estimate = estimate_vacuum_headroom(&db_path).unwrap();
        conn.execute_batch("VACUUM").unwrap();
        let after = paths.each_ref().map(|path| file_len_or_zero(path));
        let pages: i64 = conn
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .unwrap();

        let temporary_copy = u64::try_from(pages).unwrap() * u64::from(page_size);
        let grown: u64 = before
            .iter()
            .zip(after)
            .map(|(before, after)| after.saturating_sub(*before))
            .sum();
        let copy_back = match journal {
            VacuumJournal::Wal { .. } => {
                assert!(after[2] > 0, "{page_size} {journal:?}: wal-index missing");
                after[1] - before[1]
            }
            VacuumJournal::Persist => after[3],
        };
        assert!(
            temporary_copy > 0 && copy_back >= temporary_copy,
            "{page_size} {journal:?}: the measured copy back must hold every page"
        );
        (temporary_copy + grown, estimate)
    }

    #[test]
    fn a_real_vacuum_never_grows_the_volume_past_the_estimate() {
        for (page_size, powersafe_overwrite) in [512, 4096, 65_536]
            .into_iter()
            .flat_map(|page_size| [(page_size, true), (page_size, false)])
        {
            for journal in [
                VacuumJournal::Wal {
                    uncheckpointed_frames: false,
                },
                VacuumJournal::Wal {
                    uncheckpointed_frames: true,
                },
                VacuumJournal::Persist,
            ] {
                let (peak, estimate) = vacuum_peak_and_estimate(
                    page_size,
                    journal,
                    powersafe_overwrite,
                    crate::pool::CACHE_SIZE_KIB,
                );
                let case = format!("{page_size} {journal:?} psow={powersafe_overwrite}");
                eprintln!("{case}: peak {peak} estimate {estimate}");
                assert!(
                    peak <= estimate,
                    "{case}: VACUUM added {peak} bytes, estimate {estimate}"
                );
            }
        }
    }

    #[test]
    fn a_small_page_cache_falls_outside_the_estimate() {
        // Ten pages spill on almost every copied page, and each spill that needs
        // a sync starts a new sector-aligned journal header: the premise in
        // estimate_vacuum_headroom's documentation is what this arm breaks.
        let (peak, estimate) = vacuum_peak_and_estimate(512, VacuumJournal::Persist, true, "10");
        eprintln!("512 Persist, cache 10 pages: peak {peak} estimate {estimate}");
        assert!(
            peak > estimate,
            "a ten-page cache added {peak} bytes, within the estimate {estimate}"
        );
    }
}
