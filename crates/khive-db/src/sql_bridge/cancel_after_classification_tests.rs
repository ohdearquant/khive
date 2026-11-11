//! A cancel that arrives after raw SQL is classified as a write never interrupts it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_storage::types::SqlStatement;
use khive_storage::{StorageCapability, StorageError};

use super::reader_capability::execute_query_interruptibly;
use crate::test_hooks::TestHooks;

const ROWS: i64 = 300_000;
const LONG_WRITE: &str = "INSERT INTO t(x) WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL \
                          SELECT x + 1 FROM c WHERE x < 300000) SELECT x FROM c";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_after_a_write_is_classified_does_not_interrupt_it() {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let worker_saw = Arc::clone(&saw_cancel);
    let call = khive_storage::scope_request_read_cancellation(cancel_rx, async move {
        crate::read_cancellation::run_interruptible_read(
            StorageCapability::Sql,
            "classified_write",
            move |scope| {
                let conn = crate::ProgressConn::new(crate::test_hooks::registered(
                    rusqlite::Connection::open_in_memory().unwrap(),
                ))
                .unwrap();
                conn.execute_batch("CREATE TABLE t(x INTEGER)").unwrap();
                let stopped = scope.stop_probe();
                let mut sent = false;
                // The first progress callback of the write sends the cancel and waits until the
                // request has observed it, so the cancel lands while the write is running.
                conn.test_progress_handler(Some(move || {
                    if !sent {
                        sent = true;
                        cancel_tx.send(true).unwrap();
                        let started = Instant::now();
                        while !stopped() && started.elapsed() < Duration::from_secs(5) {
                            std::thread::yield_now();
                        }
                        worker_saw.store(stopped(), Ordering::Release);
                    }
                    false
                }))
                .unwrap();
                execute_query_interruptibly(
                    scope,
                    &conn,
                    &SqlStatement::new(LONG_WRITE, vec![]),
                    "classified_write",
                    false,
                    true,
                )?;
                conn.query_row("SELECT count(*) FROM t", [], |row| row.get::<_, i64>(0))
                    .map_err(|error| {
                        StorageError::driver(StorageCapability::Sql, "classified_write", error)
                    })
            },
        )
        .await
    });
    let result = tokio::time::timeout(Duration::from_secs(30), tokio::spawn(call))
        .await
        .expect("the classified write completes")
        .unwrap();
    assert!(
        saw_cancel.load(Ordering::Acquire),
        "the cancel landed while the write ran"
    );
    assert_eq!(result.ok(), Some(ROWS), "the write ran to completion");
}
