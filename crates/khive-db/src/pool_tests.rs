use super::*;
use serial_test::serial;

struct IdentityOpenHookReset;

impl Drop for IdentityOpenHookReset {
    fn drop(&mut self) {
        IDENTITY_OPEN_HOOK.with(|hook| *hook.borrow_mut() = None);
    }
}

fn install_identity_open_hook(
    hook: impl Fn(&Path, IdentityOpenStage, Option<&Connection>) + 'static,
) -> IdentityOpenHookReset {
    IDENTITY_OPEN_HOOK.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "identity open hook already installed"
        );
        *slot.borrow_mut() = Some(Box::new(hook));
    });
    IdentityOpenHookReset
}

struct StartupSpaceProbeReset;

impl Drop for StartupSpaceProbeReset {
    fn drop(&mut self) {
        STARTUP_SPACE_PROBE.with(|probe| *probe.borrow_mut() = None);
    }
}

fn install_startup_space_probe(
    floor_bytes: u64,
    probe: impl Fn(&Path) -> std::io::Result<u64> + Send + Sync + 'static,
) -> StartupSpaceProbeReset {
    STARTUP_SPACE_PROBE.with(|slot| {
        assert!(
            slot.borrow().is_none(),
            "startup space probe already installed"
        );
        *slot.borrow_mut() = Some((floor_bytes, Arc::new(probe)));
    });
    StartupSpaceProbeReset
}

#[cfg(unix)]
fn rename_pair_on_other_thread(from_a: &Path, to_a: &Path, from_b: &Path, to_b: &Path) {
    let paths = (
        from_a.to_path_buf(),
        to_a.to_path_buf(),
        from_b.to_path_buf(),
        to_b.to_path_buf(),
    );
    std::thread::spawn(move || {
        fs::rename(paths.0, paths.1).unwrap();
        fs::rename(paths.2, paths.3).unwrap();
    })
    .join()
    .unwrap();
}

mod timing {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test_support/timing.rs"
    ));
}

#[test]
fn constructor_writer_cancels_after_entering_the_wait_without_pool_timeout() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        checkout_timeout: Duration::from_secs(1),
        ..PoolConfig::default()
    })
    .unwrap();
    let held = pool.writer().unwrap();
    let before = pool.writer_acquisition_snapshot();
    let checks = Cell::new(0);
    let stopped = pool
        .writer_until(|| {
            checks.set(checks.get() + 1);
            checks.get() == 2
        })
        .unwrap();
    assert!(
        stopped.is_none(),
        "second predicate check must stop an in-flight wait"
    );
    assert_eq!(checks.get(), 2);
    assert_eq!(pool.writer_acquisition_snapshot(), before);
    drop(held);
}

#[tokio::test]
async fn constructor_writer_observes_absolute_blocking_deadline() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        checkout_timeout: Duration::from_secs(5),
        ..PoolConfig::default()
    })
    .unwrap();
    let held = pool.writer().unwrap();
    let context = khive_storage::scope_request_read_deadline(Duration::from_millis(20), async {
        khive_storage::capture_request_read_context()
    })
    .await;
    let before = pool.writer_acquisition_snapshot();
    let started = Instant::now();
    let stopped = pool
        .writer_until(|| context.blocking_stop_reason().is_some())
        .unwrap();
    assert!(stopped.is_none());
    if let Some(bound) = timing::duration_bound(Duration::from_secs(1), None) {
        assert!(
            started.elapsed() < bound,
            "request deadline must beat pool timeout within {bound:?}"
        );
    }
    assert_eq!(pool.writer_acquisition_snapshot(), before);
    drop(held);
}

#[test]
fn constructor_writer_preserves_uncancelled_checkout_timeout() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        checkout_timeout: Duration::from_millis(5),
        ..PoolConfig::default()
    })
    .unwrap();
    let held = pool.writer().unwrap();
    let before = pool.writer_acquisition_snapshot();
    let result = pool.writer_until(|| false);
    assert!(
        matches!(result, Err(SqliteError::WriterPoolCheckoutTimeout { timeout }) if timeout == Duration::from_millis(5))
    );
    let after = pool.writer_acquisition_snapshot();
    assert_eq!(after.timeouts, before.timeouts + 1);
    assert_eq!(after.pooled_acquisitions, before.pooled_acquisitions);
    drop(held);
}

struct WarningCapture {
    messages: Arc<std::sync::Mutex<Vec<String>>>,
}

