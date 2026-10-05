use super::store_guard::{
    daemon_store_lock_path, regular_store_identity, set_store_bind_race_hook, store_lock_holder_pid,
};

#[test]
fn store_guard_refuses_a_second_claim_and_names_the_holder() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("stores/khive.db");
    let first = acquire_daemon_store_guards([database.clone()]).expect("first daemon claim");
    let lock = root.join("stores/.khive.db.khived.lock");
    assert!(lock.is_file());
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        std::process::id().to_string()
    );

    let error = acquire_daemon_store_guards([database.clone()])
        .expect_err("a second daemon claim must not own the same store");
    let message = error.to_string();
    assert!(message.contains("already running"), "{message}");
    assert!(
        message.contains(&format!("pid {}", std::process::id())),
        "{message}"
    );
    assert_eq!(
        lock.parent(),
        database.parent(),
        "the guard must be anchored beside the database, independent of the HOME rendezvous"
    );

    drop(first);
    let replacement = acquire_daemon_store_guards([database]).expect("guard released");
    drop(replacement);
    assert!(
        lock.is_file(),
        "the stable lock inode must never be unlinked"
    );
}

#[test]
fn store_guard_refuses_a_database_at_another_stores_lock_sidecar() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let main = root.join("main.db");
    let second_database = root.join(".main.db.khived.lock");
    std::fs::write(&second_database, b"second-backend-sentinel").unwrap();

    let error = acquire_daemon_store_guards([main.clone(), second_database.clone()])
        .expect_err("a store claim must not open another configured database as its lock");
    let message = error.to_string();
    assert!(message.contains(&main.display().to_string()), "{message}");
    assert!(
        message.contains(&second_database.display().to_string()),
        "{message}"
    );
    assert!(message.contains("no store lock was opened"), "{message}");
    assert_eq!(
        std::fs::read(&second_database).unwrap(),
        b"second-backend-sentinel",
        "refusal must precede the lock file's set_len(0)"
    );
    assert!(!main.exists(), "the main database must remain unopened");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        1,
        "preflight must refuse before opening any claim sidecar"
    );
}

#[test]
fn store_guard_refuses_hardlinked_sidecar_before_lock_or_truncate() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("claim.db");
    let temp_database = root.join("temp-database.db");
    let lock = daemon_store_lock_path(&database).unwrap();
    let sentinel = b"temp-database-sentinel";
    std::fs::write(&temp_database, sentinel).unwrap();
    std::fs::hard_link(&temp_database, &lock).unwrap();
    assert!(std::fs::metadata(&temp_database).unwrap().nlink() > 1);
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&temp_database)
        .unwrap();
    holder.try_lock().unwrap();

    let error = acquire_daemon_store_guards([database.clone()])
        .expect_err("opened sidecar hardlink must refuse before lock or truncate");
    let message = error.to_string();
    assert!(message.contains("hard links"), "{message}");
    assert!(message.contains(&lock.display().to_string()), "{message}");
    assert!(
        message.contains(&database.display().to_string()),
        "{message}"
    );
    assert_eq!(std::fs::read(&temp_database).unwrap(), sentinel);
    assert_eq!(std::fs::read(&lock).unwrap(), sentinel);
    assert!(!database.exists());
}

#[test]
fn store_guard_names_a_configured_database_matching_the_opened_sidecar() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let first_database = root.join("a.db");
    let second_database = root.join("z.db");
    let lock = daemon_store_lock_path(&first_database).unwrap();
    let sentinel = b"configured-database-sentinel";
    std::fs::write(&second_database, sentinel).unwrap();
    std::fs::hard_link(&second_database, &lock).unwrap();

    let error = acquire_daemon_store_guards([first_database.clone(), second_database.clone()])
        .expect_err("opened sidecar must not alias another configured database");
    let message = error.to_string();
    assert!(message.contains(&lock.display().to_string()), "{message}");
    assert!(
        message.contains(&first_database.display().to_string()),
        "{message}"
    );
    assert!(
        message.contains(&second_database.display().to_string()),
        "{message}"
    );
    assert_eq!(std::fs::read(&second_database).unwrap(), sentinel);
    assert_eq!(std::fs::read(&lock).unwrap(), sentinel);
}

