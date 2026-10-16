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
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
    }
}

#[test]
fn daemon_claim_blocks_foreign_pool_before_identity_or_wal_initialization() {
    if initialize_in_isolated_child() {
        return;
    }
    for implicit in [false, true] {
        let (_dir, path) = fixture();
        let locks = path.with_file_name("volume-locks");
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
                Some(khive_db::EffectiveDiskGuardConfig::default()),
                Some(&locks),
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
            Some(khive_db::EffectiveDiskGuardConfig::default()),
            Some(&locks),
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
                None,
                None,
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

#[test]
fn sqlite_open_errors_preserve_backend_resolved_path_and_typed_cause() {
    if initialize_in_isolated_child() {
        return;
    }
    for (implicit, read_only) in [(false, false), (false, true), (true, false)] {
        let home = PathBuf::from(std::env::var_os("HOME").expect("isolated child HOME"));
        let dir = tempfile::tempdir_in(&home).unwrap();
        let path = dir.path().join("invalid.db");
        let locks = dir.path().join("volume-locks");
        std::fs::write(&path, [b'X'; 1024]).unwrap();
        let declared_path = PathBuf::from("~")
            .join(dir.path().strip_prefix(&home).unwrap())
            .join("invalid.db");
        let error = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                volume_lock_dir: Some(locks.clone()),
                ..RuntimeConfig::no_embeddings()
            };
            open_single_backend(&mut config, Some(1), None)
        } else {
            open_backend(
                &declared(&declared_path, read_only),
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                None,
                Some(khive_db::EffectiveDiskGuardConfig::default()),
                Some(&locks),
            )
        }
        .err()
        .expect("invalid SQLite bytes must refuse backend construction");
        let cause = error
            .downcast_ref::<khive_db::SqliteError>()
            .expect("open context must preserve the typed SQLite cause");
        assert!(
            matches!(cause, khive_db::SqliteError::Rusqlite(_)),
            "fixture must reach the SQLite driver: {error:#}"
        );
        let expected_path = path.display().to_string();
        assert!(
            !cause.to_string().contains(&expected_path),
            "fixture must require the added path context: {error:#}"
        );
        let message = error.to_string();
        assert!(
            message.contains("backend main"),
            "implicit={implicit}, read_only={read_only}: {message}"
        );
        assert!(
            message.contains(&expected_path),
            "implicit={implicit}, read_only={read_only}: resolved path missing: {message}"
        );
        assert!(
            !message.contains(&declared_path.display().to_string()),
            "error must disclose the opened path rather than the tilde spelling: {message}"
        );
    }
}

// MUST-FAIL: a claimed writable open that builds its pool without the host's
// captured policies leaves the default reserve and lock directory on the pool.
#[test]
fn matching_daemon_claim_opens_writable_backends_with_the_captured_policies() {
    if initialize_in_isolated_child() {
        return;
    }
    let policy = khive_db::EffectiveDiskGuardConfig {
        reserve_bytes: 123,
        guard_deadline_ms: 250,
        ..khive_db::EffectiveDiskGuardConfig::default()
    };
    for implicit in [false, true] {
        let (dir, path) = fixture();
        let locks = dir.path().join("volume-locks");
        let mut guards = acquire_daemon_store_guards(vec![path.clone()]).unwrap();
        bind_daemon_store_files(&mut guards, &[]).unwrap();
        let backend = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                disk_guard_config: Some(policy),
                volume_lock_dir: Some(locks.clone()),
                ..RuntimeConfig::default()
            };
            open_single_backend(&mut config, Some(1), Some(&guards))
        } else {
            open_backend(
                &declared(&path, false),
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                Some(&guards),
                Some(policy),
                Some(&locks),
            )
        }
        .unwrap();
        assert_eq!(backend.pool().effective_disk_guard_config(), Some(policy));
        assert_eq!(
            backend.pool().config().volume_lock_dir.as_deref(),
            Some(locks.as_path())
        );
    }
}