impl tracing::Subscriber for WarningCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        struct Visitor(Option<String>);

        impl tracing::field::Visit for Visitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = Some(format!("{value:?}"));
                }
            }
        }

        let mut visitor = Visitor(None);
        event.record(&mut visitor);
        if let Some(message) = visitor.0 {
            self.messages.lock().unwrap().push(message);
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// Restores the process CWD on drop — including on panic — so a mid-test
/// assertion failure (or an unexpected panic from the code under test)
/// can never leave the process chdir'd into a `tempfile::tempdir()` that
/// unwinds out from under every later test sharing this process.
struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn enter(dir: &Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

const POOL_ENV_VARS: [&str; 7] = [
    "KHIVE_BUSY_TIMEOUT_SECS",
    "KHIVE_CHECKOUT_TIMEOUT_SECS",
    "KHIVE_WAL_AUTOCHECKPOINT_PAGES",
    "KHIVE_JOURNAL_SIZE_LIMIT_BYTES",
    "KHIVE_WRITE_QUEUE",
    "KHIVE_WRITE_QUEUE_CAPACITY",
    "KHIVE_WRITE_ROUTING",
];

struct PoolEnvGuard {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl PoolEnvGuard {
    fn capture() -> Self {
        Self {
            saved: POOL_ENV_VARS
                .into_iter()
                .map(|key| (key, std::env::var_os(key)))
                .collect(),
        }
    }
}

impl Drop for PoolEnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn clear_pool_env() -> PoolEnvGuard {
    let guard = PoolEnvGuard::capture();
    for var in POOL_ENV_VARS {
        std::env::remove_var(var);
    }
    guard
}

fn wal_autocheckpoint_pages(conn: &Connection) -> u32 {
    conn.pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
        .expect("read PRAGMA wal_autocheckpoint")
}

fn journal_size_limit_bytes(conn: &Connection) -> i64 {
    conn.pragma_query_value(None, "journal_size_limit", |row| row.get(0))
        .expect("read PRAGMA journal_size_limit")
}

#[test]
fn read_only_rollback_journal_pool_keeps_a_dedicated_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("read_only_delete_journal.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY);")
            .unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "delete");
    }

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();

    assert!(
        pool.max_readers() > 0,
        "a read-only rollback-journal snapshot must use a genuine read-only reader, not \
         alias reader() onto the query-only writer slot"
    );
    let reader = pool.reader().expect("dedicated read-only reader checkout");
    let count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    drop(reader);
    assert_eq!(
        pool.writer_acquisition_snapshot(),
        WriterAcquisitionSnapshot::default(),
        "constructing and reading a rollback-journal snapshot must never acquire the writer"
    );
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn directory_entries(path: &Path) -> Vec<std::ffi::OsString> {
    let mut entries = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

/// A persistent-WAL snapshot can carry committed rows that exist only in
/// `<db>-wal`. With no copied `-shm`, immutable SQLite silently ignores
/// those frames while ordinary read-only SQLite creates a new `-shm`.
/// Refuse before either open strategy can lose data or mutate the source.
#[test]
fn read_only_persistent_wal_without_shm_is_refused_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("wal-source.db");
    let snapshot = dir.path().join("snapshot ?#%.db");
    let source_wal = sqlite_sidecar(&source, "-wal");
    let snapshot_wal = sqlite_sidecar(&snapshot, "-wal");
    let snapshot_shm = sqlite_sidecar(&snapshot, "-shm");

    let source_conn = Connection::open(&source).unwrap();
    let mode: String = source_conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    source_conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    source_conn
        .execute_batch(
            "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
             INSERT INTO snapshot_row(body) VALUES ('committed-only-in-wal');",
        )
        .unwrap();
    assert!(source_wal.exists(), "fixture must retain a WAL sidecar");

    std::fs::copy(&source, &snapshot).unwrap();
    std::fs::copy(&source_wal, &snapshot_wal).unwrap();
    assert!(
        !snapshot_shm.exists(),
        "fixture intentionally omits the transient shared-memory index"
    );

    let main_before = std::fs::read(&snapshot).unwrap();
    let wal_before = std::fs::read(&snapshot_wal).unwrap();
    let entries_before = directory_entries(dir.path());

    let error = match ConnectionPool::new(PoolConfig {
        path: Some(snapshot.clone()),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("a non-empty WAL without its frozen -shm must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("would omit committed WAL frames"),
        "diagnostic must explain why neither unsafe open mode is allowed: {error}"
    );

    assert_eq!(std::fs::read(&snapshot).unwrap(), main_before);
    assert_eq!(std::fs::read(&snapshot_wal).unwrap(), wal_before);
    assert_eq!(directory_entries(dir.path()), entries_before);
    assert!(
        !snapshot_shm.exists(),
        "read-only admission and every reader must keep the source free of -shm"
    );

    drop(source_conn);
}

/// A complete frozen WAL snapshot includes the WAL index. Once all three
/// files are read-only, ordinary SQLite read-only mode consumes the
/// committed WAL frames without changing the source. This is intentionally
/// not `immutable=1`: immutable SQLite ignores WAL contents.
#[test]
fn read_only_persistent_wal_with_read_only_shm_reads_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("wal-source.db");
    let snapshot = dir.path().join("frozen-wal-snapshot.db");
    let source_wal = sqlite_sidecar(&source, "-wal");
    let source_shm = sqlite_sidecar(&source, "-shm");
    let snapshot_wal = sqlite_sidecar(&snapshot, "-wal");
    let snapshot_shm = sqlite_sidecar(&snapshot, "-shm");

    let source_conn = Connection::open(&source).unwrap();
    let mode: String = source_conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    source_conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    source_conn
        .execute_batch(
            "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
             INSERT INTO snapshot_row(body) VALUES ('committed-only-in-wal');",
        )
        .unwrap();
    assert!(source_wal.exists() && source_shm.exists());

    std::fs::copy(&source, &snapshot).unwrap();
    std::fs::copy(&source_wal, &snapshot_wal).unwrap();
    std::fs::copy(&source_shm, &snapshot_shm).unwrap();

    let snapshot_paths = [&snapshot, &snapshot_wal, &snapshot_shm];
    let original_permissions =
        snapshot_paths.map(|path| std::fs::metadata(path).unwrap().permissions());
    for path in snapshot_paths {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    let main_before = std::fs::read(&snapshot).unwrap();
    let wal_before = std::fs::read(&snapshot_wal).unwrap();
    let shm_before = std::fs::read(&snapshot_shm).unwrap();
    let entries_before = directory_entries(dir.path());

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(snapshot.clone()),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let reader = pool.reader().unwrap();
    let body: String = reader
        .conn()
        .query_row("SELECT body FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(body, "committed-only-in-wal");
    drop(reader);

    let standalone = pool
        .open_standalone_reader(StandaloneReaderPurpose::DiagnosticsIndependentSnapshot)
        .unwrap();
    let count: i64 = standalone
        .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    drop(standalone);
    drop(pool);

    assert_eq!(std::fs::read(&snapshot).unwrap(), main_before);
    assert_eq!(std::fs::read(&snapshot_wal).unwrap(), wal_before);
    assert_eq!(std::fs::read(&snapshot_shm).unwrap(), shm_before);
    assert_eq!(directory_entries(dir.path()), entries_before);

    for (path, permissions) in snapshot_paths.into_iter().zip(original_permissions) {
        std::fs::set_permissions(path, permissions).unwrap();
    }
    drop(source_conn);
}

/// The configured spelling must not decide which WAL sidecars SQLite sees.
/// A symlinked snapshot is classified and opened through one canonical
/// physical path so committed frames beside the target remain visible and
/// no sidecars are ever derived beside the alias.
#[cfg(unix)]
#[test]
fn read_only_frozen_wal_symlink_reads_target_frames_without_mutation() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("wal-source.db");
    let snapshot = dir.path().join("frozen-target.db");
    let alias = dir.path().join("frozen-alias.db");
    let source_wal = sqlite_sidecar(&source, "-wal");
    let source_shm = sqlite_sidecar(&source, "-shm");
    let snapshot_wal = sqlite_sidecar(&snapshot, "-wal");
    let snapshot_shm = sqlite_sidecar(&snapshot, "-shm");
    let alias_wal = sqlite_sidecar(&alias, "-wal");
    let alias_shm = sqlite_sidecar(&alias, "-shm");

    let source_conn = Connection::open(&source).unwrap();
    let mode: String = source_conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    source_conn
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    source_conn
        .execute_batch(
            "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
             INSERT INTO snapshot_row(body) VALUES ('visible-through-target-wal');",
        )
        .unwrap();
    assert!(source_wal.exists() && source_shm.exists());

    std::fs::copy(&source, &snapshot).unwrap();
    std::fs::copy(&source_wal, &snapshot_wal).unwrap();
    std::fs::copy(&source_shm, &snapshot_shm).unwrap();
    symlink(&snapshot, &alias).unwrap();
    assert!(!alias_wal.exists() && !alias_shm.exists());

    let snapshot_paths = [&snapshot, &snapshot_wal, &snapshot_shm];
    let original_permissions =
        snapshot_paths.map(|path| std::fs::metadata(path).unwrap().permissions());
    for path in snapshot_paths {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    let main_before = std::fs::read(&snapshot).unwrap();
    let wal_before = std::fs::read(&snapshot_wal).unwrap();
    let shm_before = std::fs::read(&snapshot_shm).unwrap();
    let entries_before = directory_entries(dir.path());

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(alias.clone()),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let reader = pool.reader().unwrap();
    let body: String = reader
        .conn()
        .query_row("SELECT body FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(body, "visible-through-target-wal");
    drop(reader);
    let standalone = pool
        .open_standalone_reader(StandaloneReaderPurpose::DiagnosticsIndependentSnapshot)
        .unwrap();
    let count: i64 = standalone
        .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    drop(standalone);
    drop(pool);

    assert_eq!(std::fs::read(&snapshot).unwrap(), main_before);
    assert_eq!(std::fs::read(&snapshot_wal).unwrap(), wal_before);
    assert_eq!(std::fs::read(&snapshot_shm).unwrap(), shm_before);
    assert_eq!(directory_entries(dir.path()), entries_before);
    assert!(!alias_wal.exists() && !alias_shm.exists());

    for (path, permissions) in snapshot_paths.into_iter().zip(original_permissions) {
        std::fs::set_permissions(path, permissions).unwrap();
    }
    drop(source_conn);
}

/// A clean persistent-WAL database has no committed frames outside the
/// checkpointed main file. This is the narrow case where an encoded
/// `immutable=1` URI is safe and necessary to prevent SQLite from creating
/// fresh sidecars. Reserved URI bytes in the filesystem path must still
/// resolve to the exact database.
#[test]
fn read_only_clean_wal_snapshot_is_sidecar_free() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clean snapshot ?#%.db");
    {
        let conn = Connection::open(&path).unwrap();
        let mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        conn.execute_batch(
            "CREATE TABLE snapshot_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
             INSERT INTO snapshot_row(body) VALUES ('checkpointed');",
        )
        .unwrap();
    }
    let wal = sqlite_sidecar(&path, "-wal");
    let shm = sqlite_sidecar(&path, "-shm");
    assert!(!wal.exists() && !shm.exists());
    assert!(sqlite_header_uses_wal(&path).unwrap());

    let original_permissions = std::fs::metadata(&path).unwrap().permissions();
    let mut read_only_permissions = original_permissions.clone();
    read_only_permissions.set_readonly(true);
    std::fs::set_permissions(&path, read_only_permissions).unwrap();
    let main_before = std::fs::read(&path).unwrap();
    let entries_before = directory_entries(dir.path());

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let reader = pool.reader().unwrap();
    let body: String = reader
        .conn()
        .query_row("SELECT body FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(body, "checkpointed");
    drop(reader);
    let standalone = pool
        .open_standalone_reader(StandaloneReaderPurpose::DiagnosticsIndependentSnapshot)
        .unwrap();
    let count: i64 = standalone
        .query_row("SELECT COUNT(*) FROM snapshot_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    drop(standalone);
    drop(pool);

    assert_eq!(std::fs::read(&path).unwrap(), main_before);
    assert_eq!(directory_entries(dir.path()), entries_before);
    assert!(!wal.exists() && !shm.exists());
    std::fs::set_permissions(&path, original_permissions).unwrap();
}

/// `immutable=1` is unsafe for a database that can still change and is not
/// needed for rollback-journal reads. Keep ordinary SQLite locking/change
/// detection there so an already-open read-only pool observes a later
/// committed transaction from a live writer.
#[test]
fn read_only_live_rollback_journal_keeps_change_detection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-delete-journal.db");
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("CREATE TABLE live_row(id INTEGER PRIMARY KEY);")
        .unwrap();

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    {
        let reader = pool.reader().unwrap();
        let count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM live_row", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    writer
        .execute("INSERT INTO live_row DEFAULT VALUES", [])
        .unwrap();
    let reader = pool.reader().unwrap();
    let count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM live_row", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "rollback-journal read-only connections must retain live change detection"
    );
}

/// Raw-SQL reads run arbitrary caller SQL against a pooled reader
/// connection. A `CREATE TEMP TABLE` or `ATTACH DATABASE` issued by one
/// checkout must never remain visible to a later, unrelated checkout
/// that happens to draw the same pooled connection back out — that
/// would leak state across logical readers, and across whatever
/// separate checkouts (requests, checkouts of the same store) reuse the
/// pool. `max_readers: 1` forces the second checkout to reuse the exact
/// connection the first one returned.
#[test]
fn pooled_reader_return_clears_temp_schema_and_attached_databases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pooled-reader-reset.db");
    {
        let seed = Connection::open(&path).unwrap();
        seed.execute_batch("CREATE TABLE main_row(id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let secret_path = dir.path().join("secret.db");
    {
        let secret = Connection::open(&secret_path).unwrap();
        secret
            .execute_batch(
                "CREATE TABLE secret_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO secret_row(body) VALUES ('leaked-across-checkouts');",
            )
            .unwrap();
    }

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        max_readers: 1,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();

    {
        let reader = pool.reader().unwrap();
        reader
            .conn()
            .execute_batch("CREATE TEMP TABLE leaked_temp(id INTEGER PRIMARY KEY);")
            .unwrap();
        reader
            .conn()
            .execute_batch(&format!(
                "ATTACH DATABASE '{}' AS secret;",
                secret_path.display()
            ))
            .unwrap();
        let leaked_count: i64 = reader
            .conn()
            .query_row("SELECT COUNT(*) FROM secret.secret_row", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(leaked_count, 1);
        // The pristine-state scan on return only runs for a checkout
        // marked dirty by the raw-SQL bridge (`sql_bridge::run_pool_reader_query`);
        // this test drives the connection directly rather than through
        // `SqlReader`, so it marks the checkout dirty itself to exercise
        // the same scan a real raw-SQL reader checkout would trigger.
        reader.mark_dirty();
    }

    let reader = pool.reader().unwrap();
    let temp_table_survived: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_temp_master WHERE name = 'leaked_temp'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        temp_table_survived, 0,
        "a TEMP table from an earlier checkout must not survive pooled reader reuse"
    );

    let attachment_survived: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM pragma_database_list WHERE name = 'secret'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        attachment_survived, 0,
        "an ATTACHed database from an earlier checkout must not survive pooled reader reuse"
    );
}

/// `PRAGMA writable_schema = ON` lets a caller `DELETE` a TEMP object's
/// own `sqlite_temp_master` row while the object stays live in that
/// connection's in-memory schema — the catalog scan alone
/// (`reader_connection_state_is_pristine`) is blind to this. The
/// settings check (`reader_connection_settings_match_baseline`) must
/// still catch the connection as dirty via `writable_schema` itself and
/// disqualify it for reuse.
#[test]
fn writable_schema_evasion_of_the_temp_catalog_scan_still_disqualifies_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writable-schema-evasion.db");
    {
        let seed = Connection::open(&path).unwrap();
        seed.execute_batch("CREATE TABLE main_row(id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        max_readers: 1,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();

    {
        let reader = pool.reader().unwrap();
        reader
            .conn()
            .execute_batch("CREATE TEMP TABLE leaked_temp(id INTEGER PRIMARY KEY);")
            .unwrap();
        reader
            .conn()
            .execute_batch(
                "PRAGMA writable_schema = ON; \
                 DELETE FROM sqlite_temp_master WHERE name = 'leaked_temp';",
            )
            .unwrap();
        let visible: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_temp_master WHERE name = 'leaked_temp'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            visible, 0,
            "the evasion must actually hide the row from the catalog scan"
        );
        reader.mark_dirty();
    }

    let reader = pool.reader().unwrap();
    // A reused (not replaced) connection would still see `leaked_temp`:
    // SQLite's in-memory schema for a TEMP table survives a
    // `sqlite_temp_master` row delete. A fresh replacement connection
    // has no such table at all.
    let leaked_still_queryable = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM leaked_temp", [], |row| {
            row.get::<_, i64>(0)
        })
        .is_ok();
    assert!(
        !leaked_still_queryable,
        "a writable_schema evasion of the catalog scan must still disqualify the \
         connection via the settings check"
    );
}

/// A checkout that never runs raw SQL through [`ReaderGuard::mark_dirty`]
/// (the shape of every typed store read) must return without the
/// catalog/settings scan running at all — not merely without being
/// disqualified by it. Proven here by leaving state behind on the
/// connection that the scan *would* catch if it ran, then checking that
/// state is still there on the very next checkout: a scan that ran would
/// have detected and cleared it.
#[test]
fn a_checkout_that_never_marks_dirty_skips_the_catalog_scan_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("typed-read-skips-scan.db");
    {
        let seed = Connection::open(&path).unwrap();
        seed.execute_batch("CREATE TABLE main_row(id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        max_readers: 1,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();

    {
        let reader = pool.reader().unwrap();
        // A typed store never calls `run_pool_reader_query`, so it never
        // calls `mark_dirty` either — this checkout is returned clean.
        reader
            .conn()
            .execute_batch("CREATE TEMP TABLE survivor(id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let reader = pool.reader().unwrap();
    let survived = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM survivor", [], |row| {
            row.get::<_, i64>(0)
        })
        .is_ok();
    assert!(
        survived,
        "a non-dirty checkout must return without running the catalog scan at all, \
         so a TEMP table it left behind is still visible on the next checkout"
    );
}

/// A `busy_timeout`/`cache_size` change made directly on a dirty
/// checkout's connection must not be observed by the next checkout —
/// these are exactly the two settings the reader-capability admission
/// gate (`sql_bridge::reader_capability_admits`) refuses to let a raw
/// PRAGMA touch; this test pins the independent settings-check safety
/// net for the case where something still changed them.
#[test]
fn busy_timeout_and_cache_size_changes_do_not_survive_a_dirty_pooled_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings-evasion.db");
    {
        let seed = Connection::open(&path).unwrap();
        seed.execute_batch("CREATE TABLE main_row(id INTEGER PRIMARY KEY);")
            .unwrap();
    }

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        max_readers: 1,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();
    let default_busy_timeout_ms = i64::try_from(pool.config().busy_timeout.as_millis())
        .expect("configured busy_timeout fits i64 millis");

    {
        let reader = pool.reader().unwrap();
        reader
            .conn()
            .pragma_update(None, "busy_timeout", 1i64)
            .unwrap();
        reader
            .conn()
            .pragma_update(None, "cache_size", -64i64)
            .unwrap();
        reader
            .conn()
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .unwrap();
        reader.mark_dirty();
    }

    let reader = pool.reader().unwrap();
    let busy_timeout: i64 = reader
        .conn()
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    let cache_size: i64 = reader
        .conn()
        .query_row("PRAGMA cache_size", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        busy_timeout, default_busy_timeout_ms,
        "busy_timeout must be restored to the pool's configured baseline"
    );
    assert_eq!(
        cache_size, -65536,
        "cache_size must be restored to the pool's configured baseline"
    );
}

/// `max_readers == 0` (degraded mode) has no separate reader connection
/// to close and replace — the shared writer-as-reader lease is the only
/// connection. A dirty return must still be cleaned (or the pool
/// poisoned) rather than handing the next checkout leaked TEMP/attached
/// state, exactly as the pooled path does.
#[test]
fn degraded_shared_reader_lease_clears_temp_schema_and_attached_databases_on_dirty_return() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("degraded-shared-reader-reset.db");
    {
        let seed = Connection::open(&path).unwrap();
        seed.execute_batch("CREATE TABLE main_row(id INTEGER PRIMARY KEY);")
            .unwrap();
    }
    let secret_path = dir.path().join("secret.db");
    {
        let secret = Connection::open(&secret_path).unwrap();
        secret
            .execute_batch(
                "CREATE TABLE secret_row(id INTEGER PRIMARY KEY, body TEXT NOT NULL);\
                 INSERT INTO secret_row(body) VALUES ('leaked-across-checkouts');",
            )
            .unwrap();
    }

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        max_readers: 0,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();

    {
        let reader = pool.reader().unwrap();
        reader
            .conn()
            .execute_batch("CREATE TEMP TABLE leaked_temp(id INTEGER PRIMARY KEY);")
            .unwrap();
        reader
            .conn()
            .execute_batch(&format!(
                "ATTACH DATABASE '{}' AS secret;",
                secret_path.display()
            ))
            .unwrap();
        reader.mark_dirty();
    }

    let reader = pool.reader().unwrap();
    let temp_table_survived: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_temp_master WHERE name = 'leaked_temp'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        temp_table_survived, 0,
        "a TEMP table must not survive a dirty degraded shared-reader-lease return"
    );
    let attachment_survived: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM pragma_database_list WHERE name = 'secret'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        attachment_survived, 0,
        "an ATTACHed database must not survive a dirty degraded shared-reader-lease return"
    );
}

