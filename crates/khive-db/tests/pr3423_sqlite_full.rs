//! Exercises disk-full escalation through the real writer task and NDJSON sink.
//! This integration target has one test because the sink is process-global.

use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use khive_db::{writer_task, ConnectionPool, PoolConfig};
use khive_storage::error::WriterTaskRequestState;
use khive_storage::{StorageCapability, StorageError};
use rusqlite::ErrorCode;
use serde_json::Value;

fn storage_error(error: rusqlite::Error) -> StorageError {
    StorageError::driver(StorageCapability::Sql, "review_full_probe", error)
}

fn injected(code: i32) -> StorageError {
    storage_error(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(code),
        Some("synthetic classification control".to_string()),
    ))
}

fn read_complete_rows(path: &Path) -> Vec<Value> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("cannot read isolated sink: {error}"),
    };
    // A writer may currently be appending the last line. Parse only complete
    // newline-terminated records; malformed complete records remain errors.
    let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    bytes[..=end]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).expect("complete NDJSON record"))
        .collect()
}

fn count_kind(rows: &[Value], kind: &str) -> usize {
    rows.iter()
        .filter(|row| row["kind"].as_str() == Some(kind))
        .count()
}

async fn wait_rows(path: &Path, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows = read_complete_rows(path);
        if predicate(&rows) {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "sink/setup did not satisfy condition: {rows:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn auto_rollback_full_keeps_cause_specific_escalation() {
    // Environment is set before this binary creates a pool or the sink.
    // This file deliberately has only one test; do not merge it into a target
    // that has already initialized the global sink with different settings.
    let dir = tempfile::tempdir().expect("isolated fixture");
    let sink_dir = dir.path().join("sink");
    fs::create_dir(&sink_dir).unwrap();
    std::env::set_var("KHIVE_DB_FREE_SPACE_FLOOR_BYTES", "0");
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", &sink_dir);
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_HEARTBEAT_MS", "100");

    let pool = ConnectionPool::new(PoolConfig {
        path: Some(dir.path().join("review.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("pool opens");
    // Explicit public spawn: queue-policy auto-installation is not under test.
    let handle = writer_task::spawn(&pool, 8).expect("real writer task");
    let sink_path = sink_dir.join(format!("writer_timeouts.{}.ndjson", std::process::id()));
    wait_rows(&sink_path, |rows| count_kind(rows, "startup") == 1).await;

    // Positive control: a typed SQLITE_FULL that does not itself end SQLite's
    // transaction must already reach the new escalation hook.
    let result: Result<(), StorageError> = handle
        .send(|_| Err(injected(rusqlite::ffi::SQLITE_FULL)))
        .await;
    assert!(matches!(
        result,
        Err(StorageError::WriterTaskRequestFailed { .. })
    ));
    let control = wait_rows(&sink_path, |rows| count_kind(rows, "sqlite_full") >= 1).await;
    assert_eq!(
        count_kind(&control, "sqlite_full"),
        1,
        "control must emit once"
    );
    assert_eq!(count_kind(&control, "writer_task_retirement"), 0);

    // Negative control: busy is not disk full. Its result also must not
    // poison this otherwise clean task.
    let busy: Result<(), StorageError> = handle
        .send(|_| Err(injected(rusqlite::ffi::SQLITE_BUSY)))
        .await;
    assert!(matches!(
        busy,
        Err(StorageError::WriterTaskRequestFailed { .. })
    ));
    let one = handle
        .send(|conn| {
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(storage_error)
        })
        .await
        .expect("task remains usable after controls");
    assert_eq!(one, 1);

    // Set the page limit on the SAME persistent connection that will run the
    // failing statement. The limit is read back; a misspelled or ignored
    // PRAGMA cannot become a false behavioral success.
    let (pages, cap) = handle
        .send_top_level(|conn| {
            conn.execute_batch("CREATE TABLE review_payload(bytes BLOB NOT NULL)")
                .map_err(storage_error)?;
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |row| row.get(0))
                .map_err(storage_error)?;
            let cap: i64 = conn
                .query_row(
                    &format!("PRAGMA max_page_count = {}", pages + 1),
                    [],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            Ok((pages, cap))
        })
        .await
        .expect("bounded SQLite fixture");
    assert_eq!(cap, pages + 1);

    let (observed_tx, observed_rx) = mpsc::channel();
    let failed: Result<(), StorageError> = handle
        .send(move |conn| {
            let was_in_transaction = !conn.is_autocommit();
            match conn.execute("INSERT INTO review_payload VALUES (zeroblob(1048576))", []) {
                Ok(_) => {
                    let _ = observed_tx.send((false, was_in_transaction, conn.is_autocommit()));
                    Ok(())
                }
                Err(error) => {
                    let full = matches!(
                        &error,
                        rusqlite::Error::SqliteFailure(code, _) if code.code == ErrorCode::DiskFull
                    );
                    let _ = observed_tx.send((full, was_in_transaction, conn.is_autocommit()));
                    Err(storage_error(error))
                }
            }
        })
        .await;

    let (raw_full, was_in_transaction, auto_rolled_back) = observed_rx
        .try_recv()
        .expect("operation must actually run and expose its raw SQLite state");
    assert!(
        raw_full,
        "fixture must produce actual SQLITE_FULL, not setup/panic/other error"
    );
    assert!(
        was_in_transaction,
        "production task must have begun its transaction"
    );
    assert!(
        auto_rolled_back,
        "fixture must actually exercise SQLite automatic rollback"
    );
    assert!(
        matches!(
            failed,
            Err(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
                ..
            })
        ),
        "this scoped telemetry test preserves the inspected retirement policy"
    );

    // Retirement is enqueued by this same task AFTER its per-request error
    // hook. Unlike a periodic heartbeat, it is a FIFO barrier for that hook.
    let final_rows = wait_rows(&sink_path, |rows| {
        count_kind(rows, "writer_task_retirement") == 1
    })
    .await;

    drop(handle);
    if let Some(join) = pool.take_writer_task_join() {
        tokio::time::timeout(Duration::from_secs(10), join)
            .await
            .expect("writer task exits")
            .expect("writer task join");
    }

    // One event per SQLITE_FULL source. Busy must add none, and duplicate
    // emission also fails this assertion.
    assert_eq!(
        count_kind(&final_rows, "sqlite_full"),
        2,
        "automatic-rollback SQLITE_FULL lost its cause-specific escalation: {final_rows:?}"
    );
    assert_eq!(
        count_kind(&final_rows, "sink_error_summary"),
        0,
        "sink drops invalidate evidence"
    );
}
