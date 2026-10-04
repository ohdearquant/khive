use super::*;
use khive_runtime::daemon::{acquire_daemon_store_guards, bind_daemon_store_files};
use std::os::unix::fs::PermissionsExt as _;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("main.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE witness(value INTEGER); INSERT INTO witness VALUES(7)")
        .unwrap();
    (dir, path)
}

fn declared(path: &std::path::Path, read_only: bool) -> BackendConfig {
    BackendConfig {
        name: "main".into(),
        kind: BackendKind::Sqlite,
        path: Some(path.to_path_buf()),
        cache_mb: None,
        journal_mode: None,
        served_kinds: None,
        read_only,
        wal_ceiling_bytes: Some(0),
    }
}

#[test]
fn daemon_claim_blocks_foreign_pool_before_identity_or_wal_initialization() {
    if initialize_in_isolated_child() {
        return;
    }
    for implicit in [false, true] {
        let (_dir, path) = fixture();
        let held_path = path.with_file_name("held.db");
        let mut guards = acquire_daemon_store_guards(vec![path.clone()]).unwrap();
        bind_daemon_store_files(&mut guards, &[]).unwrap();
        std::fs::rename(&path, &held_path).unwrap();
        let other = rusqlite::Connection::open(&path).unwrap();
        other
            .execute_batch("CREATE TABLE foreign_witness(value INTEGER)")
            .unwrap();
        drop(other);
        let held_bytes = std::fs::read(&held_path).unwrap();
        let foreign_bytes = std::fs::read(&path).unwrap();
        let error = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                ..RuntimeConfig::default()
            };
            open_single_backend(&mut config, Some(1), Some(&guards))
        } else {
            open_backend(
                &declared(&path, false),
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                Some(&guards),
            )
        }
        .err()
        .expect("both daemon boot routes must bind SQLite to the held physical file");
        assert!(error.to_string().contains("held claim"), "{error:#}");
        assert_eq!(std::fs::read(&held_path).unwrap(), held_bytes);
        assert_eq!(std::fs::read(&path).unwrap(), foreign_bytes);
        assert!(!path.with_extension("db-wal").exists());
        assert!(!path.with_extension("db-shm").exists());

        // Ordinary unclaimed construction remains supported for this valid file.
        let control = open_backend(
            &declared(&path, false),
            Some(1),
            khive_db::WalCeilingPolicy::default(),
            None,
        )
        .unwrap();
        assert!(!control.is_read_only());
    }
}

#[test]
fn matching_daemon_claim_opens_read_only_snapshots_in_both_boot_routes() {
    if initialize_in_isolated_child() {
        return;
    }
    for implicit in [false, true] {
        let (_dir, path) = fixture();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let mut guards = acquire_daemon_store_guards(vec![path.clone()]).unwrap();
        bind_daemon_store_files(&mut guards, std::slice::from_ref(&path)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let backend = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                ..RuntimeConfig::default()
            };
            open_single_backend(&mut config, Some(1), Some(&guards))
        } else {
            open_backend(
                &declared(&path, true),
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                Some(&guards),
            )
        }
        .unwrap();
        assert!(backend.is_read_only());
        assert_eq!(
            backend
                .pool()
                .writer()
                .unwrap()
                .query_row("SELECT value FROM witness", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("db-wal").exists());
        assert!(!path.with_extension("db-shm").exists());
    }
}

fn initialize_in_isolated_child() -> bool {
    if crate::test_isolation::rerun_with_private_home() {
        return true;
    }
    // SAFETY: the isolation helper selected only this test in a fresh child;
    // the fixture has not opened any SQLite database yet.
    unsafe { khive_db::pool::initialize_claimed_file_observer().unwrap() };
    false
}