/// `ReaderGuard::conn` is crate-private and no method on `ReaderGuard`
/// ever returns a raw `&Connection` to a caller outside `khive-db` — the
/// only public accessor, `query_row`, must refuse anything that is not
/// an admitted read shape before it reaches SQLite at all, not merely
/// mark the checkout dirty after the fact. Before that encapsulation, a
/// caller with a raw connection could open a transaction, run DML, or
/// flip connection-local state on a lease meant to be read-only; a
/// `BEGIN`, an `INSERT`, and a setting `PRAGMA` must each be refused
/// here, and the probe rows/state they would have written must not
/// exist afterward.
#[test]
fn query_row_refuses_write_and_transaction_control_statements() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        ..PoolConfig::default()
    })
    .unwrap();
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE query_row_admission_probe(id INTEGER PRIMARY KEY);")
        .unwrap();

    let reader = pool.reader().unwrap();
    for (label, sql) in [
        ("BEGIN", "BEGIN"),
        (
            "INSERT",
            "INSERT INTO query_row_admission_probe(id) VALUES (1)",
        ),
        (
            "CREATE TEMP TABLE",
            "CREATE TEMP TABLE query_row_admission_probe_temp(id INTEGER PRIMARY KEY)",
        ),
        ("setting PRAGMA", "PRAGMA journal_mode = OFF"),
    ] {
        let result = reader.query_row(sql, [], |row| row.get::<_, i64>(0));
        assert!(
            result.is_err(),
            "query_row must refuse {label} ({sql:?}); got {result:?}"
        );
    }

    let row_count: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM query_row_admission_probe",
            [],
            |row| row.get(0),
        )
        .expect("an admitted SELECT must still succeed");
    assert_eq!(
        row_count, 0,
        "a refused INSERT must never have reached SQLite"
    );
    let temp_table_survived: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM sqlite_temp_master \
             WHERE name = 'query_row_admission_probe_temp'",
            [],
            |row| row.get(0),
        )
        .expect("an admitted SELECT must still succeed");
    assert_eq!(
        temp_table_survived, 0,
        "a refused CREATE TEMP TABLE must never have reached SQLite"
    );
    let journal_mode: String = reader
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("the read-only journal_mode PRAGMA form must still be admitted");
    assert_ne!(
        journal_mode.to_ascii_lowercase(),
        "off",
        "a refused setting PRAGMA must never have reached SQLite"
    );
}

/// A non-reusable checkout (marked via `discard()`, the cancellation
/// cleanup path in `read_cancellation.rs`) closes its connection and
/// tries to open a replacement to refill the physical pool slot. Before
/// this shared the same accounting `return_reader`'s disqualified-return
/// path uses, a failed replacement open here was silently swallowed: no
/// counter, no log, so `db_diagnostics` under-reported the lost
/// capacity.
#[test]
fn discarded_reader_replacement_open_failure_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("discard-replacement-failure.db");
    {
        let seed = Connection::open(&path).unwrap();
        seed.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
            .unwrap();
    }
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        max_readers: 1,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();

    let before = pool.reader_acquisition_snapshot();

    let reader = pool.reader().unwrap();
    reader.discard();
    // The replacement open this triggers on drop must fail deterministically:
    // remove the file a fresh SQLITE_OPEN_READ_ONLY open needs.
    std::fs::remove_file(&path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(sqlite_sidecar(&path, suffix));
    }
    drop(reader);

    let after = pool.reader_acquisition_snapshot();
    assert_eq!(
        after.reader_replacement_open_failures - before.reader_replacement_open_failures,
        1,
        "a non-reusable checkout's failed replacement open must be recorded, not silently \
         swallowed"
    );
}

/// A writable `-shm` beside a WAL database is evidence that the database is
/// not a sidecar-free frozen snapshot (and may have a live writer). Refuse
/// before opening SQLite rather than mutate the shared index or unsafely
/// assert `immutable=1` over a live database.
#[test]
fn read_only_live_wal_with_writable_shm_is_refused_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live-wal.db");
    let wal = sqlite_sidecar(&path, "-wal");
    let shm = sqlite_sidecar(&path, "-shm");
    let writer = Connection::open(&path).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer
        .execute_batch(
            "CREATE TABLE live_row(id INTEGER PRIMARY KEY);\
             INSERT INTO live_row DEFAULT VALUES;",
        )
        .unwrap();
    assert!(wal.exists() && shm.exists());

    let main_before = std::fs::read(&path).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    let shm_before = std::fs::read(&shm).unwrap();
    let error = match ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("a live WAL database with writable -shm must fail closed"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("writable WAL shared-memory sidecar"),
        "diagnostic must explain how to freeze the snapshot: {error}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), main_before);
    assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
    assert_eq!(std::fs::read(&shm).unwrap(), shm_before);

    drop(writer);
}

/// A pool's own reader connections must never be the last of its
/// connections to close. Readers are opened eagerly at construction
/// (before this test's write), so with `max_readers: 1` and no writer
/// task the pool holds exactly two connections on this database: the
/// writable `writer` and one read-only reader. Struct field order alone
/// then decides which one closes last, deterministically, with no
/// scheduling involved: `writer` was declared before `readers`, so
/// without draining `readers` first a plain pool drop leaves the
/// read-only reader as the last closer, which cannot take the EXCLUSIVE
/// lock SQLite needs to checkpoint (#3089).
#[test]
fn pool_drop_never_leaves_a_reader_as_the_last_connection_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("close-order.db");
    let wal = sqlite_sidecar(&path, "-wal");
    let shm = sqlite_sidecar(&path, "-shm");

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        max_readers: 1,
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .unwrap();

    pool.writer()
        .unwrap()
        .execute_batch(
            "CREATE TABLE close_order_row(id INTEGER PRIMARY KEY);\
             INSERT INTO close_order_row DEFAULT VALUES;",
        )
        .unwrap();
    assert!(
        wal.exists(),
        "a WAL-mode write must leave a -wal sidecar before the pool drops"
    );

    drop(pool);

    assert!(
        !wal.exists(),
        "the pool's last connection to close must be writable enough to checkpoint -wal away"
    );
    assert!(
        !shm.exists(),
        "the pool's last connection to close must be writable enough to checkpoint -shm away"
    );
}

/// A symlink cannot hide a writable target `-shm`. Admission inspects the
/// canonical target sidecar set before SQLite opens any connection and
/// therefore refuses a potentially live WAL without touching either
/// target or alias-adjacent paths.
#[cfg(unix)]
#[test]
fn read_only_live_wal_symlink_rejects_target_writable_shm_without_mutation() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("live-target.db");
    let alias = dir.path().join("live-alias.db");
    let target_wal = sqlite_sidecar(&target, "-wal");
    let target_shm = sqlite_sidecar(&target, "-shm");
    let alias_wal = sqlite_sidecar(&alias, "-wal");
    let alias_shm = sqlite_sidecar(&alias, "-shm");

    let writer = Connection::open(&target).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer
        .execute_batch(
            "CREATE TABLE live_row(id INTEGER PRIMARY KEY);\
             INSERT INTO live_row DEFAULT VALUES;",
        )
        .unwrap();
    assert!(target_wal.exists() && target_shm.exists());
    symlink(&target, &alias).unwrap();
    assert!(!alias_wal.exists() && !alias_shm.exists());

    let main_before = std::fs::read(&target).unwrap();
    let wal_before = std::fs::read(&target_wal).unwrap();
    let shm_before = std::fs::read(&target_shm).unwrap();
    let entries_before = directory_entries(dir.path());

    let error = match ConnectionPool::new(PoolConfig {
        path: Some(alias),
        read_only: true,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("a symlink must not hide the target's writable -shm"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("writable WAL shared-memory sidecar"),
        "diagnostic must identify the canonical target's live sidecar: {error}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), main_before);
    assert_eq!(std::fs::read(&target_wal).unwrap(), wal_before);
    assert_eq!(std::fs::read(&target_shm).unwrap(), shm_before);
    assert_eq!(directory_entries(dir.path()), entries_before);
    assert!(!alias_wal.exists() && !alias_shm.exists());

    drop(writer);
}

#[test]
#[serial]
fn pool_config_default_values_match_constants() {
    // Ensure defaults are not accidentally changed. The process env may
    // legitimately carry overrides — a sibling test in this process sets
    // KHIVE_CHECKOUT_TIMEOUT_SECS around its own body — so clear them
    // first: this test asserts the constants, not the env.
    let _pool_env = clear_pool_env();
    let cfg = PoolConfig::default();
    assert_eq!(
        cfg.journal_size_limit_bytes,
        DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
    );
    assert_eq!(cfg.busy_timeout, Duration::from_secs(30));
    assert_eq!(cfg.checkout_timeout, Duration::from_secs(5));
}

#[test]
#[serial]
fn legacy_env_cannot_change_wal_autocheckpoint() {
    let _pool_env = clear_pool_env();
    std::env::set_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "8000");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_autocheckpoint_env.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    {
        let writer = pool.writer().expect("writer");
        assert_eq!(
            wal_autocheckpoint_pages(writer.conn()),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            "the removed env override must not change the unclaimed fallback"
        );
    }
    pool.claim_checkpoint_ownership().expect("claim ownership");
    let writer = pool.writer().expect("writer after claim");
    assert_eq!(
        wal_autocheckpoint_pages(writer.conn()),
        0,
        "the removed env override must not change the claimed-owner setting"
    );
    std::env::remove_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES");
}

#[test]
#[serial]
fn pool_config_env_override_journal_size_limit() {
    std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "134217728");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
    assert_eq!(cfg.journal_size_limit_bytes, 134_217_728);
}

#[test]
#[serial]
fn pool_config_env_override_busy_timeout() {
    std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", "60");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_BUSY_TIMEOUT_SECS");
    assert_eq!(cfg.busy_timeout, Duration::from_secs(60));
}

#[test]
#[serial]
fn pool_config_env_override_checkout_timeout() {
    std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", "10");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS");
    assert_eq!(cfg.checkout_timeout, Duration::from_secs(10));
}

#[test]
#[serial]
fn pool_config_write_queue_defaults_unset() {
    let _pool_env = clear_pool_env();
    let cfg = PoolConfig::default();
    assert_eq!(cfg.write_queue_enabled, None);
    assert_eq!(cfg.write_queue_capacity, DEFAULT_WRITE_QUEUE_CAPACITY);
}