#[test]
fn store_lock_holder_pid_reads_at_most_64_bytes_from_the_claimed_file() {
    let mut file = tempfile::tempfile().unwrap();
    std::io::Write::write_all(&mut file, &vec![b'9'; 1024]).unwrap();
    std::io::Seek::rewind(&mut file).unwrap();
    assert_eq!(store_lock_holder_pid(&mut file), None);
    assert_eq!(std::io::Seek::stream_position(&mut file).unwrap(), 64);

    file.set_len(0).unwrap();
    std::io::Seek::rewind(&mut file).unwrap();
    std::io::Write::write_all(&mut file, b"4242\n").unwrap();
    std::io::Seek::rewind(&mut file).unwrap();
    assert_eq!(store_lock_holder_pid(&mut file), Some(4242));
}

#[test]
fn store_guard_deduplicates_and_releases_partial_claims() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let first_path = root.join("a.db");
    let second_path = root.join("b.db");
    let first = acquire_daemon_store_guards([second_path.clone()]).unwrap();
    let error =
        acquire_daemon_store_guards([second_path.clone(), first_path.clone(), first_path.clone()])
            .expect_err("overlap on the second lock must refuse the full topology");
    assert!(error.to_string().contains("b.db"), "{error}");
    let independent = acquire_daemon_store_guards([first_path.clone(), first_path])
        .expect("the failed candidate released its earlier lock and deduplicated aliases");
    drop(independent);
    drop(first);
    acquire_daemon_store_guards([second_path]).expect("second store released");
}

#[test]
fn store_guard_drop_releases_lock_with_duplicated_descriptor() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("store.db");
    let guards = acquire_daemon_store_guards([database.clone()]).expect("first claim");
    let duplicate = guards[0]._sidecar.try_clone().expect("duplicate lock fd");

    let error = acquire_daemon_store_guards([database.clone()])
        .expect_err("a live guard must still exclude another claim");
    assert!(error.to_string().contains("already running"), "{error}");

    drop(guards);
    let replacement = acquire_daemon_store_guards([database])
        .expect("dropping the guard must unlock even while a duplicate fd remains open");
    drop(replacement);
    drop(duplicate);
}

#[test]
fn store_guard_missing_database_binds_created_inode_under_claim() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("new.db");
    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    assert!(
        !database.exists(),
        "claiming the sidecar must not create SQLite data"
    );
    bind_daemon_store_files(&mut guards, &[]).unwrap();
    assert!(database.is_file());
    assert_daemon_store_identities(&guards).unwrap();
}

#[test]
fn store_guard_directory_retarget_before_bind_refuses_claimed_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().expect("canonical fixture");
    let claimed_dir = root.join("claimed");
    let moved_dir = root.join("moved");
    let database = claimed_dir.join("new.db");
    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    assert!(!database.exists(), "the database is absent at claim time");

    let retarget = claimed_dir.clone();
    set_store_bind_race_hook(move || {
        std::fs::rename(&retarget, &moved_dir).unwrap();
        std::fs::create_dir(&retarget).unwrap();
    });
    let error = bind_daemon_store_files(&mut guards, &[])
        .expect_err("directory retarget must refuse the claimed store");
    assert!(
        error.to_string().contains("changed inode while binding"),
        "{error}"
    );
    assert!(
        !database.exists(),
        "a replacement directory must not receive the database"
    );
    assert!(
        root.join("moved/.new.db.khived.lock").is_file(),
        "the sidecar remains in the claimed directory"
    );
}

#[test]
fn store_guard_missing_read_only_database_refuses_without_creation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("missing-snapshot.db");
    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    let error = bind_daemon_store_files(&mut guards, std::slice::from_ref(&database))
        .expect_err("read-only database must already exist");
    assert!(
        error.to_string().contains("cannot open claimed database"),
        "{error}"
    );
    assert!(!database.exists());
    assert!(!root.join("missing-snapshot.db-wal").exists());
    assert!(!root.join("missing-snapshot.db-shm").exists());
}