// MUST-FAIL: dropping the lock-directory requirement opens the database
// (and creates its journal files) before any refusal.
#[test]
fn writable_open_without_a_lock_directory_refuses_before_touching_the_database() {
    for implicit in [false, true] {
        let (_dir, path) = fixture();
        let before = std::fs::read(&path).unwrap();
        let error = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                volume_lock_dir: None,
                ..RuntimeConfig::default()
            };
            open_single_backend(&mut config, Some(1), None)
        } else {
            open_backend(
                &declared(&path, false),
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                None,
                Some(khive_db::EffectiveDiskGuardConfig::default()),
                None,
            )
        }
        .err()
        .expect("a writable open needs a lock directory");
        assert!(
            error.to_string().contains("KHIVE_VOLUME_LOCK_DIR"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("db-wal").exists());
        assert!(!path.with_extension("db-shm").exists());
    }
}

// MUST-FAIL: creating the database's parent directory before the writer policy
// is resolved leaves that directory behind when the open is refused.
#[test]
fn writable_open_without_a_lock_directory_creates_no_parent_directory() {
    for implicit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().join("missing");
        let path = top.join("nested").join("main.db");
        let error = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                volume_lock_dir: None,
                ..RuntimeConfig::default()
            };
            open_single_backend(&mut config, Some(1), None)
        } else {
            open_backend(
                &declared(&path, false),
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                None,
                Some(khive_db::EffectiveDiskGuardConfig::default()),
                None,
            )
        }
        .err()
        .expect("a writable open needs a lock directory");
        assert!(
            error.to_string().contains("KHIVE_VOLUME_LOCK_DIR"),
            "{error:#}"
        );
        assert!(!top.exists());
    }
}

// MUST-FAIL: same ordering as above for a declared backend that has a lock
// directory but no resolved disk policy.
#[test]
fn writable_open_without_a_disk_policy_creates_no_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let top = dir.path().join("missing");
    let path = top.join("nested").join("main.db");
    let locks = dir.path().join("volume-locks");
    let error = open_backend(
        &declared(&path, false),
        Some(1),
        khive_db::WalCeilingPolicy::default(),
        None,
        None,
        Some(&locks),
    )
    .err()
    .expect("a writable open needs a disk policy");
    assert!(error.to_string().contains("disk policy"), "{error:#}");
    assert!(!top.exists());
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

#[test]
fn parent_directory_creation_errors_identify_backend_and_failed_path() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    for implicit in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("blocked-parent");
        let sentinel = b"parent is a regular file";
        std::fs::write(&parent, sentinel).unwrap();
        let path = parent.join("main.db");
        let locks = dir.path().join("volume-locks");
        let expected_backend = if implicit { "main" } else { "archive" };
        let error = if implicit {
            let mut config = RuntimeConfig {
                db_path: Some(path.clone()),
                volume_lock_dir: Some(locks.clone()),
                disk_guard_config: Some(khive_db::EffectiveDiskGuardConfig::default()),
                wal_ceiling_bytes: 0,
                wal_ceiling_configured_bytes: 0,
                wal_ceiling_source: khive_runtime::WalCeilingSource::BackendField,
                wal_ceiling_env_raw: None,
                ..RuntimeConfig::no_embeddings()
            };
            open_single_backend(&mut config, Some(1), None)
        } else {
            let mut backend = declared(&path, false);
            backend.name = expected_backend.into();
            open_backend(
                &backend,
                Some(1),
                khive_db::WalCeilingPolicy::default(),
                None,
                Some(khive_db::EffectiveDiskGuardConfig::default()),
                Some(&locks),
            )
        }
        .err()
        .expect("a regular file cannot become the database parent directory");
        let message = error.to_string();
        assert!(message.contains("cannot create"), "{message}");
        assert!(
            message.contains(&format!("backend {expected_backend}")),
            "{message}"
        );
        assert!(message.contains(&parent.display().to_string()), "{message}");
        assert_eq!(std::fs::read(&parent).unwrap(), sentinel);
        assert!(!path.exists());
        assert!(!path.with_extension("db-wal").exists());
        assert!(!path.with_extension("db-shm").exists());
    }
}