#[test]
#[serial]
fn clear_pool_env_restores_overrides_on_drop() {
    let _ambient_env = PoolEnvGuard::capture();
    std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", "73");

    {
        let _pool_env = clear_pool_env();
        assert_eq!(std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS"), None);
    }

    assert_eq!(
        std::env::var_os("KHIVE_BUSY_TIMEOUT_SECS"),
        Some(std::ffi::OsString::from("73"))
    );
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_enabled() {
    std::env::set_var("KHIVE_WRITE_QUEUE", "1");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(true));
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_enabled_accepts_true_case_insensitive() {
    std::env::set_var("KHIVE_WRITE_QUEUE", "True");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(true));
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_enabled_accepts_zero_as_explicit_off() {
    std::env::set_var("KHIVE_WRITE_QUEUE", "0");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(false));
}

/// A SET-but-non-Unicode `KHIVE_WRITE_QUEUE` value (invalid UTF-8 on
/// unix) must count as SET — `Some(false)` ("any SET value other than
/// 1/true means off"), never a fall-through to the file-backed default.
/// That is why `PoolConfig::default()` reads `var_os`, not `var`.
#[cfg(unix)]
#[test]
#[serial]
fn pool_config_env_override_write_queue_non_unicode_value_is_explicit_off() {
    use std::os::unix::ffi::OsStrExt;
    let _pool_env = clear_pool_env();
    std::env::set_var(
        "KHIVE_WRITE_QUEUE",
        std::ffi::OsStr::from_bytes(b"\xff\xfe"),
    );
    let cfg = PoolConfig::default();
    assert_eq!(cfg.write_queue_enabled, Some(false));
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_invalid_value_is_explicit_off() {
    // Documented contract (`write_queue_enabled` docs): `"1"`/`"true"`
    // (case-insensitive) set `Some(true)`; any other value — garbage
    // included — sets `Some(false)`, never `None`.
    std::env::set_var("KHIVE_WRITE_QUEUE", "banana");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_QUEUE");
    assert_eq!(cfg.write_queue_enabled, Some(false));
}

#[test]
#[serial]
fn pool_config_write_routing_strict_defaults_off() {
    let _pool_env = clear_pool_env();
    let cfg = PoolConfig::default();
    assert!(!cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_override_write_routing_strict() {
    std::env::set_var("KHIVE_WRITE_ROUTING", "strict");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_ROUTING");
    assert!(cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_override_write_routing_strict_case_insensitive() {
    std::env::set_var("KHIVE_WRITE_ROUTING", "STRICT");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_ROUTING");
    assert!(cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_write_routing_ignores_unrecognized_value() {
    std::env::set_var("KHIVE_WRITE_ROUTING", "eventual");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_ROUTING");
    assert!(!cfg.write_routing_strict);
}

#[test]
#[serial]
fn pool_config_env_override_write_queue_capacity() {
    std::env::set_var("KHIVE_WRITE_QUEUE_CAPACITY", "64");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_QUEUE_CAPACITY");
    assert_eq!(cfg.write_queue_capacity, 64);
}

#[test]
#[serial]
fn pool_config_env_invalid_write_queue_capacity_falls_back_to_default() {
    std::env::set_var("KHIVE_WRITE_QUEUE_CAPACITY", "0");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_WRITE_QUEUE_CAPACITY");
    assert_eq!(cfg.write_queue_capacity, DEFAULT_WRITE_QUEUE_CAPACITY);
}

#[test]
#[serial]
fn pool_config_invalid_journal_size_limit_falls_back_to_default() {
    std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "");
    let cfg = PoolConfig::default();
    std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
    assert_eq!(
        cfg.journal_size_limit_bytes,
        DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
    );
}

#[test]
fn file_backed_pool_opens_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_pool.db");
    let cfg = PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");
    assert!(path.exists());
    assert!(pool.max_readers() > 0);
}

#[cfg(windows)]
#[test]
fn windows_read_only_legacy_reader_rejects_different_opened_file_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    let replacement = dir.path().join("replacement.db");
    for target in [&path, &replacement] {
        let conn = Connection::open(target).unwrap();
        conn.execute_batch("CREATE TABLE marker (value INTEGER)")
            .unwrap();
    }
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        read_only: true,
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("open a legacy read-only pool without a stored database UUID");
    assert_eq!(pool.opened_database_id, None);
    assert_eq!(
        pool.opened_file_identity,
        Some(database_file_identity(&path).unwrap())
    );
    pool.open_reader_connection()
        .expect("a later reader of the pinned file succeeds");

    // SQLite's Windows VFS omits FILE_SHARE_DELETE, so a live pool bars
    // pathname replacement. Exercise the same post-open gate with an
    // actual connection to another file while the legacy pool stays live.
    let other =
        Connection::open_with_flags(&replacement, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let error = pool
        .verify_connection_file_identity(&other, &path)
        .expect_err("the opened handle differs from the requested path");
    assert!(error.to_string().contains("identity changed"), "{error}");
    let error = pool
        .verify_connection_file_identity(&other, &replacement)
        .expect_err("a different opened main handle cannot join this pool");
    assert!(error.to_string().contains("identity changed"), "{error}");
}

#[cfg(unix)]
#[test]
fn standalone_and_new_reader_keep_the_first_opened_target_after_symlink_retarget() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.db");
    let second = dir.path().join("second.db");
    for (path, marker) in [(&first, 11), (&second, 22)] {
        let conn = Connection::open(path).unwrap();
        conn.execute("CREATE TABLE identity_marker (value INTEGER NOT NULL)", [])
            .unwrap();
        conn.execute("INSERT INTO identity_marker (value) VALUES (?1)", [marker])
            .unwrap();
    }
    let alias = dir.path().join("current.db");
    symlink(&first, &alias).unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(alias.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("open the first database through its alias");

    fs::remove_file(&alias).unwrap();
    symlink(&second, &alias).unwrap();

    let standalone = pool
        .open_standalone_writer_untracked()
        .expect("standalone probe stays on the opened database");
    let value: i64 = standalone
        .query_row("SELECT value FROM identity_marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 11);

    let reader = pool
        .open_reader_connection()
        .expect("a newly opened reader stays on the same database");
    let value: i64 = reader
        .query_row("SELECT value FROM identity_marker", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 11);
}

#[cfg(unix)]
#[test]
fn standalone_writer_refuses_replaced_pinned_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("current.db");
    let replacement = dir.path().join("replacement.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("open the first database");
    let replacement_conn = Connection::open(&replacement).unwrap();
    replacement_conn
        .execute_batch("CREATE TABLE replacement_marker (value INTEGER)")
        .unwrap();
    drop(replacement_conn);
    fs::rename(&replacement, &path).unwrap();

    let error = pool
        .open_standalone_writer_untracked()
        .expect_err("a replaced file must not receive a standalone probe");
    assert!(
        error.to_string().contains("file identity changed"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn standalone_reader_refuses_replaced_pinned_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("current.db");
    let replacement = dir.path().join("replacement.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("open the first database");
    let replacement_conn = Connection::open(&replacement).unwrap();
    replacement_conn
        .execute_batch("CREATE TABLE replacement_marker (value INTEGER)")
        .unwrap();
    drop(replacement_conn);
    fs::rename(&replacement, &path).unwrap();

    let error = pool
        .open_standalone_reader(StandaloneReaderPurpose::ExplicitSqlReadTransaction)
        .expect_err("a replaced file must not serve a standalone read");
    assert!(
        error.to_string().contains("file identity changed"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn existing_path_replacement_after_sqlite_open_before_first_stat_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.db");
    let replacement = dir.path().join("replacement.db");
    let original = Connection::open(&path).unwrap();
    original
        .execute_batch("CREATE TABLE original_marker (id INTEGER)")
        .unwrap();
    drop(original);
    let other = Connection::open(&replacement).unwrap();
    other
        .execute_batch("CREATE TABLE replacement_marker (id INTEGER)")
        .unwrap();
    drop(other);
    let pinned_path = mint_db_identity(&path).unwrap().1;
    let original_identity = database_file_identity(&path).unwrap();
    let replacement_identity = database_file_identity(&replacement).unwrap();
    assert_ne!(original_identity, replacement_identity);

    let swapped = std::rc::Rc::new(Cell::new(false));
    let _hook = install_identity_open_hook({
        let path = path.clone();
        let swapped = std::rc::Rc::clone(&swapped);
        move |target, stage, conn| {
            if target != pinned_path.as_path()
                || stage != IdentityOpenStage::AfterMainOpenBeforeFirstStat
            {
                return;
            }
            let opened = conn.expect("SQLite main must already be open");
            let marker: i64 = opened
                .query_row(
                    "SELECT count(*) FROM main.sqlite_master WHERE name='original_marker'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(marker, 1, "the opened handle belongs to the original file");
            let staged = path.with_extension("staged.db");
            fs::hard_link(&replacement, &staged).unwrap();
            fs::rename(&staged, &path).unwrap();
            swapped.set(true);
        }
    });
    let error = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        read_only: true,
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .err()
    .expect("changed path must fail before pool admission");
    assert!(
        swapped.get(),
        "swap must occur in the open-to-first-stat window"
    );
    assert_eq!(database_file_identity(&path).unwrap(), replacement_identity);
    assert!(error.to_string().contains("identity changed"), "{error}");
}

#[cfg(unix)]
#[test]
fn initially_absent_path_replacement_cannot_pin_a_different_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.db");
    let pinned_path = mint_db_identity(&path).unwrap().1;
    let parked = dir.path().join("opened-writer.db");
    let replacement = dir.path().join("replacement.db");
    let replacement_conn = Connection::open(&replacement).unwrap();
    replacement_conn
        .execute_batch("CREATE TABLE replacement_marker (value INTEGER)")
        .unwrap();
    drop(replacement_conn);
    assert!(!path.exists(), "exercise the absent first-open path");

    let replacement_ran = std::rc::Rc::new(Cell::new(false));
    let _hook = install_identity_open_hook({
        let path = path.clone();
        let replacement_ran = std::rc::Rc::clone(&replacement_ran);
        move |target, stage, conn| {
            if target != pinned_path.as_path()
                || stage != IdentityOpenStage::AfterInitialIdentityWrite
            {
                return;
            }
            assert!(
                read_database_id(conn.expect("opened writer"))
                    .unwrap()
                    .is_some(),
                "the first writer's identity must be committed before replacement"
            );
            rename_pair_on_other_thread(&path, &parked, &replacement, &path);
            replacement_ran.set(true);
        }
    });
    let error = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .err()
    .expect("an absent path must not pin a replacement after SQLite opened its writer");
    assert!(replacement_ran.get(), "the replacement hook must execute");
    assert!(error.to_string().contains("identity changed"), "{error}");
    assert!(path.exists());
}

#[cfg(unix)]
#[test]
fn transient_swap_cannot_return_a_standalone_connection_to_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("current.db");
    let pinned_path = mint_db_identity(&path).unwrap().1;
    let parked = dir.path().join("parked.db");
    let replacement = dir.path().join("replacement.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.writer()
        .unwrap()
        .execute_batch(
            "CREATE TABLE identity_marker (value INTEGER); \
                        INSERT INTO identity_marker (value) VALUES (11)",
        )
        .unwrap();
    let mut replacement_conn = Connection::open(&replacement).unwrap();
    replacement_conn
        .execute_batch(
            "CREATE TABLE identity_marker (value INTEGER); \
                        INSERT INTO identity_marker (value) VALUES (22)",
        )
        .unwrap();
    let replacement_id = initialize_database_id(&mut replacement_conn).unwrap();
    assert_ne!(pool.opened_database_id, Some(replacement_id));
    drop(replacement_conn);

    let swap_ran = std::rc::Rc::new(Cell::new(false));
    let restore_ran = std::rc::Rc::new(Cell::new(false));
    let _hook = install_identity_open_hook({
        let path = path.clone();
        let swap_ran = std::rc::Rc::clone(&swap_ran);
        let restore_ran = std::rc::Rc::clone(&restore_ran);
        move |target, stage, conn| {
            if target != pinned_path.as_path() {
                return;
            }
            match stage {
                IdentityOpenStage::AfterMainOpenBeforeFirstStat => {}
                IdentityOpenStage::BeforeStandaloneOpen => {
                    rename_pair_on_other_thread(&path, &parked, &replacement, &path);
                    swap_ran.set(true);
                }
                IdentityOpenStage::AfterStandaloneOpen => {
                    let opened = conn.expect("SQLite opened the swapped path");
                    let marker: i64 = opened
                        .query_row("SELECT value FROM identity_marker", [], |row| row.get(0))
                        .unwrap();
                    assert_eq!(marker, 22, "the opened handle belongs to the replacement");
                    rename_pair_on_other_thread(&path, &replacement, &parked, &path);
                    restore_ran.set(true);
                }
                IdentityOpenStage::AfterInitialIdentityWrite => {}
            }
        }
    });
    let error = pool
        .open_standalone_writer_untracked()
        .expect_err("both pathname stats see the original, but SQLite opened replacement");
    assert!(swap_ran.get(), "the swap hook must execute");
    assert!(restore_ran.get(), "the restore hook must execute");
    assert!(error.to_string().contains("identity changed"), "{error}");
    assert_eq!(
        database_file_identity(&path).unwrap(),
        pool.opened_file_identity.unwrap()
    );
}

#[test]
fn standalone_wal_writer_uses_configured_journal_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone_wal_journal_limit.db");
    let configured_limit = 12_345_678;
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        journal_size_limit_bytes: configured_limit,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("WAL pool open");

    let standalone = pool
        .open_standalone_writer_untracked()
        .expect("standalone WAL writer open");
    assert_eq!(current_journal_mode(&standalone).unwrap(), "wal");
    assert_eq!(journal_size_limit_bytes(&standalone), configured_limit);
}

#[test]
fn standalone_rollback_writer_keeps_sqlite_journal_size_limit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone_rollback_journal_limit.db");
    let sqlite_default = {
        let conn = Connection::open(&path).expect("seed rollback-journal database");
        assert_eq!(current_journal_mode(&conn).unwrap(), "delete");
        journal_size_limit_bytes(&conn)
    };
    let configured_limit = if sqlite_default == 12_345_678 {
        23_456_789
    } else {
        12_345_678
    };
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        wal_mode: false,
        journal_size_limit_bytes: configured_limit,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("rollback-journal pool open");

    let standalone = pool
        .open_standalone_writer_untracked()
        .expect("standalone rollback-journal writer open");
    assert_eq!(current_journal_mode(&standalone).unwrap(), "delete");
    assert_eq!(journal_size_limit_bytes(&standalone), sqlite_default);
}

#[test]
fn every_writer_capable_connection_maintains_rfc3339_expression_indexes() {
    const INSERT: &str = "INSERT INTO deadlines(id, due) VALUES (?1, ?2)";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rfc3339_expression_index.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool open");
    {
        let writer = pool.writer().expect("pooled writer");
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE deadlines(id INTEGER PRIMARY KEY, due TEXT);
                 CREATE INDEX idx_deadlines_strict \
                     ON deadlines(ifnull(khive_rfc3339_strict_key(due), x''));
                 CREATE INDEX idx_deadlines_relaxed \
                     ON deadlines(khive_rfc3339_key(due));",
            )
            .expect("pooled writer registers both key functions");
    }

    // Control: a connection that did not go through the pool's
    // initialization cannot maintain these indexes. This is the
    // failure every unregistered writer used to hit.
    let bare = Connection::open(&path).expect("bare connection");
    let refused = bare
        .execute(INSERT, rusqlite::params![1, "2026-01-01T00:00:00Z"])
        .expect_err("an unregistered connection cannot maintain the index");
    assert!(
        refused.to_string().contains("unknown function"),
        "unexpected refusal: {refused}"
    );
    drop(bare);

    let tracked = pool.open_standalone_writer().expect("tracked standalone");
    let untracked = pool
        .open_standalone_writer_untracked()
        .expect("untracked standalone");
    for (id, conn) in [(2, &tracked), (3, &untracked)] {
        conn.execute(INSERT, rusqlite::params![id, "2026-01-01T00:00:00Z"])
            .unwrap_or_else(|error| panic!("standalone writer {id}: {error}"));
    }
    {
        let writer = pool.writer().expect("pooled writer");
        writer
            .conn()
            .execute(INSERT, rusqlite::params![4, "2026-01-01T00:00:00Z"])
            .expect("pooled writer");
    }
    let reader = pool
        .open_standalone_reader(StandaloneReaderPurpose::DiagnosticsIndependentSnapshot)
        .expect("standalone reader");
    let indexed: i64 = reader
        .query_row(
            "SELECT COUNT(*) FROM deadlines \
             WHERE ifnull(khive_rfc3339_strict_key(due), x'') <= khive_rfc3339_strict_key(?1)",
            ["2026-06-01T00:00:00Z"],
            |row| row.get(0),
        )
        .expect("standalone reader registers the key functions");
    assert_eq!(indexed, 3);
}

#[test]
fn writer_connections_follow_checkpoint_ownership_claim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_autocheckpoint.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("pool open");

    // Unclaimed: every writer-capable connection keeps the bounded
    // fallback, so a pool without a checkpoint task retains SQLite's own
    // WAL reclamation.
    {
        let writer = pool.writer().expect("pooled writer");
        assert_eq!(
            wal_autocheckpoint_pages(writer.conn()),
            FALLBACK_WAL_AUTOCHECKPOINT_PAGES
        );
    }
    let standalone = pool
        .open_standalone_writer()
        .expect("standalone writer opened before any claim");
    assert_eq!(
        wal_autocheckpoint_pages(&standalone),
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES
    );
    drop(standalone);

    // Claimed: the already-open pooled writer is re-configured under the
    // writer mutex, and every later writer-capable open disables the
    // autocheckpoint entirely.
    pool.claim_checkpoint_ownership().expect("claim ownership");
    {
        let writer = pool.writer().expect("pooled writer after claim");
        assert_eq!(wal_autocheckpoint_pages(writer.conn()), 0);
    }
    let claimed_standalone = pool
        .open_standalone_writer()
        .expect("standalone writer opened after the claim");
    assert_eq!(wal_autocheckpoint_pages(&claimed_standalone), 0);
    drop(claimed_standalone);

    let later_infrastructure = pool
        .open_standalone_writer_untracked()
        .expect("later infrastructure writer");
    assert_eq!(wal_autocheckpoint_pages(&later_infrastructure), 0);

    let memory_pool = ConnectionPool::new(PoolConfig {
        write_queue_enabled: Some(false),
        ..PoolConfig::default()
    })
    .expect("in-memory pool open");
    let memory_writer = memory_pool.writer().expect("in-memory writer");
    assert_eq!(
        wal_autocheckpoint_pages(memory_writer.conn()),
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
        "an unclaimed in-memory pool keeps the bounded fallback"
    );
}

#[test]
fn standalone_writer_waits_for_checkpoint_claim_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint_claim_race.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            checkout_timeout: Duration::from_secs(5),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .expect("pool open"),
    );

    let legacy_conn = pool.legacy_conn();
    let held_writer = legacy_conn.lock();
    let claim_start = Arc::new(std::sync::Barrier::new(2));
    let claim_pool = Arc::clone(&pool);
    let claim_thread_start = Arc::clone(&claim_start);
    let claim_thread = thread::spawn(move || {
        claim_thread_start.wait();
        claim_pool.claim_checkpoint_ownership()
    });
    claim_start.wait();

    {
        let mut state = pool.checkpoint_ownership.state.lock();
        while state.phase != CheckpointOwnership::Claiming {
            pool.checkpoint_ownership.changed.wait(&mut state);
        }
    }

    let open_start = Arc::new(std::sync::Barrier::new(2));
    let open_pool = Arc::clone(&pool);
    let open_thread_start = Arc::clone(&open_start);
    let open_thread = thread::spawn(move || {
        open_thread_start.wait();
        let conn = open_pool
            .open_standalone_writer()
            .expect("standalone writer after claim resolution");
        wal_autocheckpoint_pages(&conn)
    });
    open_start.wait();

    {
        let mut state = pool.checkpoint_ownership.state.lock();
        while state.connection_waiters == 0 {
            pool.checkpoint_ownership.changed.wait(&mut state);
        }
        assert_eq!(state.phase, CheckpointOwnership::Claiming);
    }

    drop(held_writer);
    claim_thread
        .join()
        .expect("claim thread joins")
        .expect("claim succeeds");
    assert_eq!(
        open_thread.join().expect("standalone-open thread joins"),
        0,
        "a writer open concurrent with a successful claim must inherit claimed ownership"
    );
}

#[test]
fn standalone_fallback_application_linearizes_before_claim_publication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint_open_before_claim.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            checkout_timeout: Duration::from_secs(5),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .expect("pool open"),
    );
    let pause = Arc::new(CheckpointConnectionConfigPause::new());
    *pool.checkpoint_ownership.connection_config_pause.lock() = Some(Arc::clone(&pause));

    let open_pool = Arc::clone(&pool);
    let open_thread = thread::spawn(move || {
        let conn = open_pool
            .open_standalone_writer()
            .expect("standalone writer opens");
        wal_autocheckpoint_pages(&conn)
    });
    pause.selected.wait();
    assert!(
        pool.checkpoint_ownership.state.try_lock().is_none(),
        "standalone selection must retain the ownership gate until its PRAGMA is applied"
    );

    let (claim_observed_tx, claim_observed_rx) = std::sync::mpsc::sync_channel(0);
    *pool.checkpoint_ownership.claim_lock_observed.lock() = Some(claim_observed_tx);
    let claim_pool = Arc::clone(&pool);
    let claim_thread = thread::spawn(move || claim_pool.claim_checkpoint_ownership());
    assert!(
        claim_observed_rx
            .recv()
            .expect("claim reports whether it observed gate contention"),
        "the claim must attempt the gate between fallback selection and PRAGMA application"
    );
    pause.resume.wait();

    assert_eq!(
        open_thread.join().expect("standalone-open thread joins"),
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
        "an open linearized before the claim keeps the fallback"
    );
    claim_thread
        .join()
        .expect("claim thread joins")
        .expect("claim succeeds after standalone configuration");
    assert_eq!(pool.effective_wal_autocheckpoint_pages(), 0);
}

#[test]
fn failed_checkpoint_ownership_claim_keeps_fallback_and_can_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint_claim_retry.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        checkout_timeout: Duration::from_millis(1),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("pool open");

    let legacy_conn = pool.legacy_conn();
    let held_writer = legacy_conn.lock();
    let error = pool
        .claim_checkpoint_ownership()
        .expect_err("the held pooled writer must make the claim time out");
    assert!(matches!(
        error,
        SqliteError::WriterPoolCheckoutTimeout { .. }
    ));
    assert_eq!(
        pool.effective_wal_autocheckpoint_pages(),
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
        "a failed claim must leave later writer connections fallback-safe"
    );

    let fallback_writer = pool
        .open_standalone_writer()
        .expect("standalone writer after failed claim");
    assert_eq!(
        wal_autocheckpoint_pages(&fallback_writer),
        FALLBACK_WAL_AUTOCHECKPOINT_PAGES
    );
    drop(fallback_writer);

    drop(held_writer);
    pool.claim_checkpoint_ownership()
        .expect("the ownership claim remains retryable");
    assert_eq!(pool.effective_wal_autocheckpoint_pages(), 0);
    let writer = pool.writer().expect("pooled writer after successful retry");
    assert_eq!(wal_autocheckpoint_pages(writer.conn()), 0);
}

#[test]
fn threshold_crossing_commits_do_not_run_an_implicit_checkpoint_once_claimed() {
    const FORMER_AUTOCHECKPOINT_THRESHOLD_PAGES: i64 = FALLBACK_WAL_AUTOCHECKPOINT_PAGES as i64;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("no_implicit_checkpoint.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    pool.claim_checkpoint_ownership()
        .expect("claim ownership for the dedicated-owner posture");
    let writer = pool.writer().expect("pooled writer");
    writer
        .execute_batch("CREATE TABLE blobs (value BLOB NOT NULL)")
        .expect("create fixture table");

    let page_size: i64 = writer
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .expect("read page size");
    let payload_bytes = page_size * 32;
    for _ in 0..160 {
        writer
            .execute(
                "INSERT INTO blobs (value) VALUES (zeroblob(?1))",
                [payload_bytes],
            )
            .expect("autocommit fixture row");
    }

    let log_frames: i64 = writer
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(1))
        .expect("observe WAL frame count");
    assert!(
        log_frames > FORMER_AUTOCHECKPOINT_THRESHOLD_PAGES,
        "the commit sequence must retain more than the former automatic threshold; \
         observed {log_frames} frames"
    );
}

/// The other half of the ownership model: a writable pool that no
/// checkpoint task ever claims must retain SQLite's own bounded WAL
/// reclamation. The same commit sequence that retains >4,000 frames under
/// a claimed owner must NOT accumulate them here — an implicit
/// autocheckpoint fires on the threshold-crossing commit and drains the
/// WAL, which is the regression guard against unbounded WAL growth (and
/// eventual disk exhaustion) on embedded / one-shot writable pools.
#[test]
fn unclaimed_pool_retains_bounded_autocheckpoint_reclamation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bounded_fallback_reclamation.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("pool open");
    let writer = pool.writer().expect("pooled writer");
    writer
        .execute_batch("CREATE TABLE blobs (value BLOB NOT NULL)")
        .expect("create fixture table");

    let page_size: i64 = writer
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .expect("read page size");
    let payload_bytes = page_size * 32;
    for _ in 0..160 {
        writer
            .execute(
                "INSERT INTO blobs (value) VALUES (zeroblob(?1))",
                [payload_bytes],
            )
            .expect("autocommit fixture row");
    }

    // No PASSIVE pass here — read the frame count via wal_checkpoint's
    // log column only after the fixture, exactly as the claimed-owner
    // test does. With the bounded fallback live, the autocheckpoint that
    // fired on a threshold-crossing commit already drained the WAL, so
    // far fewer than the threshold's frames remain.
    let log_frames: i64 = writer
        .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(1))
        .expect("observe WAL frame count");
    assert!(
        log_frames < FALLBACK_WAL_AUTOCHECKPOINT_PAGES as i64,
        "an unclaimed pool must reclaim WAL frames via the bounded autocheckpoint; \
         observed {log_frames} retained frames"
    );
}

#[tokio::test]
#[serial]
async fn unset_write_queue_resolves_on_for_file_backed_pool() {
    let _pool_env = clear_pool_env();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unset_file_backed.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: None,
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(true));
    // Behavioral half: the resolved value actually routes — a writer
    // task spawns for this pool, not merely a config field flipping.
    assert!(
        pool.writer_task_handle()
            .expect("spawn inside a runtime context must not error")
            .is_some(),
        "resolved-on file-backed pool must actually spawn the writer task"
    );
}

