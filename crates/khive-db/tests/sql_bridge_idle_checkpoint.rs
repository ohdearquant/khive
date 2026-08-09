//! Long-lived bridge-session WAL regression for #1460 and #1812.
//!
//! Idle MCP sessions retain their cached read-only connections, but no active
//! statement, transaction, or reader permit. The central ADR-091 checkpointer
//! must therefore keep making complete PASSIVE progress even when the number
//! of retained sessions exceeds `max_readers`; the WAL must reuse a bounded
//! amount of space rather than growing once per write/checkpoint cycle.

use std::sync::Arc;

use khive_db::checkpoint::{checkpoint_once, TruncateState};
use khive_db::{CheckpointConfig, ConnectionPool, PoolConfig, SqlBridge};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess as _, SqlReader as _};

const SESSION_COUNT: usize = 8;
const WRITE_CYCLES: i64 = 4;
const ROWS_PER_CYCLE: i64 = 96;

fn wal_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut path = db_path.as_os_str().to_owned();
    path.push("-wal");
    path.into()
}

#[tokio::test]
async fn multiple_long_lived_idle_bridge_sessions_allow_bounded_checkpoint_progress() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("idle_bridge_checkpoint.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            max_readers: 2,
            write_queue_enabled: Some(true),
            checkout_timeout: std::time::Duration::from_millis(50),
            ..PoolConfig::default()
        })
        .expect("file-backed pool"),
    );

    // #1848 makes the central task the only routine checkpoint owner. Keep
    // autocheckpoint disabled on this writer so progress below is attributable
    // to `checkpoint_once`, never to a threshold-crossing commit.
    let writer = pool.writer().expect("fixture writer");
    writer
        .conn()
        .execute_batch(
            "PRAGMA wal_autocheckpoint = 0; \
             CREATE TABLE bridge_writes \
             (cycle INTEGER NOT NULL, row_no INTEGER NOT NULL, payload BLOB NOT NULL);",
        )
        .expect("initialize WAL fixture");

    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut sessions = Vec::with_capacity(SESSION_COUNT);
    for _ in 0..SESSION_COUNT {
        let mut session = bridge.reader().await.expect("open bridge session");
        let count = session
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM bridge_writes".into(),
                params: vec![],
                label: Some("idle_bridge_session_probe".into()),
            })
            .await
            .expect("complete the session's one-shot read");
        assert!(matches!(count, Some(SqlValue::Integer(0))));
        sessions.push(session);
    }
    assert_eq!(
        sessions.len(),
        SESSION_COUNT,
        "retained idle sessions must not consume the two-reader active-operation budget"
    );

    let checkpoint_conn = pool
        .open_standalone_writer()
        .expect("dedicated checkpoint connection");
    let checkpoint_config = CheckpointConfig {
        // This regression observes the central task's ordinary PASSIVE path;
        // full TRUNCATE is neither necessary nor a zero-reader-window crutch.
        truncate_high_water_pages: u64::MAX,
        ..CheckpointConfig::default()
    };
    let mut truncate_state = TruncateState::default();
    let payload = vec![0x5Au8; 8 * 1024];
    let mut first_cycle_wal_bytes = None;

    for cycle in 0..WRITE_CYCLES {
        writer
            .conn()
            .execute_batch("BEGIN IMMEDIATE")
            .expect("begin fixture write cycle");
        for row_no in 0..ROWS_PER_CYCLE {
            writer
                .conn()
                .execute(
                    "INSERT INTO bridge_writes (cycle, row_no, payload) VALUES (?1, ?2, ?3)",
                    rusqlite::params![cycle, row_no, payload.as_slice()],
                )
                .expect("append fixture row");
        }
        writer
            .conn()
            .execute_batch("COMMIT")
            .expect("commit fixture write cycle");

        checkpoint_once(
            &pool,
            &checkpoint_conn,
            &checkpoint_config,
            &mut truncate_state,
        )
        .expect("central PASSIVE checkpoint must run with idle sessions retained");

        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = checkpoint_conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .expect("observe checkpoint progress");
        assert_eq!(busy, 0, "PASSIVE checkpoint unexpectedly reported busy");
        assert!(log_frames > 0, "write cycle produced no WAL frames");
        assert_eq!(
            checkpointed_frames, log_frames,
            "idle long-lived sessions pinned checkpoint progress in cycle {cycle}: \
             log={log_frames}, checkpointed={checkpointed_frames}"
        );

        let wal_bytes = std::fs::metadata(wal_path(&path))
            .expect("WAL exists after committed writes")
            .len();
        let first = *first_cycle_wal_bytes.get_or_insert(wal_bytes);
        assert!(
            wal_bytes <= first.saturating_mul(2),
            "WAL did not remain bounded under idle multi-session load: \
             first_cycle={first} bytes, cycle_{cycle}={wal_bytes} bytes"
        );
    }

    // Keep all connections alive through every checkpoint assertion: dropping
    // them earlier would reduce this to a short-lived-reader test and miss the
    // production session-lifetime shape from #1460/#1812.
    assert_eq!(sessions.len(), SESSION_COUNT);
}
