use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use khive_db::StorageBackend;

/// Copy `db_path` (and its `-wal` sidecar, if present) into a fresh scratch
/// temp directory and open the copy read-only. The caller must keep the
/// returned `TempDir` alive for as long as the backend is used; dropping it
/// deletes the snapshot. Any `-shm` maintenance the read-only open performs
/// lands on this disposable copy, never on `db_path`'s own sidecar.
pub(crate) fn open_read_only_snapshot(
    db_path: &Path,
) -> Result<(StorageBackend, tempfile::TempDir)> {
    let mut config = khive_runtime::RuntimeConfig {
        db_path: Some(db_path.to_path_buf()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let wal_ceiling = config.resolve_wal_ceiling_policy(true)?;
    let snapshot_dir = tempfile::TempDir::new()
        .context("failed to create a scratch directory for the dry-run db snapshot")?;
    let file_name = db_path
        .file_name()
        .with_context(|| format!("{} has no file name component", db_path.display()))?;
    let snapshot_db = snapshot_dir.path().join(file_name);
    std::fs::copy(db_path, &snapshot_db)
        .with_context(|| format!("failed to snapshot {} for dry-run", db_path.display()))?;

    let wal_path = wal_sidecar_path(db_path);
    if wal_path.exists() {
        let snapshot_wal = wal_sidecar_path(&snapshot_db);
        std::fs::copy(&wal_path, &snapshot_wal)
            .with_context(|| format!("failed to snapshot {} for dry-run", wal_path.display()))?;
    }
    // The read-only open below refuses a non-empty WAL sidecar with no
    // shared-memory index beside it (immutable mode would drop committed
    // frames) and refuses a writable one (a live index is not a snapshot).
    // Carry the `-shm` beside the WAL copy and mark both copies read-only:
    // this private point-in-time copy is exactly the frozen snapshot form
    // that open accepts. `fs::copy` preserves the source's (writable)
    // permission bits, so the freeze is required, not decorative.
    let shm_path = shm_sidecar_path(db_path);
    if shm_path.exists() {
        let snapshot_shm = shm_sidecar_path(&snapshot_db);
        std::fs::copy(&shm_path, &snapshot_shm)
            .with_context(|| format!("failed to snapshot {} for dry-run", shm_path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for sidecar in [
            wal_sidecar_path(&snapshot_db),
            shm_sidecar_path(&snapshot_db),
        ] {
            if sidecar.exists() {
                let mut permissions = std::fs::metadata(&sidecar)
                    .with_context(|| format!("stat snapshot sidecar {}", sidecar.display()))?
                    .permissions();
                permissions.set_mode(0o444);
                std::fs::set_permissions(&sidecar, permissions)
                    .with_context(|| format!("freeze snapshot sidecar {}", sidecar.display()))?;
            }
        }
    }

    let backend = StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(
        &snapshot_db,
        None,
        wal_ceiling,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((backend, snapshot_dir))
}

/// The `-wal` sidecar path SQLite uses alongside a WAL-mode database file.
fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// The `-shm` shared-memory index path SQLite uses alongside a WAL-mode
/// database file.
fn shm_sidecar_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-shm");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_snapshot_validates_captured_wal_environment() {
        if crate::test_process::run_in_child() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("source.db");
        let seed = StorageBackend::sqlite_for_test(&db).unwrap();
        seed.prepare_core_schema().unwrap();
        drop(seed);
        let before = std::fs::read(&db).unwrap();
        std::env::set_var("KHIVE_SQLITE_WAL_CEILING_BYTES", "abc");
        let error = open_read_only_snapshot(&db)
            .err()
            .expect("SNAPSHOT_INVALID_ENV");
        assert!(
            matches!(error.downcast_ref::<khive_runtime::RuntimeError>(), Some(khive_runtime::RuntimeError::Storage(khive_storage::StorageError::Driver { source, .. })) if matches!(source.downcast_ref(), Some(khive_db::SqliteError::InvalidConfig(message)) if message.contains("KHIVE_SQLITE_WAL_CEILING_BYTES")))
        );
        for raw in ["0", "8192"] {
            std::env::set_var("KHIVE_SQLITE_WAL_CEILING_BYTES", raw);
            let (snapshot, _scratch) =
                open_read_only_snapshot(&db).expect("read-only ceiling is diagnostic");
            assert!(snapshot.is_read_only());
            assert_eq!(
                snapshot.pool_arc().config().wal_ceiling.bytes,
                raw.parse::<u64>().unwrap()
            );
            assert_eq!(
                snapshot.pool_arc().config().wal_ceiling.source,
                khive_runtime::WalCeilingSource::Environment
            );
        }
        assert_eq!(std::fs::read(&db).unwrap(), before);
    }

    #[tokio::test]
    async fn snapshot_reads_committed_wal_without_changing_source_files() {
        if crate::test_process::run_in_child() {
            return;
        }
        std::env::set_var("KHIVE_SQLITE_WAL_CEILING_BYTES", "0");
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("source.db");
        let seed = StorageBackend::sqlite_for_test(&db).unwrap();
        seed.prepare_core_schema().unwrap();
        {
            let sql = seed.sql();
            let mut writer = sql.writer().await.unwrap();
            writer.execute_script(
                "CREATE TABLE snapshot_probe(x INTEGER); INSERT INTO snapshot_probe VALUES (7);".to_string()
            ).await.unwrap();
        }
        let source_paths = [db.clone(), wal_sidecar_path(&db), shm_sidecar_path(&db)];
        let before: Vec<Vec<u8>> = source_paths
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        assert!(!before[1].is_empty(), "committed WAL must be present");
        assert!(!before[2].is_empty(), "WAL index must be present");
        let (snapshot, scratch) = open_read_only_snapshot(&db).unwrap();
        assert!(snapshot.is_read_only());
        let copied_db = scratch.path().join("source.db");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [wal_sidecar_path(&copied_db), shm_sidecar_path(&copied_db)] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o444
                );
            }
        }
        let sql = snapshot.sql();
        let mut reader = sql.reader().await.unwrap();
        let row = reader
            .query_scalar(khive_storage::SqlStatement {
                sql: "SELECT x FROM snapshot_probe".to_string(),
                params: vec![],
                label: Some("snapshot WAL regression".to_string()),
            })
            .await
            .unwrap();
        assert!(matches!(row, Some(khive_storage::SqlValue::Integer(7))));
        drop(reader);
        drop(sql);
        drop(snapshot);
        for (path, bytes) in source_paths.iter().zip(before) {
            assert_eq!(
                std::fs::read(path).unwrap(),
                bytes,
                "{} changed",
                path.display()
            );
        }
        let scratch_path = scratch.path().to_path_buf();
        assert!(copied_db.exists());
        drop(scratch);
        assert!(!scratch_path.exists());
        drop(seed);
    }
}