#[tokio::test]
#[serial]
async fn unset_write_queue_resolves_off_for_memory_backed_pool() {
    let _pool_env = clear_pool_env();
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        write_queue_enabled: None,
        ..PoolConfig::default()
    })
    .expect("in-memory pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(false));
    // Behavioral half: resolved-off means no writer task, even inside a
    // runtime context where one could spawn.
    assert!(
        pool.writer_task_handle()
            .expect("disabled queue must resolve without error")
            .is_none(),
        "resolved-off in-memory pool must not spawn a writer task"
    );
}

#[test]
#[serial]
fn explicit_false_stays_off_for_file_backed_pool() {
    let _pool_env = clear_pool_env();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("explicit_false_file_backed.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(false));
}

#[tokio::test]
#[serial]
async fn explicit_true_stays_on_for_memory_backed_pool() {
    let _pool_env = clear_pool_env();
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        write_queue_enabled: Some(true),
        ..PoolConfig::default()
    })
    .expect("in-memory pool should open");
    assert_eq!(pool.config().write_queue_enabled, Some(true));
    // Pinned behavioral contract: the explicit-on preference survives in
    // the stored config, but an in-memory pool cannot host a writer
    // task — `writer_task::spawn` fails its standalone-connection open
    // and degrades to no writer task, so callers fall back to the
    // legacy pool-mutex write path and there is no JoinHandle to drain.
    assert!(
        pool.writer_task_handle()
            .expect("spawn degrade must resolve without error")
            .is_none(),
        "explicit-on in-memory pool must degrade to no writer task"
    );
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the spawn attempt must happen exactly once and degrade, not retry"
    );
    assert!(
        pool.take_writer_task_join().is_none(),
        "a degraded spawn stores no JoinHandle to drain"
    );
}

#[test]
#[serial]
fn explicit_true_on_memory_pool_warns_but_false_and_none_do_not() {
    let _pool_env = clear_pool_env();
    let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = WarningCapture {
        messages: Arc::clone(&messages),
    };

    tracing::subscriber::with_default(subscriber, || {
        let _explicit_true = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(true),
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
        let _explicit_false = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
        let _unset = ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: None,
            ..PoolConfig::default()
        })
        .expect("in-memory pool should open");
    });

    let messages = messages.lock().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.contains("write queue explicitly requested"))
            .count(),
        1,
        "only an explicit in-memory queue request should warn: {messages:?}"
    );
    let warning = messages
        .iter()
        .find(|message| message.contains("write queue explicitly requested"))
        .expect("explicit in-memory queue warning should be captured");
    assert!(
        warning.contains("in-memory pools cannot host a writer task"),
        "warning must explain why the request is inert: {messages:?}"
    );
}

#[test]
fn standalone_writer_open_counts_its_connection_class_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("standalone_writer_counter.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool");

    let _standalone = pool
        .open_standalone_writer()
        .expect("standalone writer opens");

    assert_eq!(
        pool.writer_acquisition_snapshot(),
        WriterAcquisitionSnapshot {
            acquisitions: 1,
            pooled_acquisitions: 0,
            standalone_acquisitions: 1,
            writer_task_acquisitions: 0,
            timeouts: 0,
            writer_task_begin_busy: 0,
            writer_task_begin_busy_absorbed: 0,
            writer_task_begin_errors: 0,
            writer_task_request_failures: 0,
            writer_task_side_effects_unknown: 0,
        },
        "the public standalone boundary must contribute to the aggregate exactly once"
    );
}

