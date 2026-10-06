//! The public file-backed constructors default the volume-lock directory through
//! the one per-user resolver, so every process of a user shares one directory.
//! Each case runs in a child process because the resolver reads the environment.

use std::path::PathBuf;

use khive_db::{PoolConfig, StorageBackend, WalCeilingPolicy};
use khive_storage::test_support::run_exact_test_in_child;

const CHILD: &str = "KHIVE_VOLUME_LOCK_DIR_DEFAULT_CHILD";

// MUST-FAIL: a constructor that builds its pool from a config other than the
// resolved default leaves a different directory on the pool.
#[test]
fn public_constructors_carry_the_resolved_volume_lock_directory() {
    let root = tempfile::tempdir().expect("fixture root");
    let shared = root.path().join("shared-locks");
    if run_exact_test_in_child(CHILD, false, |command| {
        command.env("KHIVE_VOLUME_LOCK_DIR", &shared);
    }) {
        return;
    }
    let expected = PathBuf::from(
        std::env::var_os("KHIVE_VOLUME_LOCK_DIR").expect("the parent exports the lock directory"),
    );
    let databases = root.path().join("databases");
    std::fs::create_dir(&databases).expect("database directory");
    let backends = [
        StorageBackend::sqlite(databases.join("plain.db")).expect("sqlite"),
        StorageBackend::sqlite_with_max_readers(databases.join("readers.db"), Some(2))
            .expect("sqlite_with_max_readers"),
        StorageBackend::sqlite_with_max_readers_and_wal_ceiling(
            databases.join("ceiling.db"),
            Some(2),
            WalCeilingPolicy::default(),
        )
        .expect("sqlite_with_max_readers_and_wal_ceiling"),
    ];
    for backend in &backends {
        assert_eq!(
            backend.pool().config().volume_lock_dir.as_deref(),
            Some(expected.as_path())
        );
    }
}

// MUST-FAIL: falling back to a working-directory-relative path would make this
// default `Some`.
#[test]
fn pool_config_default_has_no_lock_directory_when_the_user_has_no_home() {
    if run_exact_test_in_child(CHILD, false, |command| {
        command
            .env_remove("KHIVE_VOLUME_LOCK_DIR")
            .env_remove("HOME")
            .env_remove("USERPROFILE");
    }) {
        return;
    }
    assert_eq!(PoolConfig::default().volume_lock_dir, None);
    let error = khive_db::default_volume_lock_dir().expect_err("no home, no override");
    assert!(
        error.to_string().contains("KHIVE_VOLUME_LOCK_DIR"),
        "{error}"
    );
}
