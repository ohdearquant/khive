use super::*;
use crate::file_identity::database_file_identity_from_file;

fn seed(path: &Path, marker: &str) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(&format!(
        "CREATE TABLE {marker}(value INTEGER); INSERT INTO {marker} VALUES(7)"
    ))
    .unwrap();
}

#[test]
fn changed_claim_is_refused_before_database_identity_or_wal_writes() {
    if initialize_in_isolated_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let held_path = dir.path().join("held.db");
    seed(&path, "claimed_marker");
    let held = std::fs::File::open(&path).unwrap();
    let expected = database_file_identity_from_file(&held).unwrap();
    std::fs::rename(&path, &held_path).unwrap();
    seed(&path, "unclaimed_marker");
    let held_bytes = std::fs::read(&held_path).unwrap();
    let other_bytes = std::fs::read(&path).unwrap();
    assert_ne!(database_file_identity(&path).unwrap(), expected);

    let error = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        expected_file_identity: Some(expected),
        ..PoolConfig::for_test()
    })
    .err()
    .expect("opening another physical database must not satisfy a held claim");
    assert!(error.to_string().contains("held claim"), "{error}");
    assert_eq!(std::fs::read(&held_path).unwrap(), held_bytes);
    assert_eq!(std::fs::read(&path).unwrap(), other_bytes);
    assert!(!path.with_extension("db-wal").exists());
    assert!(!path.with_extension("db-shm").exists());

    // This input is otherwise a valid pool target, so the refusal witnesses
    // the external claim rather than a corrupt or inaccessible fixture.
    let unclaimed = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::for_test()
    })
    .unwrap();
    assert_ne!(unclaimed.opened_file_identity_record(), Some(expected));
    let conn = unclaimed.writer().unwrap();
    assert_eq!(
        conn.query_row("SELECT value FROM unclaimed_marker", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        7
    );
}

#[test]
fn matching_claim_allows_normal_identity_initialization_and_wal() {
    if initialize_in_isolated_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    seed(&path, "claimed_marker");
    let held = std::fs::File::open(&path).unwrap();
    let expected = database_file_identity_from_file(&held).unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        expected_file_identity: Some(expected),
        ..PoolConfig::for_test()
    })
    .unwrap();
    assert_eq!(pool.opened_file_identity_record(), Some(expected));
    let conn = pool.writer().unwrap();
    assert_eq!(
        conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert!(read_database_id(&conn).unwrap().is_some());
}

#[test]
fn claimed_identity_cannot_be_attached_to_an_in_memory_pool() {
    if initialize_in_isolated_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    seed(&path, "claimed_marker");
    let expected = database_file_identity(&path).unwrap();
    assert!(ConnectionPool::new(PoolConfig {
        expected_file_identity: Some(expected),
        ..PoolConfig::for_test()
    })
    .is_err());
}

/// A fresh single-test process establishes the startup precondition without
/// changing the default VFS while unrelated parallel fixtures may be using it.
fn initialize_in_isolated_child() -> bool {
    in_isolated_child(true)
}