#[test]
fn reader_snapshot_tracks_pool_saturation_hold_lifecycle_and_exception_classes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reader_acquisition_counters.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        max_readers: 1,
        checkout_timeout: Duration::from_millis(2),
        ..PoolConfig::default()
    })
    .expect("file-backed pool");

    assert_eq!(
        pool.reader_acquisition_snapshot(),
        ReaderAcquisitionSnapshot {
            reader_admission_capacity: 1,
            available_reader_admission_slots: 1,
            ..ReaderAcquisitionSnapshot::default()
        }
    );

    let held = pool.reader().expect("first pooled checkout succeeds");
    assert_eq!(
        pool.reader_acquisition_snapshot(),
        ReaderAcquisitionSnapshot {
            reader_admission_capacity: 1,
            available_reader_admission_slots: 0,
            acquisitions: 1,
            pooled_checkouts: 1,
            active_pooled_checkouts: 1,
            peak_active_pooled_checkouts: 1,
            ..ReaderAcquisitionSnapshot::default()
        }
    );

    let timeout = match pool.reader() {
        Ok(_) => panic!("the sole live checkout must exhaust bounded admission"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &timeout,
            SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::DatabaseBusy
        ),
        "reader saturation must keep the pool-exhausted classification: {timeout}"
    );
    drop(held);

    let explicit = pool
        .open_standalone_reader(StandaloneReaderPurpose::ExplicitSqlReadTransaction)
        .expect("explicit read-transaction exception opens");
    drop(explicit);
    let infrastructure = pool
        .open_standalone_reader(StandaloneReaderPurpose::DiagnosticsIndependentSnapshot)
        .expect("infrastructure exception opens");
    drop(infrastructure);

    let snapshot = pool.reader_acquisition_snapshot();
    assert_eq!(snapshot.acquisitions, 2);
    assert_eq!(snapshot.pooled_checkouts, 1);
    assert_eq!(snapshot.standalone_opens, 1);
    assert_eq!(snapshot.infrastructure_standalone_opens, 1);
    assert_eq!(snapshot.checkout_timeouts, 1);
    assert_eq!(snapshot.active_pooled_checkouts, 0);
    assert_eq!(snapshot.peak_active_pooled_checkouts, 1);
    assert_eq!(snapshot.completed_pooled_checkouts, 1);
    assert!(
        snapshot.max_completed_hold_micros > 0,
        "the held checkout's completed lifecycle must expose nonzero hold evidence"
    );
}

#[test]
fn in_memory_pool_degrades_to_single_connection() {
    let cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).expect("in-memory pool should open");
    assert_eq!(pool.max_readers(), 0);
}

#[test]
fn wal_ceiling_below_one_frame_reset_floor_is_typed_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-ceiling-floor.db");
    let seed = Connection::open(&path).unwrap();
    seed.execute_batch("CREATE TABLE seed (id INTEGER PRIMARY KEY)")
        .unwrap();
    let page_size: i64 = seed
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let page_size = u64::try_from(page_size)
        .map_err(|_| SqliteError::InvalidData("SQLite reported a negative page size".to_string()))
        .unwrap();
    drop(seed);
    let bytes = page_size + 55;

    let error = match ConnectionPool::new(PoolConfig {
        path: Some(path),
        wal_ceiling: WalCeilingPolicy {
            bytes,
            source: WalCeilingSource::BackendField,
        },
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("the one-frame reset floor must refuse this policy"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SqliteError::WalCeilingBelowMinimum {
            bytes: observed,
            page_size: observed_page_size,
            minimum_bytes,
        } if observed == bytes
            && observed_page_size == page_size
            && minimum_bytes == page_size + 56
    ));
}

#[test]
fn wal_ceiling_floor_reads_existing_backend_page_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-ceiling-8192.db");
    let seed = Connection::open(&path).unwrap();
    seed.pragma_update(None, "page_size", 8192).unwrap();
    seed.execute_batch("CREATE TABLE seed (id INTEGER PRIMARY KEY)")
        .unwrap();
    let page_size: i64 = seed
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let page_size = u64::try_from(page_size)
        .map_err(|_| SqliteError::InvalidData("SQLite reported a negative page size".to_string()))
        .unwrap();
    assert_eq!(page_size, 8192, "fixture must persist the larger page size");
    drop(seed);

    let error = match ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        wal_ceiling: WalCeilingPolicy {
            bytes: 4096 + 56,
            source: WalCeilingSource::BackendField,
        },
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("an 8192-byte page cannot fit below its own one-frame floor"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SqliteError::WalCeilingBelowMinimum {
            bytes: 4152,
            page_size: 8192,
            minimum_bytes: 8248,
        }
    ));
    let refused = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let identity_rows: i64 = refused
        .query_row(
            "SELECT COUNT(*) FROM main.sqlite_master WHERE name = ?1",
            [DATABASE_ID_TABLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        identity_rows, 0,
        "WAL policy refusal must precede identity nonce installation"
    );
}

#[test]
fn wal_ceiling_at_one_frame_floor_fails_closed_without_io_limiter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wal-ceiling-unavailable.db");
    let seed = Connection::open(&path).unwrap();
    seed.execute_batch("CREATE TABLE seed (id INTEGER PRIMARY KEY)")
        .unwrap();
    let page_size: i64 = seed
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    let page_size = u64::try_from(page_size)
        .map_err(|_| SqliteError::InvalidData("SQLite reported a negative page size".to_string()))
        .unwrap();
    drop(seed);
    let bytes = page_size + 56;

    let error = match ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        wal_ceiling: WalCeilingPolicy {
            bytes,
            source: WalCeilingSource::Environment,
        },
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("enabled ceiling must not open without the WAL I/O limiter"),
        Err(error) => error,
    };
    assert_eq!(
        error.wal_capacity_stage(),
        Some(crate::error::SQLITE_WAL_CAPACITY_UNAVAILABLE_STAGE)
    );
    assert!(matches!(
        error,
        SqliteError::WalCapacityUnavailable {
            bytes: observed,
            capability: "WAL I/O limiter",
        } if observed == bytes
    ));
    let refused = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let identity_rows: i64 = refused
        .query_row(
            "SELECT COUNT(*) FROM main.sqlite_master WHERE name = ?1",
            [DATABASE_ID_TABLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        identity_rows, 0,
        "WAL policy refusal must precede identity nonce installation"
    );
}

#[test]
fn wal_ceiling_refuses_in_memory_backend_at_pool_open() {
    let error = match ConnectionPool::new(PoolConfig {
        wal_ceiling: WalCeilingPolicy {
            bytes: 8192,
            source: WalCeilingSource::BackendField,
        },
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("an in-memory backend cannot enforce a WAL ceiling"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SqliteError::WalCeilingUnsupported {
            backend_kind: "in-memory backend",
            ..
        }
    ));
}

#[test]
fn wal_ceiling_refuses_non_wal_backend_at_pool_open() {
    let dir = tempfile::tempdir().unwrap();
    let error = match ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("non-wal-ceiling.db")),
        wal_mode: false,
        wal_ceiling: WalCeilingPolicy {
            bytes: 8192,
            source: WalCeilingSource::BackendField,
        },
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("a rollback-journal backend cannot enforce a WAL ceiling"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SqliteError::WalCeilingUnsupported {
            backend_kind: "non-WAL backend",
            ..
        }
    ));
}

#[test]
fn wal_ceiling_refuses_offset_overflow_at_pool_open() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = i64::MAX as u64 + 1;
    let error = match ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("wal-ceiling-overflow.db")),
        wal_ceiling: WalCeilingPolicy {
            bytes,
            source: WalCeilingSource::BackendField,
        },
        ..PoolConfig::for_test()
    }) {
        Ok(_) => panic!("ceiling cannot exceed signed SQLite file offsets"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        SqliteError::WalCeilingOffsetOverflow { bytes: observed } if observed == bytes
    ));
}

#[test]
fn writer_checkout_and_release_works() {
    let cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    {
        let _writer = pool.writer().expect("writer checkout should succeed");
    }
    // After drop, writer should be re-acquirable.
    let _writer2 = pool
        .writer()
        .expect("second writer checkout should succeed");
}

#[test]
fn db_capacity_floor_refuses_pooled_writer_before_sqlite_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("capacity.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(100, |_| Ok(100));

    let error = match pool.writer() {
        Ok(_) => panic!("the reserve must refuse this checkout"),
        Err(error) => error,
    };
    let mapped = error.into_storage_error(StorageCapability::Sql, "test_write");
    assert!(
        matches!(
            mapped,
            StorageError::CapacityFloor {
                capability: StorageCapability::Sql,
                available_bytes: 100,
                floor_bytes: 100,
                ..
            }
        ),
        "the refusal must keep its typed capacity classification"
    );
    assert_eq!(pool.writer_acquisition_snapshot().pooled_acquisitions, 0);
}

#[test]
fn db_capacity_floor_keeps_legacy_pool_and_checkpoint_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-capacity.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE existing_data (value INTEGER)")
        .unwrap();
    drop(conn);

    let _probe = install_startup_space_probe(100, |_| Ok(100));
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("pool startup must remain available below the reserve");

    assert_eq!(pool.opened_database_id, None);
    let reader = pool.reader().expect("pooled reads remain available");
    let exists: i64 = reader
        .query_row(
            "SELECT count(*) FROM main.sqlite_master WHERE name = ?1",
            [DATABASE_ID_TABLE],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(exists, 0, "low-space startup must not write a nonce");
    drop(reader);
    assert!(matches!(
        pool.writer(),
        Err(SqliteError::CapacityFloor { .. })
    ));
    pool.open_standalone_writer_untracked()
        .expect("checkpoint infrastructure must still open");
}

#[test]
fn read_only_legacy_pool_accepts_later_nonce_installation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-read-only.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE existing_data (value INTEGER)")
        .unwrap();
    drop(conn);

    let read_only = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        read_only: true,
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("open legacy database read-only");
    assert_eq!(read_only.opened_database_id, None);

    let _probe = install_startup_space_probe(0, |_| {
        panic!("the disabled floor must not sample disk space")
    });
    let writable = ConnectionPool::new(PoolConfig {
        path: Some(path),
        wal_mode: false,
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("writable pool installs the nonce");
    assert!(writable.opened_database_id.is_some());

    read_only
        .open_reader_connection()
        .expect("the preexisting read-only pool must keep replacing readers");
}

#[test]
fn db_capacity_floor_samples_each_pooled_writer_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("fresh-capacity.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    let samples = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&samples);
    pool.set_test_write_admission(100, move |_| {
        match observed.fetch_add(1, Ordering::SeqCst) {
            0 => Ok(102),
            1 => Ok(100),
            extra => panic!("unexpected capacity sample {extra}"),
        }
    });

    drop(pool.writer().expect("first admission clears the reserve"));
    let second = pool.writer();
    assert!(
        matches!(
            second,
            Err(SqliteError::CapacityFloor {
                available_bytes: 100,
                ..
            })
        ),
        "the second admission must see the lower free-space sample"
    );
    assert_eq!(samples.load(Ordering::SeqCst), 2);
    assert_eq!(pool.writer_acquisition_snapshot().pooled_acquisitions, 1);
}

#[test]
fn db_capacity_floor_covers_standalone_and_cancellable_writer_admission() {
    let dir = tempfile::tempdir().unwrap();
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("other-capacity.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(100, |_| Ok(99));

    assert!(matches!(
        pool.open_standalone_writer(),
        Err(SqliteError::CapacityFloor { .. })
    ));
    assert!(matches!(
        pool.writer_until(|| false),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let counters = pool.writer_acquisition_snapshot();
    assert_eq!(counters.standalone_acquisitions, 0);
    assert_eq!(counters.pooled_acquisitions, 0);
}

#[test]
fn db_capacity_floor_does_not_probe_in_memory_pool() {
    let mut pool = ConnectionPool::new(PoolConfig::for_test()).unwrap();
    pool.set_test_write_admission(u64::MAX, |_| {
        panic!("an in-memory writer has no filesystem volume to probe")
    });
    drop(pool.writer().expect("in-memory writer remains available"));
}

#[test]
fn writer_checkout_snapshot_counts_successes_and_timeouts_at_the_pool_boundary() {
    let cfg = PoolConfig {
        path: None,
        checkout_timeout: Duration::from_millis(1),
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).unwrap();

    assert_eq!(
        pool.writer_acquisition_snapshot(),
        WriterAcquisitionSnapshot::default()
    );

    let held = pool.writer().expect("first checkout succeeds");
    let error = match pool.writer() {
        Ok(_) => panic!("the held pool mutex must force a finite-wait timeout"),
        Err(error) => error,
    };
    assert!(
        matches!(
            &error,
            SqliteError::WriterPoolCheckoutTimeout { timeout }
                if *timeout == Duration::from_millis(1)
        ),
        "timeout must have a stable, structurally matchable stage: {error}"
    );
    assert_eq!(
        pool.writer_acquisition_snapshot(),
        WriterAcquisitionSnapshot {
            acquisitions: 1,
            pooled_acquisitions: 1,
            standalone_acquisitions: 0,
            writer_task_acquisitions: 0,
            timeouts: 1,
            // A pool-mutex checkout timeout must NOT bleed into the
            // writer-task BEGIN counters: separate stages, separate
            // counters. This is the mislabeling guard in assertion form.
            writer_task_begin_busy: 0,
            writer_task_begin_busy_absorbed: 0,
            writer_task_begin_errors: 0,
            writer_task_request_failures: 0,
            writer_task_side_effects_unknown: 0,
        }
    );

    drop(held);
    let _reacquired = pool.writer().expect("checkout succeeds after release");
    assert_eq!(
        pool.writer_acquisition_snapshot(),
        WriterAcquisitionSnapshot {
            acquisitions: 2,
            pooled_acquisitions: 2,
            standalone_acquisitions: 0,
            writer_task_acquisitions: 0,
            timeouts: 1,
            writer_task_begin_busy: 0,
            writer_task_begin_busy_absorbed: 0,
            writer_task_begin_errors: 0,
            writer_task_request_failures: 0,
            writer_task_side_effects_unknown: 0,
        }
    );
}

#[test]
fn zero_wait_maintenance_skip_is_not_reported_as_a_checkout_timeout() {
    let pool = ConnectionPool::new(PoolConfig::default()).unwrap();
    let held = pool.writer().expect("finite-wait checkout succeeds");
    let before = pool.writer_acquisition_snapshot();

    assert!(
        pool.try_checkpoint_nowait().is_err(),
        "zero-wait maintenance checkout must skip while held"
    );

    assert_eq!(
        pool.writer_acquisition_snapshot(),
        before,
        "a checkpoint-style zero-wait skip is not a finite-wait checkout timeout"
    );
    drop(held);
}

#[test]
fn checkpoint_capability_reclaims_wal_below_the_capacity_floor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkpoint_floor.db");
    let mut pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .unwrap();
    pool.set_test_write_admission(0, |_| Ok(0));
    pool.writer()
        .unwrap()
        .execute_batch(
            "CREATE TABLE checkpoint_floor (id INTEGER); \
             INSERT INTO checkpoint_floor VALUES (1)",
        )
        .unwrap();

    let wal_path = path.with_extension("db-wal");
    assert!(std::fs::metadata(&wal_path).unwrap().len() > 0);
    pool.set_test_write_admission(100, |_| Ok(99));
    assert!(matches!(
        pool.writer(),
        Err(SqliteError::CapacityFloor { .. })
    ));
    let before = pool.writer_acquisition_snapshot();

    let checkpoint = pool
        .try_checkpoint_nowait()
        .expect("checkpoint recovery must bypass the floor");
    assert_eq!(checkpoint.passive().unwrap().busy, 0);
    assert_eq!(checkpoint.truncate().unwrap().busy, 0);
    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0);
    assert_eq!(pool.writer_acquisition_snapshot(), before);
}

/// ADR-091 Plank 0: `WriterGuard::transaction` registers/deregisters a
/// tx_registry entry around the closure. See
/// crates/khive-db/docs/api/pool.md#writer_guard_transaction_registers_during_closure_only
#[test]
#[serial(tx_registry)]
fn writer_guard_transaction_registers_during_closure_only() {
    let cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = ConnectionPool::new(cfg).unwrap();
    let guard = pool.writer().unwrap();

    let mut seen_during_closure = false;
    let result: Result<(), SqliteError> = guard.transaction(|_conn| {
        seen_during_closure = khive_storage::tx_registry::snapshot()
            .iter()
            .any(|(_, label)| label.as_deref() == Some("writer_guard_tx"));
        Ok(())
    });
    result.expect("transaction should commit");

    assert!(
        seen_during_closure,
        "expected a writer_guard_tx entry visible inside the closure"
    );
    assert!(
        !khive_storage::tx_registry::snapshot()
            .iter()
            .any(|(_, label)| label.as_deref() == Some("writer_guard_tx")),
        "expected the entry to be gone after the transaction completes"
    );
}

/// ADR-067 Component A: `writer_task_handle` must fail loud (typed
/// error, not panic) with no Tokio runtime available. See
/// crates/khive-db/docs/api/pool.md#writer_task_handle_fails_loud_without_tokio_runtime
#[test]
fn writer_task_handle_fails_loud_without_tokio_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_task_no_runtime.db");
    let cfg = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
    };
    let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");

    let result = pool.writer_task_handle();

    assert!(
        matches!(result, Err(StorageError::WriterTaskNoRuntime)),
        "expected Err(StorageError::WriterTaskNoRuntime) outside a Tokio \
         runtime, got {result:?}"
    );
    assert_eq!(
        pool.writer_task_spawn_count(),
        0,
        "the guard must reject before ever attempting tokio::spawn"
    );
}

