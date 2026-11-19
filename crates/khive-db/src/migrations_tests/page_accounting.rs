use super::*;
use std::path::Path;

fn file_names(dir: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(dir)
        .expect("list dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn pragma_u64(conn: &Connection, pragma: &str) -> u64 {
    let value: i64 = conn.query_row(pragma, [], |row| row.get(0)).expect(pragma);
    u64::try_from(value).expect("non-negative pragma value")
}

/// Insert rows that each spill past one page, then delete them all, so their
/// pages land on the freelist.
fn fill_and_free(conn: &Connection, rows: usize) {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS ballast (id INTEGER PRIMARY KEY, body BLOB);")
        .expect("create ballast");
    for _ in 0..rows {
        conn.execute("INSERT INTO ballast (body) VALUES (zeroblob(4096))", [])
            .expect("insert ballast row");
    }
    conn.execute("DELETE FROM ballast", [])
        .expect("delete ballast");
}

#[test]
fn page_accounting_reports_freed_pages_as_the_writer_sees_them() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pages.db");
    let conn = Connection::open(&path).expect("create db");
    conn.execute_batch("CREATE TABLE ballast (id INTEGER PRIMARY KEY, body BLOB);")
        .expect("create ballast");

    let fresh = inspect_page_accounting(&path).expect("accounting on a fresh file");
    assert_eq!(fresh.freelist_count, 0, "a fresh file has no free pages");

    fill_and_free(&conn, 200);
    let writer = PageAccounting {
        page_size: pragma_u64(&conn, "PRAGMA page_size"),
        page_count: pragma_u64(&conn, "PRAGMA page_count"),
        freelist_count: pragma_u64(&conn, "PRAGMA freelist_count"),
    };
    drop(conn);

    let read = inspect_page_accounting(&path).expect("accounting after the deletes");
    assert!(
        read.freelist_count >= 200,
        "200 deleted rows of one overflow page each must free at least 200 pages, read {read:?}"
    );
    assert!(read.freelist_count < read.page_count, "{read:?}");
    assert_eq!(
        read, writer,
        "the read-only open must report what the writer reported"
    );
}

fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

#[test]
fn page_accounting_includes_frames_not_yet_checkpointed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("wal-source.db");
    let snapshot = dir.path().join("frozen-wal-snapshot.db");
    let conn = Connection::open(&source).expect("create db");
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .expect("WAL mode without automatic checkpoints");
    fill_and_free(&conn, 200);
    let writer_free = pragma_u64(&conn, "PRAGMA freelist_count");
    let writer_pages = pragma_u64(&conn, "PRAGMA page_count");
    assert!(writer_free >= 200, "writer sees {writer_free} free pages");

    // A live writer's database is refused, so inspect a frozen copy: the main
    // file, its WAL and its WAL index, all three read-only.
    let snapshot_paths = [
        snapshot.clone(),
        sidecar(&snapshot, "-wal"),
        sidecar(&snapshot, "-shm"),
    ];
    for (from, to) in [
        &source,
        &sidecar(&source, "-wal"),
        &sidecar(&source, "-shm"),
    ]
    .into_iter()
    .zip(&snapshot_paths)
    {
        std::fs::copy(from, to).expect("copy snapshot file");
        let mut permissions = std::fs::metadata(to).expect("stat copy").permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(to, permissions).expect("freeze copy");
    }
    drop(conn);

    // Control: the snapshot's main-file header still says no free pages, because
    // no checkpoint copied the frames back. A header read that skipped the WAL
    // would report this zero.
    let header = std::fs::read(&snapshot).expect("read main file");
    let on_disk_free = u32::from_be_bytes(header[36..40].try_into().expect("4 bytes"));
    assert_eq!(
        on_disk_free, 0,
        "the deletes must still be WAL-only for this test to mean anything"
    );

    let read = inspect_page_accounting(&snapshot).expect("accounting with uncheckpointed frames");
    assert_eq!(read.freelist_count, writer_free);
    assert_eq!(read.page_count, writer_pages);

    for path in &snapshot_paths {
        let mut permissions = std::fs::metadata(path).expect("stat copy").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).expect("thaw copy");
    }
}

#[test]
fn page_accounting_refuses_a_database_with_a_live_writer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("live.db");
    let conn = Connection::open(&path).expect("create db");
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x INTEGER);")
        .expect("WAL database held open");

    let error = inspect_page_accounting(&path).expect_err("a live writer's file must be refused");
    assert!(
        error
            .to_string()
            .contains("writable WAL shared-memory sidecar"),
        "{error}"
    );
    drop(conn);
}

#[test]
fn page_accounting_and_integrity_create_no_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("closed.db");
    {
        let conn = Connection::open(&path).expect("create db");
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x INTEGER);")
            .expect("WAL file with one table");
    }
    let before = file_names(dir.path());
    assert_eq!(
        before,
        vec!["closed.db".to_string()],
        "control: closing the only connection removes the WAL sidecars"
    );

    inspect_page_accounting(&path).expect("accounting");
    assert_eq!(
        inspect_integrity(&path).expect("integrity"),
        vec!["ok".to_string()]
    );

    assert_eq!(
        file_names(dir.path()),
        before,
        "a read must not leave a -wal or -shm behind"
    );
}

#[test]
fn integrity_returns_every_problem_past_sqlites_default_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.db");
    {
        let conn = Connection::open(&path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE t (a INTEGER); CREATE INDEX i ON t (a); \
             CREATE TABLE e (a INTEGER); CREATE INDEX ie ON e (a);",
        )
        .expect("schema");
        for value in 0..300 {
            conn.execute("INSERT INTO t (a) VALUES (?1)", [value])
                .expect("insert row");
        }
        // Point index `i` at the empty index tree of `e`: every row of `t` is
        // then missing from its index.
        let empty_root: i64 = conn
            .query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name = 'ie'",
                [],
                |row| row.get(0),
            )
            .expect("root page of ie");
        conn.execute_batch("PRAGMA writable_schema = ON;")
            .expect("writable schema");
        conn.execute("DELETE FROM sqlite_schema WHERE name = 'ie'", [])
            .expect("drop ie from the schema");
        conn.execute(
            "UPDATE sqlite_schema SET rootpage = ?1 WHERE name = 'i'",
            [empty_root],
        )
        .expect("repoint i");
    }

    // Control: SQLite's own default stops at 100 rows on this file.
    let conn = Connection::open(&path).expect("reopen");
    let mut default_check = conn.prepare("PRAGMA integrity_check").expect("prepare");
    let default_rows = default_check
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows");
    assert_eq!(
        default_rows.len(),
        100,
        "the default cap this test must exceed"
    );
    drop(default_check);
    drop(conn);

    let rows = inspect_integrity(&path).expect("integrity check runs on a corrupt file");
    let missing = rows
        .iter()
        .filter(|row| row.contains("missing from index i"))
        .count();
    assert_eq!(
        missing,
        300,
        "every row of t is reported, rows: {}",
        rows.len()
    );
    assert!(rows.len() > 100, "{} rows", rows.len());
    assert!(!rows.iter().any(|row| row == "ok"));
}