#[test]
fn store_guard_replaced_canonical_inode_refuses_after_open() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("database.db");
    std::fs::write(&database, b"original").unwrap();
    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    bind_daemon_store_files(&mut guards, &[]).unwrap();

    let replacement = root.join("replacement.db");
    std::fs::write(&replacement, b"replacement").unwrap();
    std::fs::rename(&replacement, &database).unwrap();
    let error = assert_daemon_store_identities(&guards)
        .expect_err("a replaced canonical inode must refuse daemon boot");
    assert!(
        error.to_string().contains("changed inode after open"),
        "{error}"
    );
}

#[test]
fn store_guard_hardlink_names_remain_independent_unsupported_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().expect("canonical fixture");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();
    let database = first_dir.join("database.db");
    let hardlink = second_dir.join("alias.db");
    std::fs::write(&database, b"same inode").unwrap();
    std::fs::hard_link(&database, &hardlink).unwrap();
    let original = regular_store_identity(&database).unwrap();
    assert_eq!(original, regular_store_identity(&hardlink).unwrap());

    let first = acquire_daemon_store_guards([database]).unwrap();
    let second = acquire_daemon_store_guards([hardlink])
        .expect("distinct hardlink names have distinct path sidecars and are unsupported aliases");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
}

#[test]
fn store_guard_refuses_to_truncate_a_sidecar_path_that_holds_other_data() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let header = b"SQLite format 3\0";
    let mut sqlite_database = header.to_vec();
    sqlite_database.extend([0x5a_u8; 100]);
    let cases: [(&str, Vec<u8>); 3] = [
        ("sqlite-database", sqlite_database),
        ("sqlite-header-only", header.to_vec()),
        ("overlong-record", vec![b'7'; 65]),
    ];
    for (label, bytes) in cases {
        let database = root.join(label).join("main.db");
        let lock = daemon_store_lock_path(&database).unwrap();
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, &bytes).unwrap();

        let error = acquire_daemon_store_guards([database.clone()])
            .expect_err("a sidecar that holds another file's data must not be truncated");
        let message = error.to_string();
        assert!(
            message.contains(&lock.display().to_string()),
            "{label}: {message}"
        );
        assert!(message.contains("left untouched"), "{label}: {message}");
        assert_eq!(
            std::fs::read(&lock).unwrap(),
            bytes,
            "{label}: the refused sidecar must keep its bytes"
        );
        assert!(!database.exists(), "{label}: no database may be created");
    }
}

#[test]
fn store_guard_reuses_a_short_pid_only_sidecar_after_a_restart() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("restart.db");
    let lock = daemon_store_lock_path(&database).unwrap();
    std::fs::write(&lock, b"123456").unwrap();

    let guards = acquire_daemon_store_guards([database]).expect("a pid-only sidecar is reusable");
    assert_eq!(
        std::fs::read_to_string(&lock).unwrap(),
        std::process::id().to_string()
    );
    drop(guards);
}

#[test]
fn store_guard_refuses_a_database_named_like_a_lock_sidecar() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join(".other.db.khived.lock");

    let error = acquire_daemon_store_guards([database.clone()])
        .expect_err("a database must not carry a lock sidecar's file name");
    let message = error.to_string();
    assert!(
        message.contains(&database.display().to_string()),
        "{message}"
    );
    assert!(message.contains("no store lock was opened"), "{message}");
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        0,
        "the refusal must precede every claim's side effects"
    );
}

#[test]
fn store_guard_read_only_claim_refuses_a_missing_parent_without_creating_anything() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let writable = root.join("a-writable/store.db");
    let snapshot_dir = root.join("z-missing");
    let snapshot = snapshot_dir.join("snapshot.db");

    let error = claim_stores(
        &[writable.clone(), snapshot.clone()],
        std::slice::from_ref(&snapshot),
    )
    .expect_err("a read-only claim must not create its parent directory");
    let message = error.to_string();
    assert!(
        message.contains(&snapshot_dir.display().to_string()),
        "{message}"
    );
    assert!(!snapshot_dir.exists());
    assert!(
        !root.join("a-writable").exists(),
        "the refusal must precede the writable claim that sorts first"
    );
}