/// #1847: strict store routing must preserve the typed missing-runtime
/// failure instead of collapsing it into a direct-writer fallback.
#[test]
fn strict_writer_task_for_write_preserves_missing_runtime_error() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("strict_writer_task_no_runtime.db")),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool should open");

    let result = pool.writer_task_for_write(None, "strict_test_write");

    assert!(
        matches!(result, Err(StorageError::WriterTaskNoRuntime)),
        "strict routing must preserve WriterTaskNoRuntime, got {result:?}"
    );
    assert_eq!(pool.writer_task_spawn_count(), 0);
}

/// Join-handle lifecycle: a spawn-configured pool stores exactly one
/// JoinHandle — the first `take_writer_task_join` after spawn returns
/// it, and every later take returns `None` (the one-shot contract that
/// lets exactly one subsystem own the drain).
#[tokio::test]
async fn take_writer_task_join_returns_some_once_then_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("join_lifecycle.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
    })
    .expect("file-backed pool should open");

    // Spawning is lazy: nothing to take before the first
    // `writer_task_handle()` call actually spawns the task.
    assert!(
        pool.take_writer_task_join().is_none(),
        "before spawn there is no JoinHandle to take"
    );
    assert!(!pool.writer_task_join_was_stored());
    pool.writer_task_handle()
        .expect("runtime is present")
        .expect("write queue enabled must spawn a writer task");
    assert!(pool.writer_task_join_was_stored());

    let join = pool
        .take_writer_task_join()
        .expect("the first take must return the spawned task's JoinHandle");
    assert!(
        pool.take_writer_task_join().is_none(),
        "the second take must return None — the handle is one-shot"
    );

    // Await the taken handle before the test exits instead of dropping
    // it detached. The writer task only exits once every
    // `WriterTaskHandle` clone (the mpsc senders) is gone, and the pool's
    // own `writer_task` OnceLock holds one, so the pool must drop first —
    // the same drop-then-await order the batch-ingest drain relies on.
    drop(pool);
    tokio::time::timeout(Duration::from_secs(5), join)
        .await
        .expect("the writer task must exit once every handle clone is dropped")
        .expect("the writer task must not panic");
}

/// Debug half of the first-wins contract: a second
/// `set_writer_task_join` call is a construction bug, and debug builds
/// trip the method's debug_assert loudly instead of carrying on.
#[cfg(debug_assertions)]
#[tokio::test]
#[should_panic(expected = "writer task JoinHandle stored twice")]
async fn set_writer_task_join_second_store_trips_debug_assert() {
    let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool should open");
    pool.set_writer_task_join(tokio::spawn(async {}));
    pool.set_writer_task_join(tokio::spawn(async {}));
}

/// The at-most-once guard holds across the TAKEN state too: once the
/// handle has been taken, the slot is empty, but a second store is still
/// a construction bug and must trip the same debug_assert (the
/// `writer_task_join_stored` flag remembers the first store).
#[cfg(debug_assertions)]
#[tokio::test]
#[should_panic(expected = "writer task JoinHandle stored twice")]
async fn set_writer_task_join_second_store_after_take_trips_debug_assert() {
    let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool should open");
    pool.set_writer_task_join(tokio::spawn(async {}));
    assert!(pool.take_writer_task_join().is_some());
    pool.set_writer_task_join(tokio::spawn(async {}));
}

/// Release half of the first-wins contract: with the debug_assert
/// compiled out, a second `set_writer_task_join` call keeps the
/// EXISTING handle and drops the new one. The stored handle is
/// therefore the first task's, so awaiting the taken handle completes
/// the FIRST task's observable effect.
#[cfg(not(debug_assertions))]
#[tokio::test]
async fn set_writer_task_join_first_wins_keeps_existing_handle() {
    let pool = ConnectionPool::new(PoolConfig::default()).expect("in-memory pool should open");

    // First task: completes promptly and signals completion — the
    // observable effect the bounded await below asserts on.
    let (first_done_tx, first_done_rx) = tokio::sync::oneshot::channel::<()>();
    let first = tokio::spawn(async move {
        let _ = first_done_tx.send(());
    });
    // Second task: parks on a receiver nobody sends to, so it never
    // completes on its own. If first-wins failed and this task's handle
    // were the stored one, the bounded await below would time out.
    let (_never_sent, never_rx) = tokio::sync::oneshot::channel::<()>();
    let second = tokio::spawn(async move {
        let _ = never_rx.await;
    });

    pool.set_writer_task_join(first);
    pool.set_writer_task_join(second);

    let taken = pool
        .take_writer_task_join()
        .expect("the first handle must still be stored");
    tokio::time::timeout(Duration::from_secs(5), taken)
        .await
        .expect("stored handle must be the first task's; the second never completes")
        .expect("the first task must not panic");
    assert!(
        first_done_rx.await.is_ok(),
        "completing the taken handle must mean the FIRST task ran to completion"
    );
}

/// ADR-091 backend-scoped attribution: the real path, a directory
/// symlink, a file-level symlink, a relative spelling, and a bare file
/// name (resolved against the current directory) must all mint an
/// identical `DbIdentity` and canonical path for the same database.
#[test]
#[serial(pool_cwd)]
fn mint_db_identity_alias_convergence() {
    let dir = tempfile::tempdir().unwrap();
    let real_dir = dir.path().join("real");
    fs::create_dir(&real_dir).unwrap();
    let db_path = real_dir.join("khive.db");
    fs::write(&db_path, b"").unwrap();

    #[cfg(unix)]
    let dir_symlink = dir.path().join("dir_link");
    #[cfg(unix)]
    let file_symlink = dir.path().join("file_link.db");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&real_dir, &dir_symlink).unwrap();
        std::os::unix::fs::symlink(&db_path, &file_symlink).unwrap();
    }

    let (via_real, canonical_real) = mint_db_identity(&db_path).unwrap();

    // Relative spelling: resolved against the process CWD (step 1).
    let relative_result = {
        let _cwd = CwdGuard::enter(&real_dir);
        mint_db_identity(&PathBuf::from("khive.db"))
    };
    let (via_relative, canonical_relative) = relative_result.unwrap();
    assert_eq!(canonical_real, canonical_relative);
    assert_eq!(via_real, via_relative);

    #[cfg(unix)]
    {
        let (via_dir_symlink, canonical_dir_symlink) =
            mint_db_identity(&dir_symlink.join("khive.db")).unwrap();
        assert_eq!(canonical_real, canonical_dir_symlink);
        assert_eq!(via_real, via_dir_symlink);

        let (via_file_symlink, canonical_file_symlink) = mint_db_identity(&file_symlink).unwrap();
        assert_eq!(canonical_real, canonical_file_symlink);
        assert_eq!(via_real, via_file_symlink);
    }

    // Bare file name: resolved against the current directory (step 1).
    let bare_name_result = {
        let _cwd = CwdGuard::enter(&real_dir);
        mint_db_identity(&PathBuf::from("khive.db"))
    };
    let (via_bare_name, canonical_bare_name) = bare_name_result.unwrap();
    assert_eq!(canonical_real, canonical_bare_name);
    assert_eq!(via_real, via_bare_name);
}

/// ADR-091 backend-scoped attribution: `DbIdentity`/canonical-path
/// equality across alias spellings (proven above by
/// `mint_db_identity_alias_convergence`) does not by itself prove the
/// walpin sidecar re-key — `sidecar_dir_for` is a separate, purely
/// lexical derivation (`walpin::sidecar_dir_for`) that must be fed the
/// *minted* canonical path, never the raw configured one. This test
/// opens a real `ConnectionPool` (not the private `mint_db_identity` free
/// function) through each alias spelling and asserts
/// `sidecar_dir_for(pool.canonical_path())` converges to one directory —
/// exercising the actual `ConnectionPool::new` → `canonical_path()` wiring
/// every sidecar consumer (`checkpoint.rs`) reads from.
#[test]
#[serial(pool_cwd)]
fn sidecar_dir_for_alias_convergence() {
    let dir = tempfile::tempdir().unwrap();
    let real_dir = dir.path().join("real");
    fs::create_dir(&real_dir).unwrap();
    let db_path = real_dir.join("khive.db");
    fs::write(&db_path, b"").unwrap();

    #[cfg(unix)]
    let dir_symlink = dir.path().join("dir_link");
    #[cfg(unix)]
    let file_symlink = dir.path().join("file_link.db");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&real_dir, &dir_symlink).unwrap();
        std::os::unix::fs::symlink(&db_path, &file_symlink).unwrap();
    }

    let pool_for = |path: &Path| -> Arc<ConnectionPool> {
        let cfg = PoolConfig {
            path: Some(path.to_path_buf()),
            ..PoolConfig::for_test()
        };
        Arc::new(ConnectionPool::new(cfg).expect("file-backed pool should open"))
    };
    let sidecar_of = |pool: &ConnectionPool| -> PathBuf {
        crate::walpin::sidecar_dir_for(pool.canonical_path().expect("file-backed pool"))
    };

    let via_real = pool_for(&db_path);
    let sidecar_real = sidecar_of(&via_real);

    let via_relative = {
        let _cwd = CwdGuard::enter(&real_dir);
        pool_for(Path::new("khive.db"))
    };
    assert_eq!(
        sidecar_real,
        sidecar_of(&via_relative),
        "a relative spelling of the same database must derive the same sidecar directory"
    );

    #[cfg(unix)]
    {
        let via_dir_symlink = pool_for(&dir_symlink.join("khive.db"));
        assert_eq!(
            sidecar_real,
            sidecar_of(&via_dir_symlink),
            "opening through a directory symlink must derive the same sidecar directory"
        );

        let via_file_symlink = pool_for(&file_symlink);
        assert_eq!(
            sidecar_real,
            sidecar_of(&via_file_symlink),
            "opening through a file-level symlink must derive the same sidecar directory"
        );
    }

    let via_bare_name = {
        let _cwd = CwdGuard::enter(&real_dir);
        pool_for(Path::new("khive.db"))
    };
    assert_eq!(
        sidecar_real,
        sidecar_of(&via_bare_name),
        "a bare file name resolved against the current directory must derive the same \
         sidecar directory"
    );
}