fn in_isolated_child(initialize: bool) -> bool {
    const CHILD: &str = "KHIVE_DB_CLAIMED_FILE_CHILD";
    let thread = std::thread::current();
    let name = thread.name().unwrap();
    if let Some(selected) = std::env::var_os(CHILD) {
        assert_eq!(selected, name);
        // SAFETY: the parent selected exactly this test in a fresh process;
        // this runs before the fixture performs any SQLite operation.
        if initialize {
            unsafe { super::initialize_claimed_file_observer().unwrap() };
        }
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[test]
fn descriptor_claim_refuses_foreign_open_even_after_pathname_is_restored() {
    exercise_descriptor_swap(None);
}

#[test]
fn later_pooled_reader_refuses_foreign_descriptor_before_sql() {
    exercise_descriptor_swap(Some(LaterConnection::PooledReader));
}

#[test]
fn standalone_writer_refuses_foreign_descriptor_before_sql() {
    exercise_descriptor_swap(Some(LaterConnection::StandaloneWriter));
}

#[test]
fn standalone_reader_refuses_foreign_descriptor_before_sql() {
    exercise_descriptor_swap(Some(LaterConnection::StandaloneReader));
}

enum LaterConnection {
    PooledReader,
    StandaloneWriter,
    StandaloneReader,
}

fn exercise_descriptor_swap(later: Option<LaterConnection>) {
    if initialize_in_isolated_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let held_path = dir.path().join("held.db");
    let foreign_path = dir.path().join("foreign.db");
    seed(&path, "claimed_marker");
    seed(&foreign_path, "foreign_marker");
    let held = std::fs::File::open(&path).unwrap();
    let expected = database_file_identity_from_file(&held).unwrap();
    let foreign_identity = database_file_identity(&foreign_path).unwrap();
    assert_ne!(expected, foreign_identity);
    let pool = later.as_ref().map(|_| {
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            expected_file_identity: Some(expected),
            max_readers: 1,
            ..PoolConfig::for_test()
        })
        .unwrap()
    });
    let claimed_bytes = std::fs::read(&path).unwrap();
    let foreign_bytes = std::fs::read(&foreign_path).unwrap();
    let observed = std::rc::Rc::new(std::cell::Cell::new(None));

    let target = path.clone();
    let held_target = held_path.clone();
    let foreign_target = foreign_path.clone();
    claimed_file_identity::BEFORE_NATIVE_OPEN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            // The pre-open pathname check saw A. SQLite will actually open B.
            std::fs::rename(&target, &held_target).unwrap();
            std::fs::rename(&foreign_target, &target).unwrap();
        }));
    });
    let target = path.clone();
    let held_target = held_path.clone();
    let foreign_target = foreign_path.clone();
    let callback_observed = observed.clone();
    claimed_file_observer::set_test_hook(Some(Box::new(move |actual| {
        // This callback follows native fstat of the descriptor SQLite owns.
        // Restore A's pathname while SQLite continues holding B's descriptor.
        callback_observed.set(Some(actual));
        std::fs::rename(&target, &foreign_target).unwrap();
        std::fs::rename(&held_target, &target).unwrap();
    })));
    let result = match later {
        None => ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            expected_file_identity: Some(expected),
            ..PoolConfig::for_test()
        })
        .map(|_| ()),
        Some(kind) => {
            let pool = pool.as_ref().unwrap();
            match kind {
                LaterConnection::PooledReader => pool.open_reader_connection(),
                LaterConnection::StandaloneWriter => pool.open_standalone_writer_untracked(),
                LaterConnection::StandaloneReader => pool.open_standalone_reader(
                    StandaloneReaderPurpose::DiagnosticsIndependentSnapshot,
                ),
            }
            .map(|_| ())
        }
    };
    claimed_file_observer::set_test_hook(None);
    let error = result.expect_err("a restored pathname must not authorize B's descriptor");
    assert_eq!(observed.get(), Some(foreign_identity.unix_parts()));
    assert!(
        error.to_string().contains("opened SQLite descriptor"),
        "{error}"
    );
    assert_eq!(database_file_identity(&path).unwrap(), expected);
    assert_eq!(std::fs::read(&path).unwrap(), claimed_bytes);
    assert_eq!(std::fs::read(&foreign_path).unwrap(), foreign_bytes);
    let untouched = if pool.is_some() {
        vec![&foreign_path]
    } else {
        vec![&path, &foreign_path]
    };
    for database in untouched {
        assert!(!database.with_extension("db-wal").exists());
        assert!(!database.with_extension("db-shm").exists());
        let conn = Connection::open(database).unwrap();
        assert!(read_database_id(&conn).unwrap().is_none());
    }
}

#[test]
fn claimed_pool_without_process_startup_observer_refuses() {
    if in_isolated_child(false) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    seed(&path, "claimed_marker");
    let held = std::fs::File::open(&path).unwrap();
    let expected = database_file_identity_from_file(&held).unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        expected_file_identity: Some(expected),
        ..PoolConfig::for_test()
    })
    .err()
    .expect("a claimed pool must not lazily mutate a live VFS syscall table");
    assert!(
        error
            .to_string()
            .contains("not initialized at process startup"),
        "{error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("db-wal").exists());
    assert!(!path.with_extension("db-shm").exists());
    ConnectionPool::new(PoolConfig {
        path: Some(path),
        ..PoolConfig::for_test()
    })
    .unwrap();
}

#[test]
fn observation_without_native_descriptor_evidence_refuses() {
    if initialize_in_isolated_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    seed(&path, "claimed_marker");
    let held = std::fs::File::open(&path).unwrap();
    let expected = database_file_identity_from_file(&held).unwrap();
    let guard = claimed_file_observer::begin(expected).unwrap();
    let error = guard.finish().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no actual descriptor identity observation"),
        "{error}"
    );
    // Refusal restores the TLS scope, so it cannot poison the next real open.
    ConnectionPool::new(PoolConfig {
        path: Some(path),
        expected_file_identity: Some(expected),
        ..PoolConfig::for_test()
    })
    .unwrap();
}
