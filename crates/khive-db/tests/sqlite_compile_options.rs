//! Build-time options of the bundled SQLite that the store's concurrency depends on. The
//! workspace `.cargo/config.toml` sets them through `LIBSQLITE3_FLAGS`, which the
//! libsqlite3-sys bundled build passes to the compiler.
//!
//! Both options remove a process-wide mutex that every connection shares. FTS5 and BM25
//! ranking allocate and fetch pages per row, so with either lock in place concurrent
//! searches on separate connections serialize, and with both in place removing one only
//! moves the wait to the other.

fn compile_options() -> Vec<String> {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory connection");
    let mut stmt = conn
        .prepare("PRAGMA compile_options")
        .expect("prepare compile_options");
    stmt.query_map([], |row| row.get::<_, String>(0))
        .expect("query compile_options")
        .collect::<Result<Vec<_>, _>>()
        .expect("read compile_options")
}

fn assert_bundled_build_is_visible(options: &[String]) {
    // Control: the read works and sees the bundled build's own options.
    assert!(
        options.iter().any(|o| o == "ENABLE_FTS5"),
        "compile_options did not list ENABLE_FTS5, so this read is not seeing the bundled SQLite: {options:?}"
    );
}

/// With memory statistics on (SQLite's default), every `sqlite3_malloc` and `sqlite3_free`
/// takes one global mutex to update the counters. khive reads none of the statistics this
/// gives up (`sqlite3_memory_used`, `sqlite3_status`, the soft and hard heap limits).
#[test]
fn bundled_sqlite_keeps_no_global_memory_statistics() {
    let options = compile_options();
    assert_bundled_build_is_visible(&options);
    assert!(
        options.iter().any(|o| o == "DEFAULT_MEMSTATUS=0"),
        "SQLite was built with memory statistics on, so every allocation takes one process-wide \
         mutex; build with LIBSQLITE3_FLAGS containing -DSQLITE_DEFAULT_MEMSTATUS=0 as \
         .cargo/config.toml sets it: {options:?}"
    );
}

/// `SQLITE_ENABLE_MEMORY_MANAGEMENT` makes every connection in the process share one page
/// cache behind one mutex. khive calls nothing the define enables (`sqlite3_release_memory`,
/// the soft and hard heap limits), and without it each connection keeps its own cache.
#[test]
fn bundled_sqlite_uses_per_connection_page_caches() {
    let options = compile_options();
    assert_bundled_build_is_visible(&options);
    assert!(
        !options.iter().any(|o| o == "ENABLE_MEMORY_MANAGEMENT"),
        "SQLite was built with ENABLE_MEMORY_MANAGEMENT (one process-wide page cache behind one \
         mutex); build with LIBSQLITE3_FLAGS containing -USQLITE_ENABLE_MEMORY_MANAGEMENT as \
         .cargo/config.toml sets it: {options:?}"
    );
}