/// ADR-091 backend-scoped attribution: opening via a file-level symlink
/// whose target does not exist yet (a valid first-open state), then
/// after the target is created, opening via the target path directly,
/// must mint identical `DbIdentity` values — the first-open path
/// resolves the final component before canonicalizing the parent.
#[cfg(unix)]
#[test]
fn mint_db_identity_dangling_symlink_first_open_convergence() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.db");
    let link = dir.path().join("link.db");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(!target.exists(), "target must not exist yet (dangling)");

    let (via_dangling_link, canonical_via_link) = mint_db_identity(&link).unwrap();

    // Now create the target (as SQLite would on first write) and mint
    // again directly against the target path.
    fs::write(&target, b"").unwrap();
    let (via_target, canonical_via_target) = mint_db_identity(&target).unwrap();

    assert_eq!(canonical_via_link, canonical_via_target);
    assert_eq!(via_dangling_link, via_target);
}

/// A resolved target whose parent directory does not exist must fail
/// minting exactly as the subsequent database open itself would fail.
#[test]
fn mint_db_identity_missing_parent_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nonexistent_subdir").join("khive.db");
    let result = mint_db_identity(&missing);
    assert!(
        result.is_err(),
        "minting must fail when the parent directory does not exist"
    );
}

/// Non-UTF-8 database paths (Unix) must round-trip through
/// `DbIdentity`/canonicalization without loss.
#[cfg(unix)]
#[test]
fn mint_db_identity_non_utf8_path_round_trips() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    // 0xFF is not valid UTF-8 as a standalone byte.
    let raw_name = OsStr::from_bytes(b"khive-\xffdb.sqlite");
    let db_path = dir.path().join(raw_name);
    // Some Unix filesystems (notably macOS's APFS) reject non-UTF-8
    // names outright at the syscall level — that is a filesystem
    // limitation, not a `mint_db_identity` bug, so skip rather than
    // fail where the underlying `write` itself cannot succeed.
    if let Err(e) = fs::write(&db_path, b"") {
        eprintln!(
            "skipping mint_db_identity_non_utf8_path_round_trips: filesystem rejected a \
             non-UTF-8 file name ({e}); this platform's filesystem does not support the \
             case under test"
        );
        return;
    }

    let (identity, canonical) = mint_db_identity(&db_path).unwrap();
    assert_eq!(canonical.file_name().unwrap(), raw_name);

    let (identity_again, canonical_again) = mint_db_identity(&db_path).unwrap();
    assert_eq!(identity, identity_again);
    assert_eq!(canonical, canonical_again);
}

fn admission_identity_after_refusal(pool: &ConnectionPool) -> String {
    let held = pool.reader().expect("hold the sole reader");
    let Err(error) = pool.resolve_reader_checkout(
        StorageCapability::Sql,
        "identity_read",
        pool.reader_until(|| false),
    ) else {
        panic!("held reader must exhaust this pool's admission budget");
    };
    assert!(
        error.is_retryable(),
        "admission refusal must remain retryable"
    );
    let display = error.to_string();
    let StorageError::AdmissionTimeout {
        operation,
        timeout_ms,
        pool_identity,
    } = error
    else {
        panic!("pool refusal must retain its typed admission classification");
    };
    assert_eq!(
        operation, "identity_read",
        "pool identity must not alter operation"
    );
    assert_eq!(timeout_ms, 20);
    let identity = pool_identity.expect("typed admission error must name the pool");
    assert!(
        !identity.contains('/') && !identity.contains('\\'),
        "pool identity must never contain a directory or separator: {identity}"
    );
    assert_eq!(
        display,
        format!("admission timeout during identity_read after 20ms (pool: {identity})"),
        "admission error text must name the refusing pool"
    );
    drop(held);
    identity
}

fn identity_test_pool(path: Option<PathBuf>, read_only: bool) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path,
        read_only,
        max_readers: 1,
        checkout_timeout: Duration::from_millis(20),
        ..PoolConfig::default()
    })
    .unwrap()
}

#[test]
fn reader_admission_timeout_identifies_the_refusing_pool() {
    let dir = tempfile::tempdir().unwrap();
    for read_only in [false, true] {
        let name = format!("identity-{}.db", uuid::Uuid::new_v4());
        let path = dir.path().join(&name);
        {
            let seed = Connection::open(&path).unwrap();
            seed.execute_batch("CREATE TABLE seed (id INTEGER)")
                .unwrap();
        }
        let canonical = fs::canonicalize(&path).unwrap();
        let configured = dir.path().join(".").join(&name);
        assert_ne!(configured.as_os_str(), canonical.as_os_str());
        let pool = identity_test_pool(Some(configured), read_only);
        assert_eq!(
            admission_identity_after_refusal(&pool),
            name,
            "typed admission field must contain only the canonical file name"
        );
        #[cfg(unix)]
        {
            let alias = dir
                .path()
                .join(format!("alias-{}.db", uuid::Uuid::new_v4()));
            std::os::unix::fs::symlink(&canonical, &alias).unwrap();
            let alias_pool = identity_test_pool(Some(alias), read_only);
            assert_eq!(
                admission_identity_after_refusal(&alias_pool),
                name,
                "symlink spelling must not change the canonical database file name"
            );
        }
    }
    let memory = identity_test_pool(None, false);
    assert_eq!(admission_identity_after_refusal(&memory), ":memory:");
}

#[test]
fn reader_admission_identity_hash_is_build_stable() {
    // Literal vectors pin the specified encoding and digest, not a seeded
    // process-local hasher or a hash recomputed by the implementation.
    #[cfg(unix)]
    assert_eq!(
        pool_identity_suffix(Path::new("/khive/pool/khive.db")),
        "8fa8797b",
        "suffix must match the published Unix SHA-256 vector"
    );
    #[cfg(windows)]
    assert_eq!(
        pool_identity_suffix(Path::new("/khive/pool/khive.db")),
        "1186b990",
        "suffix must match the published Windows SHA-256 vector"
    );
}

fn assert_disambiguated_identity(identity: &str, basename: &str) {
    let suffix = identity
        .strip_prefix(&format!("{basename}#"))
        .expect("different open files with the same basename need a hash suffix");
    assert_eq!(
        suffix.len(),
        8,
        "disambiguation needs exactly eight hex digits"
    );
    assert!(
        suffix.bytes().all(|b| b.is_ascii_hexdigit()),
        "disambiguation must contain only a hash, never directory text"
    );
}

#[test]
fn reader_admission_identity_disambiguates_open_files() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let basename = format!("collision-{}.db", uuid::Uuid::new_v4());
    let first = identity_test_pool(Some(first_dir.path().join(&basename)), false);
    assert_eq!(admission_identity_after_refusal(&first), basename);
    let second_path = second_dir.path().join(&basename);
    let second = identity_test_pool(Some(second_path.clone()), false);
    let first_identity = admission_identity_after_refusal(&first);
    let second_identity = admission_identity_after_refusal(&second);
    assert_disambiguated_identity(&first_identity, &basename);
    assert_disambiguated_identity(&second_identity, &basename);
    assert_ne!(
        first_identity, second_identity,
        "distinct files need distinct identities"
    );
    assert_eq!(admission_identity_after_refusal(&first), first_identity);
    drop(second);
    assert_eq!(
        admission_identity_after_refusal(&first),
        basename,
        "closing the colliding store must remove its registry entry"
    );
    let reopened = identity_test_pool(Some(second_path), false);
    assert_eq!(admission_identity_after_refusal(&first), first_identity);
    assert_eq!(admission_identity_after_refusal(&reopened), second_identity);
}

#[test]
fn reader_admission_identity_same_path_pools_share_label() {
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let basename = format!("same-path-{}.db", uuid::Uuid::new_v4());
    let first = identity_test_pool(Some(first_dir.path().join(&basename)), false);
    let duplicate = identity_test_pool(Some(first_dir.path().join(".").join(&basename)), false);
    assert_eq!(
        admission_identity_after_refusal(&first),
        basename,
        "two pools on the same canonical path must not get a suffix"
    );
    assert_eq!(admission_identity_after_refusal(&duplicate), basename);
    let other = identity_test_pool(Some(second_dir.path().join(&basename)), false);
    let first_identity = admission_identity_after_refusal(&first);
    let other_identity = admission_identity_after_refusal(&other);
    assert_disambiguated_identity(&first_identity, &basename);
    assert_disambiguated_identity(&other_identity, &basename);
    assert_eq!(admission_identity_after_refusal(&duplicate), first_identity);
    drop(first);
    assert_eq!(
        admission_identity_after_refusal(&other),
        other_identity,
        "dropping one pool must retain the other pool's path registration"
    );
    drop(duplicate);
    assert_eq!(
        admission_identity_after_refusal(&other),
        basename,
        "dropping the final pool must remove the path registration"
    );
}

/// The checkout tri-state, arm by arm, at its single home. Each refusal
/// arm asserts the classification it must NOT collapse into, because the
/// historical defect was exactly a pairwise swap: cancellation surfaced as
/// the retryable `AdmissionTimeout` while genuine pool exhaustion surfaced
/// as a non-retryable `Driver` failure.
#[test]
fn resolve_reader_checkout_maps_each_arm_distinctly() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        ..PoolConfig::default()
    })
    .unwrap();

    let guard = pool
        .resolve_reader_checkout(
            StorageCapability::Sql,
            "arm_checked_out",
            pool.reader_until(|| false),
        )
        .expect("an uncontended checkout must pass the guard through");
    drop(guard);

    let Err(cancelled) =
        pool.resolve_reader_checkout(StorageCapability::Sql, "arm_cancelled", Ok(None))
    else {
        panic!("a cancelled checkout must be refused");
    };
    assert!(
        matches!(cancelled, StorageError::Timeout { .. }),
        "cancellation/deadline before checkout must be the non-retryable \
         Timeout, got {cancelled:?}"
    );

    let Err(exhausted) = pool.resolve_reader_checkout(
        StorageCapability::Sql,
        "arm_exhausted",
        Err(pool_exhausted_error(Duration::from_millis(5), 1)),
    ) else {
        panic!("an exhausted checkout must be refused");
    };
    assert!(
        matches!(exhausted, StorageError::AdmissionTimeout { .. }),
        "the pool's own SQLITE_BUSY (checkout_timeout exhausted) must be \
         the retryable AdmissionTimeout, got {exhausted:?}"
    );

    let Err(opaque) = pool.resolve_reader_checkout(
        StorageCapability::Entities,
        "arm_driver",
        Err(SqliteError::InvalidData("retired pooled writer".into())),
    ) else {
        panic!("an opaque checkout error must be refused");
    };
    assert!(
        matches!(
            &opaque,
            StorageError::Driver { capability, .. }
                if *capability == StorageCapability::Entities
        ),
        "any other checkout error must stay a non-retryable Driver failure \
         under the caller's capability, got {opaque:?}"
    );
}

/// #2793: a maximum with no name has no next step for the operator who
/// reads it. The fast checkouts are the control — they complete through
/// the same route, so naming the slow one distinguishes rather than
/// restating that something was recorded.
#[test]
fn the_longest_completed_hold_names_the_operation_that_held_it() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        ..PoolConfig::default()
    })
    .unwrap();

    for _ in 0..3 {
        let guard = pool
            .resolve_reader_checkout(
                StorageCapability::Sql,
                "fast_read",
                pool.reader_until(|| false),
            )
            .expect("a fast checkout resolves");
        drop(guard);
    }

    let slow = pool
        .resolve_reader_checkout(
            StorageCapability::Sql,
            "slow_read",
            pool.reader_until(|| false),
        )
        .expect("the slow checkout resolves");
    // The sleep orders the holds; nothing here asserts a duration, because
    // the hold figure is diagnostic evidence and never a timing gate.
    thread::sleep(Duration::from_millis(20));
    drop(slow);

    let snapshot = pool.reader_acquisition_snapshot();
    assert_eq!(
        snapshot.completed_pooled_checkouts, 4,
        "all four checkouts must complete through the pooled route, or the \
         attribution below is reading a population of one"
    );
    assert_eq!(
        snapshot.max_completed_hold_operation,
        Some("slow_read"),
        "the longest hold must name the operation that held it; got {:?} at \
         {} micros",
        snapshot.max_completed_hold_operation,
        snapshot.max_completed_hold_micros
    );
}

/// The `None` in the snapshot is a reading, not a gap: a checkout that
/// never passed `resolve_reader_checkout` carries no operation name, and
/// the diagnostics say so rather than attributing it to whatever ran
/// nearby.
#[test]
fn a_checkout_taken_outside_the_resolve_route_reports_no_operation() {
    let pool = ConnectionPool::new(PoolConfig {
        path: None,
        ..PoolConfig::default()
    })
    .unwrap();

    let guard = pool
        .reader_until(|| false)
        .expect("the checkout succeeds")
        .expect("the checkout is not cancelled");
    drop(guard);

    let snapshot = pool.reader_acquisition_snapshot();
    assert_eq!(snapshot.completed_pooled_checkouts, 1);
    assert_eq!(
        snapshot.max_completed_hold_operation, None,
        "an unlabelled route must report no operation rather than borrow one"
    );
}
