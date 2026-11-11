//! The permanent progress handler stops a statement only when this thread's registration
//! names the statement's own connection.

use super::*;
use rusqlite::Connection;

const ROWS: i64 = 300_000;
const LONG_READ: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c \
                         WHERE x < 300000) SELECT count(*) FROM c";
const LONG_WRITE: &str = "INSERT INTO t(x) WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL \
                          SELECT x + 1 FROM c WHERE x < 300000) SELECT x FROM c";

fn connection() -> ProgressConn {
    ProgressConn::new(Connection::open_in_memory().unwrap()).unwrap()
}

fn scope(operation: &'static str) -> (Arc<ReadControl>, InterruptibleReadScope) {
    let control = ReadControl::new(&capture_request_read_context(), operation);
    let scope = InterruptibleReadScope {
        control: Arc::clone(&control),
        capability: StorageCapability::Sql,
    };
    (control, scope)
}

fn long_read(conn: &ProgressConn) -> StorageResult<i64> {
    conn.query_row(LONG_READ, [], |row| row.get(0))
        .map_err(|error| StorageError::driver(StorageCapability::Sql, "long_read", error))
}

/// Run `long_read` and record whether SQLite itself interrupted the statement.
///
/// The scope maps a read that completes after a stop request to a timeout as well, so
/// the scope's result alone cannot show that the statement was stopped.
fn interrupted_long_read(conn: &ProgressConn, interrupted: &mut bool) -> StorageResult<i64> {
    let result = long_read(conn);
    *interrupted = matches!(&result, Err(error) if storage_error_is_sqlite_interrupt(error));
    result
}

#[test]
fn a_cancel_stops_the_registered_read_and_not_a_write_on_another_connection() {
    let read_conn = connection();
    let write_conn = connection();
    write_conn
        .execute_batch("CREATE TABLE t(x INTEGER)")
        .unwrap();
    let (control, scope) = scope("identity");
    let mut write = None;
    let mut interrupted = false;
    let read = scope.run(&read_conn, || {
        control.cancel(STOP_REQUEST);
        write = Some(write_conn.execute(LONG_WRITE, []));
        interrupted_long_read(&read_conn, &mut interrupted)
    });
    assert!(
        interrupted,
        "SQLite interrupted the registered read's statement"
    );
    assert_eq!(
        write.expect("the write ran").expect("the write completes"),
        ROWS as usize,
        "a statement on another connection of the thread is never stopped by the read"
    );
    assert!(
        matches!(read, Err(StorageError::Timeout { .. })),
        "the registered read stops: {read:?}"
    );
}

#[test]
fn a_cancel_during_a_read_stops_it() {
    let conn = connection();
    let (control, scope) = scope("read_stops");
    // The cancel lands while the read is registered and before its statement starts, so only
    // the progress handler can stop it: an interrupt with no statement running is cleared when
    // the next statement starts.
    let mut interrupted = false;
    let read = scope.run(&conn, || {
        control.cancel(STOP_REQUEST);
        interrupted_long_read(&conn, &mut interrupted)
    });
    assert!(
        interrupted,
        "SQLite interrupted the cancelled read's statement"
    );
    assert!(
        matches!(read, Err(StorageError::Timeout { .. })),
        "the cancelled read stops: {read:?}"
    );
    assert_eq!(
        long_read(&conn).unwrap(),
        ROWS,
        "the slot is cleared after the read"
    );
}

#[test]
fn an_unwinding_read_clears_its_registration() {
    let conn = connection();
    let (control, scope) = scope("unwind");
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scope.run(&conn, || -> StorageResult<()> {
            panic!("injected read panic")
        })
    }));
    assert!(unwound.is_err());
    control.cancel(STOP_REQUEST);
    assert!(
        !progress_stop(conn.id()),
        "after the unwind the handler stops nothing on the connection"
    );
    assert!(
        !scope.cleanup_failed(),
        "a clean unwind leaves the connection reusable"
    );
}

#[tokio::test]
async fn an_unwinding_read_whose_clear_fails_quarantines() {
    scope_test_read_cleanup_failure(async {
        let conn = connection();
        let (_control, scope) = scope("unwind_clear_failure");
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scope.run(&conn, || -> StorageResult<()> {
                panic!("injected read panic")
            })
        }));
        assert!(unwound.is_err());
        assert!(
            scope.cleanup_failed(),
            "a failed clear on the unwind path marks the connection for quarantine"
        );
    })
    .await;
}

#[test]
fn a_cancel_during_a_traversal_inside_a_read_scope_stops_it() {
    let conn = connection();
    let (control, scope) = scope("traversal_inside_read");
    let mut interrupted = false;
    let read = scope.run(&conn, || {
        with_traversal_stop(
            &conn,
            || false,
            || {
                control.cancel(STOP_REQUEST);
                interrupted_long_read(&conn, &mut interrupted)
            },
        )
    });
    assert!(
        interrupted,
        "SQLite interrupted the traversal's statement on the enclosing read's cancel"
    );
    assert!(
        matches!(read, Err(StorageError::Timeout { .. })),
        "the enclosing read's cancel stops the traversal's statement: {read:?}"
    );
}

#[test]
fn a_cancel_after_a_traversal_returns_still_stops_the_enclosing_read() {
    let conn = connection();
    let (control, scope) = scope("after_traversal");
    let mut interrupted = false;
    let read = scope.run(&conn, || {
        assert_eq!(
            with_traversal_stop(&conn, || false, || long_read(&conn))?,
            ROWS
        );
        control.cancel(STOP_REQUEST);
        interrupted_long_read(&conn, &mut interrupted)
    });
    assert!(
        interrupted,
        "SQLite interrupted the read's statement after the traversal returned"
    );
    assert!(
        matches!(read, Err(StorageError::Timeout { .. })),
        "the read registration survives the traversal: {read:?}"
    );
}