#[test]
fn store_guard_creates_a_private_database_and_sqlite_sidecars() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("new.db");
    // SAFETY: umask only changes this process's creation mask, and the
    // isolated child runs no other test.
    unsafe { libc::umask(0) };
    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    assert!(!database.exists());
    bind_daemon_store_files(&mut guards, &[]).unwrap();
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_daemon_store_identities(&guards).unwrap();

    let connection = rusqlite::Connection::open(&database).expect("open claimed database");
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("enable WAL");
    assert_eq!(journal_mode, "wal");
    connection
        .execute_batch(
            "CREATE TABLE permission_fixture (value INTEGER NOT NULL); \
             INSERT INTO permission_fixture VALUES (1);",
        )
        .expect("commit fixture write");
    let value: i64 = connection
        .query_row("SELECT value FROM permission_fixture", [], |row| row.get(0))
        .expect("read committed fixture");
    assert_eq!(value, 1);

    // The last SQLite connection can remove its sidecars when it closes.
    for path in [
        &database,
        &root.join("new.db-wal"),
        &root.join("new.db-shm"),
    ] {
        let metadata = std::fs::metadata(path).expect("written database and live sidecars");
        assert!(metadata.is_file(), "{}", path.display());
        assert!(metadata.len() > 0, "{}", path.display());
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o600,
            "{}",
            path.display()
        );
    }
    assert_daemon_store_identities(&guards).unwrap();
    connection
        .close()
        .unwrap_or_else(|(_, error)| panic!("close fixture connection: {error}"));
    drop(guards);
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn store_guard_preserves_an_existing_database_mode_through_sqlite_shutdown() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("existing.db");
    // SAFETY: this isolated child runs only this test, so no other test observes the mask.
    unsafe { libc::umask(0) };
    let seed = rusqlite::Connection::open(&database).expect("create existing fixture");
    seed.execute_batch(
        "CREATE TABLE permission_fixture (value INTEGER NOT NULL); \
         INSERT INTO permission_fixture VALUES (1);",
    )
    .expect("seed existing database");
    seed.close()
        .unwrap_or_else(|(_, error)| panic!("close seed connection: {error}"));
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
    let original_identity = regular_store_identity(&database).unwrap();
    assert!(original_identity.is_some());
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o644
    );

    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o644
    );
    bind_daemon_store_files(&mut guards, &[]).unwrap();
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let connection = rusqlite::Connection::open(&database).expect("open existing claimed database");
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("enable WAL");
    assert_eq!(journal_mode, "wal");
    connection
        .execute("INSERT INTO permission_fixture VALUES (2)", [])
        .expect("commit another fixture write");
    let values: i64 = connection
        .query_row("SELECT SUM(value) FROM permission_fixture", [], |row| {
            row.get(0)
        })
        .expect("read preserved and new rows");
    assert_eq!(values, 3);

    for path in [
        &database,
        &root.join("existing.db-wal"),
        &root.join("existing.db-shm"),
    ] {
        let metadata = std::fs::metadata(path).expect("written database and live sidecars");
        assert!(metadata.is_file(), "{}", path.display());
        assert!(metadata.len() > 0, "{}", path.display());
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            0o644,
            "{}",
            path.display()
        );
    }
    assert_daemon_store_identities(&guards).unwrap();
    connection
        .close()
        .unwrap_or_else(|(_, error)| panic!("close fixture connection: {error}"));
    drop(guards);
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        regular_store_identity(&database).unwrap(),
        original_identity
    );
}

#[test]
fn store_guard_bind_names_the_read_only_declaration_for_a_write_protected_database() {
    let dir = tempfile::tempdir().expect("fixture");
    let root = dir.path().canonicalize().expect("canonical fixture");
    let database = root.join("snapshot.db");
    std::fs::write(&database, b"frozen").unwrap();
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o444)).unwrap();
    if std::fs::OpenOptions::new()
        .write(true)
        .open(&database)
        .is_ok()
    {
        // The caller bypasses file permissions, so the bind cannot be refused.
        return;
    }

    let mut guards = acquire_daemon_store_guards([database.clone()]).unwrap();
    let error = bind_daemon_store_files(&mut guards, &[])
        .expect_err("a writable bind of a write-protected database must refuse");
    let message = error.to_string();
    assert!(
        message.contains(&database.display().to_string()),
        "{message}"
    );
    assert!(message.contains("no filesystem write bits"), "{message}");
    assert!(message.contains("`read_only = true`"), "{message}");
}
