use super::*;
use crate::pool::PoolConfig;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess as _, SqlReader as _};

#[tokio::test]
async fn top_level_wal_checkpoint_ends_the_active_pin_run() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("top_level_checkpoint.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE t (value INTEGER); INSERT INTO t VALUES (1)")
            .unwrap();
    }

    let _task_guard =
        crate::checkpoint::CheckpointRunTaskGuard::start(&pool, std::time::Duration::from_secs(60));
    crate::checkpoint::record_checkpoint_run_result(&pool, Some((0, 20, 10)));
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    bridge
        .writer()
        .await
        .unwrap()
        .execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)
        .await
        .unwrap();

    assert_eq!(
        crate::checkpoint::checkpoint_run_status(&pool),
        crate::checkpoint::CheckpointRunStatus::NoObservation
    );
}

include!("sql_bridge_atomic_serialization_tests.rs");
include!("sql_bridge_write_admission_tests.rs");

fn database_tx_view(pool: &ConnectionPool) -> khive_storage::tx_registry::TxOriginFilter {
    match pool.origin() {
        khive_storage::tx_registry::TxOrigin::Database(identity) => {
            khive_storage::tx_registry::TxOriginFilter::Secondary(identity)
        }
        other => panic!("expected a file-backed database origin, got {other:?}"),
    }
}

struct NotifyOnDrop(Arc<tokio::sync::Notify>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

fn blocking_non_interrupting_progress_gate(
    conn: &rusqlite::Connection,
) -> (
    Arc<tokio::sync::Notify>,
    Arc<std::sync::Barrier>,
    Arc<tokio::sync::Notify>,
) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let callback_entered = Arc::clone(&entered);
    let release = Arc::new(std::sync::Barrier::new(2));
    let callback_release = Arc::clone(&release);
    let completed = Arc::new(tokio::sync::Notify::new());
    let notify_on_drop = NotifyOnDrop(Arc::clone(&completed));
    let blocked_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_blocked_once = Arc::clone(&blocked_once);
    conn.progress_handler(
        1_000,
        Some(move || {
            let _keep_until_connection_drop = &notify_on_drop;
            if !callback_blocked_once.swap(true, std::sync::atomic::Ordering::SeqCst) {
                callback_entered.notify_one();
                callback_release.wait();
                // The gate deliberately stalls but never asks SQLite to
                // abort. It proves completion-preserving SQLite work ignores
                // request-read cancellation and finishes normally.
                return false;
            }
            false
        }),
    )
    .unwrap();
    (entered, release, completed)
}

fn progress_gate_statement() -> SqlStatement {
    SqlStatement {
        sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 999\
                  ) SELECT SUM(value) FROM rows"
            .into(),
        params: vec![],
        label: None,
    }
}

fn slow_insert_statement() -> SqlStatement {
    SqlStatement {
        sql: "INSERT INTO cancellation_write_probe(value) \
                  WITH RECURSIVE rows(value) AS (\
                  SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 10000\
                  ) SELECT value FROM rows"
            .into(),
        params: vec![],
        label: Some("non-interruptible-write-probe".into()),
    }
}

fn passive_checkpoint(conn: &rusqlite::Connection) -> (i64, i64, i64) {
    conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .unwrap()
}

fn deliberately_slow_read_statement() -> SqlStatement {
    SqlStatement {
        sql: "WITH RECURSIVE numbers(value) AS (\
                  SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
                  ) SELECT SUM(a.value * b.value * c.value) \
                  FROM numbers AS a CROSS JOIN numbers AS b CROSS JOIN numbers AS c"
            .into(),
        params: vec![],
        label: Some("read-cancellation-progress-probe".into()),
    }
}

async fn wait_for_progress(probe: &std::sync::atomic::AtomicUsize) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while probe.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("slow SQLite statement never reached its progress callback");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pooled_stats_count_cancellation_stops_scan_and_releases_reader() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("stats-count-cancel.db")),
            max_readers: 1,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    // Expand a small file-backed fixture into a long scan, preserving the
    // handler's exact outer COUNT rather than substituting a SUM query.
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE count_fixture(n INTEGER PRIMARY KEY); \
             WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000) \
             INSERT INTO count_fixture SELECT x FROM n; \
             CREATE VIEW events AS SELECT 'local' AS namespace, 'knowledge.learn' AS verb \
             FROM count_fixture a CROSS JOIN count_fixture b CROSS JOIN count_fixture c;",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let query = tokio::spawn(crate::scope_test_read_progress(
        Arc::clone(&progress),
        crate::scope_request_read_cancellation(cancel_rx, async move {
            let result = reader.query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM events WHERE namespace = ?1 AND verb LIKE 'knowledge.%'".into(),
                    params: vec![SqlValue::Text("local".into())],
                    label: Some("knowledge.stats.event_count".into()),
                }).await;
            (reader, result)
        }),
    ));
    wait_for_progress(progress.as_ref()).await;
    assert!(
        !query.is_finished(),
        "COUNT must still be scanning before cancellation"
    );
    let started = std::time::Instant::now();
    let grace = crate::read_cancellation::sqlite_interrupt_grace_from_env();
    cancel_tx.send(true).unwrap();
    let (mut reader, result) = tokio::time::timeout(grace, query)
        .await
        .expect("COUNT did not settle within the interrupt grace")
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(StorageError::Timeout { .. })),
        "{result:?}"
    );
    assert_eq!(
        pool.available_readers(),
        1,
        "COUNT retained the sole pooled reader"
    );
    let stopped = progress.load(std::sync::atomic::Ordering::SeqCst);
    let next = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM count_fixture".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(next, Some(SqlValue::Integer(1000))));
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::SeqCst),
        stopped,
        "cancelled callback leaked into the next borrower"
    );
    eprintln!(
        "stats_count_cancel_ms={} grace_ms={}",
        elapsed.as_secs_f64() * 1000.0,
        grace.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_before_reader_checkout_is_prompt_and_executes_no_statement() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cancel_before_checkout.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_secs(5),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE checkout_cancel_probe(value INTEGER NOT NULL); \
                 INSERT INTO checkout_cancel_probe VALUES (0);",
        )
        .unwrap();
    let held_reader = pool.reader().expect("hold the sole pooled reader");
    // Production-shaped file-backed raw-SQL reads must use the same pool
    // admission as typed stores. Before ADR-165 Slice 2, `reader()` opened
    // a standalone connection and this held pooled reader was invisible.
    // A plain SELECT (not a DML/RETURNING statement) is used here: the
    // reader capability's admission gate now refuses non-read statement
    // shapes before checkout is even attempted, so a DML probe would
    // never reach the cancellation-race path this test exercises.
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut waiting_reader = bridge.reader().await.unwrap();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let waiting = tokio::spawn(crate::scope_request_read_cancellation(
        cancel_rx,
        async move {
            waiting_reader
                .query_row(SqlStatement {
                    sql: "SELECT value FROM checkout_cancel_probe".into(),
                    params: vec![],
                    label: Some("must-not-run-after-cancelled-checkout".into()),
                })
                .await
        },
    ));

    tokio::task::yield_now().await;
    cancel_tx.send(true).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
        .await
        .expect("cancelled reader checkout waited for the five-second pool timeout")
        .expect("checkout task panicked");
    // Cancellation is NOT an admission wait: it must stay the non-admission
    // Timeout, never the retryable AdmissionTimeout, so a cancelled request
    // does not signal clients to retry into a saturated pool.
    assert!(matches!(result, Err(StorageError::Timeout { .. })));

    drop(held_reader);
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let value: i64 = pool
        .reader()
        .unwrap()
        .conn()
        .query_row("SELECT value FROM checkout_cancel_probe", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        value, 0,
        "the probe row must be untouched: nothing else in this test writes to it"
    );
    assert_eq!(
        pool.available_readers(),
        1,
        "reader checkout leaked a permit"
    );
}

/// Reproduction probe for the reader-capability admission gap: `sqlite3_stmt_readonly`
/// (the only check the pre-fix bridge applied to raw SQL reaching the
/// pooled reader) returns `true` for `ATTACH`, configuration `PRAGMA`s,
/// and `CREATE TEMP TABLE` — none of which write the main database file,
/// but all of which leave connection-local state that `reset_reader_connection`
/// did not catch. Each probe below is submitted through the reader
/// capability exactly as an untrusted caller would reach it
/// (`SqlBridge::reader` -> `SqlReader::query_all`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_reader_capability_admission_of_state_mutating_statements() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_admission_probe.db")),
        max_readers: 2,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE reader_admission_probe(value INTEGER NOT NULL);")
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let probes: [&str; 5] = [
        "ATTACH DATABASE ':memory:' AS x",
        "PRAGMA writable_schema=ON",
        "PRAGMA busy_timeout=1",
        "PRAGMA cache_size(-64)",
        "CREATE TEMP TABLE t(x)",
    ];
    let mut admitted = Vec::new();
    for probe in probes {
        let mut reader = bridge.reader().await.unwrap();
        let result = reader
            .query_all(SqlStatement {
                sql: probe.into(),
                params: vec![],
                label: Some("reader-admission-probe".into()),
            })
            .await;
        admitted.push((probe, result.is_ok()));
    }
    eprintln!("reader capability admission per probe: {admitted:#?}");
    for (probe, was_admitted) in &admitted {
        assert!(
            !was_admitted,
            "reader capability must refuse {probe:?}; the pre-fix bridge wrongly admitted it"
        );
    }

    // Control: an ordinary SELECT and an allow-listed introspection
    // PRAGMA must still be admitted through the same reader capability.
    let controls: [&str; 2] = [
        "SELECT value FROM reader_admission_probe",
        "PRAGMA table_info(reader_admission_probe)",
    ];
    for control in controls {
        let mut reader = bridge.reader().await.unwrap();
        let result = reader
            .query_all(SqlStatement {
                sql: control.into(),
                params: vec![],
                label: Some("reader-admission-control".into()),
            })
            .await;
        assert!(
            result.is_ok(),
            "reader capability must still admit {control:?}: {result:?}"
        );
    }
}

/// A `WITH` clause is not by itself a read shape: SQLite accepts
/// `WITH ... INSERT/UPDATE/DELETE`, including with `RETURNING`, and the
/// admission classifier must walk past the common-table-expression list
/// to the main statement's own head keyword rather than admitting every
/// statement that merely starts with `WITH`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_reader_capability_admission_of_with_cte_dml_statements() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cte_dml_admission_probe.db")),
        max_readers: 2,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE cte_dml_admission_probe(id INTEGER PRIMARY KEY, value INTEGER NOT NULL);",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let probes: [&str; 4] = [
        "WITH x(v) AS (SELECT 1) \
             INSERT INTO cte_dml_admission_probe(id, value) SELECT 1, v FROM x",
        "WITH x(v) AS (SELECT 1) \
             INSERT INTO cte_dml_admission_probe(id, value) SELECT 1, v FROM x RETURNING id",
        "WITH x(v) AS (SELECT 0) \
             UPDATE cte_dml_admission_probe SET value = value + (SELECT v FROM x) WHERE id = 1",
        "WITH x(v) AS (SELECT 1) \
             DELETE FROM cte_dml_admission_probe WHERE id = (SELECT v FROM x)",
    ];
    let mut admitted = Vec::new();
    for probe in probes {
        let mut reader = bridge.reader().await.unwrap();
        let result = reader
            .query_all(SqlStatement {
                sql: probe.into(),
                params: vec![],
                label: Some("cte-dml-admission-probe".into()),
            })
            .await;
        eprintln!("{probe:?} -> {result:?}");
        admitted.push((probe, result.is_ok()));
    }
    eprintln!("WITH-DML reader capability admission per probe: {admitted:#?}");
    for (probe, was_admitted) in &admitted {
        assert!(
            !was_admitted,
            "reader capability must refuse {probe:?}; it is a WITH-prefixed write, not a read"
        );
    }

    // Controls: ordinary and recursive read-only CTEs, including a
    // multi-CTE chain and a CTE body containing string literals with
    // commas (the shape produced by the graph traversal compiler), must
    // still be admitted.
    let controls: [&str; 3] = [
        "WITH x(v) AS (SELECT 1) SELECT v FROM x",
        "WITH RECURSIVE n(v) AS (VALUES(0) UNION ALL SELECT v + 1 FROM n WHERE v < 3) \
             SELECT v FROM n",
        "WITH a(v) AS (SELECT 1), b(v) AS (SELECT v FROM a WHERE ',' || 'x' NOT LIKE '%,%') \
             SELECT v FROM b",
    ];
    for control in controls {
        let mut reader = bridge.reader().await.unwrap();
        let result = reader
            .query_all(SqlStatement {
                sql: control.into(),
                params: vec![],
                label: Some("cte-dml-admission-control".into()),
            })
            .await;
        assert!(
            result.is_ok(),
            "reader capability must still admit {control:?}: {result:?}"
        );
    }
}

/// A CTE name may be a SQLite-quoted identifier — double-quoted,
/// backtick-quoted, or bracket-quoted — and is not restricted to the
/// unquoted `[A-Za-z0-9_]+` shape `skip_common_table_expressions` uses to
/// recognize an ordinary keyword. A quoted name containing parentheses
/// must still be admitted when the statement underneath the CTE list is
/// a read, and refused when it is a write, exactly like an unquoted name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_reader_capability_admission_of_quoted_cte_names() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_quoted_cte_admission_probe.db")),
        max_readers: 2,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE quoted_cte_admission_probe(id INTEGER PRIMARY KEY, value INTEGER NOT NULL);",
            )
            .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let controls: [&str; 3] = [
        "WITH \"my (cte)\"(v) AS (SELECT 1) SELECT v FROM \"my (cte)\"",
        "WITH `my (cte)`(v) AS (SELECT 1) SELECT v FROM `my (cte)`",
        "WITH [my (cte)](v) AS (SELECT 1) SELECT v FROM [my (cte)]",
    ];
    for control in controls {
        let mut reader = bridge.reader().await.unwrap();
        let result = reader
            .query_all(SqlStatement {
                sql: control.into(),
                params: vec![],
                label: Some("quoted-cte-admission-control".into()),
            })
            .await;
        assert!(
            result.is_ok(),
            "reader capability must admit a quoted CTE name in {control:?}: {result:?}"
        );
    }

    let write_probe = "WITH \"my (cte)\"(v) AS (SELECT 1) \
             INSERT INTO quoted_cte_admission_probe(id, value) SELECT 1, v FROM \"my (cte)\"";
    let mut reader = bridge.reader().await.unwrap();
    let result = reader
        .query_all(SqlStatement {
            sql: write_probe.into(),
            params: vec![],
            label: Some("quoted-cte-admission-probe".into()),
        })
        .await;
    assert!(
        result.is_err(),
        "reader capability must refuse a write statement under a quoted CTE name: {result:?}"
    );
}

/// Regression for the in-memory reader path: `PoolBackedReader` (the
/// `SqlBridge::reader` implementation used whenever `is_file_backed` is
/// `false`) computed its admission classification differently from
/// `SqliteReader`, always passing `None` for the cached transaction-control
/// read instead of classifying the statement. That made the deferred-read
/// snapshot pair (`BEGIN DEFERRED` ... `COMMIT`) a multi-call reader
/// already relies on to hold one consistent view across several pooled
/// checkouts — see `khive-pack-memory`'s fresh-tail leg — indistinguishable
/// from an unrecognized statement head and refused outright. Mutating
/// statements must still be refused through this path exactly as they are
/// through the file-backed one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_backed_reader_admits_deferred_read_transaction_control() {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE pool_backed_reader_probe(value INTEGER NOT NULL); \
                 INSERT INTO pool_backed_reader_probe VALUES (1);",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let mut reader = bridge.reader().await.unwrap();

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: Some("pool-backed-reader-snapshot-begin".into()),
        })
        .await
        .expect("BEGIN DEFERRED must be admitted through the pool-backed reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT value FROM pool_backed_reader_probe".into(),
            params: vec![],
            label: Some("pool-backed-reader-snapshot-read".into()),
        })
        .await
        .expect("a read inside the admitted snapshot must succeed");
    assert_eq!(rows.len(), 1);
    reader
        .query_all(SqlStatement {
            sql: "COMMIT".into(),
            params: vec![],
            label: Some("pool-backed-reader-snapshot-commit".into()),
        })
        .await
        .expect("COMMIT must be admitted through the pool-backed reader");

    let integrity = reader
        .query_scalar(SqlStatement {
            sql: "PRAGMA integrity_check".into(),
            params: vec![],
            label: Some("pool-backed-reader-integrity-check".into()),
        })
        .await
        .expect("PRAGMA integrity_check must be admitted through the pool-backed reader");
    assert!(matches!(integrity, Some(SqlValue::Text(ref s)) if s.eq_ignore_ascii_case("ok")));

    for probe in [
        "ATTACH DATABASE ':memory:' AS x",
        "PRAGMA writable_schema=ON",
        "CREATE TEMP TABLE t(x)",
        "SAVEPOINT nested_snapshot",
        // A `WITH` head is not itself a read shape: SQLite also allows
        // `WITH ... INSERT/UPDATE/DELETE`, with or without `RETURNING`,
        // and the pool-backed route must refuse it exactly like the
        // file-backed one does.
        "WITH x(v) AS (SELECT 99) INSERT INTO pool_backed_reader_probe(value) SELECT v FROM x",
        "WITH x(v) AS (SELECT 99) \
             INSERT INTO pool_backed_reader_probe(value) SELECT v FROM x RETURNING value",
        "WITH x(v) AS (SELECT 0) \
             UPDATE pool_backed_reader_probe SET value = value + (SELECT v FROM x)",
        "WITH x(v) AS (SELECT 1) \
             DELETE FROM pool_backed_reader_probe WHERE value = (SELECT v FROM x)",
    ] {
        let mut reader = bridge.reader().await.unwrap();
        let result = reader
            .query_all(SqlStatement {
                sql: probe.into(),
                params: vec![],
                label: Some("pool-backed-reader-admission-probe".into()),
            })
            .await;
        assert!(
            result.is_err(),
            "pool-backed reader capability must refuse {probe:?}; got {result:?}"
        );
    }

    // The refused probes above must not have left a stray write behind:
    // exactly the one row inserted before the probe loop is still there.
    let rows = bridge
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT value FROM pool_backed_reader_probe".into(),
            params: vec![],
            label: Some("pool-backed-reader-post-probe-read".into()),
        })
        .await
        .expect("a plain read must still work after every probe above was refused");
    assert_eq!(
        rows.len(),
        1,
        "a refused WITH-DML probe must not have committed a row"
    );
}

/// An explicit deferred read-transaction span
/// (`begin_read_snapshot`/`end_read_snapshot` in `khive-pack-memory`)
/// that never reaches its own `COMMIT` — because the caller hit an error
/// mid-span and gave up on the read, or was cancelled — must still leave
/// the pool's connection rolled back to autocommit before it can be
/// reused. Before the span retained a real owned connection guard across
/// `.await` points, every `PoolBackedReader` call drew its own fresh
/// checkout: `BEGIN` and `COMMIT` could land on different checkouts, and
/// an abandoned span between them returned the connection to the pool
/// while still inside an open transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_deferred_read_transaction_span_is_rolled_back_before_reuse() {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE abandoned_span_probe(value INTEGER NOT NULL); \
                 INSERT INTO abandoned_span_probe VALUES (1);",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);

    {
        let mut reader = bridge.reader().await.unwrap();
        reader
            .query_all(SqlStatement {
                sql: "BEGIN DEFERRED".into(),
                params: vec![],
                label: Some("abandoned-span-begin".into()),
            })
            .await
            .expect("the span must open");
        let failed = reader
            .query_all(SqlStatement {
                sql: "SELECT value FROM abandoned_span_probe_missing_table".into(),
                params: vec![],
                label: Some("abandoned-span-failing-read".into()),
            })
            .await;
        assert!(
            failed.is_err(),
            "the probe read against a nonexistent table must fail"
        );
        // `reader`, and the still-open transaction guard it owns, is
        // dropped here without ever reaching `COMMIT`/`ROLLBACK`.
    }

    let writer = pool.writer().unwrap();
    assert!(
        writer.conn().is_autocommit(),
        "an abandoned deferred-read span must be rolled back before its connection \
             returns to service"
    );
    drop(writer);

    let mut reader = bridge.reader().await.unwrap();
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT value FROM abandoned_span_probe".into(),
            params: vec![],
            label: Some("abandoned-span-post-recovery-read".into()),
        })
        .await
        .expect("a fresh checkout must read normally after the abandoned span");
    assert_eq!(
        rows.len(),
        1,
        "the original row must be intact; the abandoned span must not have committed \
             anything"
    );
}

/// A pooled-reader checkout that exhausts `checkout_timeout` WITHOUT any
/// cancellation is a genuine admission wait and must surface as the
/// retryable AdmissionTimeout. Before the fix, `reader_until`'s
/// pool-exhausted error was mapped to `StorageError::Driver`, so a
/// saturated pooled read stayed a non-retryable driver failure and the new
/// AdmissionTimeout branch (reachable only for `Ok(None)` cancellation) was
/// dead for real timeouts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pooled_reader_checkout_timeout_is_a_retryable_admission_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_admission_timeout.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(200),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE reader_admission_probe(value INTEGER NOT NULL); \
                 INSERT INTO reader_admission_probe VALUES (0);",
        )
        .unwrap();
    // Hold the sole pooled reader so the contending checkout cannot succeed
    // and must run `checkout_timeout` to exhaustion — no cancellation.
    let held_reader = pool.reader().expect("hold the sole pooled reader");

    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut contender = bridge.reader().await.unwrap();
    let blocked = contender
        .query_row(SqlStatement {
            sql: "SELECT value FROM reader_admission_probe".into(),
            params: vec![],
            label: Some("reader-admission-timeout-probe".into()),
        })
        .await;
    assert!(
        matches!(blocked, Err(StorageError::AdmissionTimeout { .. })),
        "an exhausted pooled-reader checkout must be a retryable AdmissionTimeout; got {blocked:?}"
    );

    drop(held_reader);
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(
        pool.available_readers(),
        1,
        "reader checkout leaked a permit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_read_interrupts_sqlite_releases_permit_and_stops_work() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_abandoned_read.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(500),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = SqliteReader {
        handle: Some(
            open_explicit_read_transaction_handle(Arc::clone(&pool))
                .await
                .unwrap(),
        ),
        pool: Arc::clone(&pool),
        poisoned: false,
    };
    let mut contender = bridge.reader().await.unwrap();
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let progress_in_scope = Arc::clone(&progress);

    let query = tokio::spawn(crate::scope_test_read_progress(
        progress_in_scope,
        async move { reader.query_all(deliberately_slow_read_statement()).await },
    ));
    wait_for_progress(progress.as_ref()).await;
    query.abort();
    assert!(matches!(query.await, Err(error) if error.is_cancelled()));

    tokio::time::timeout(
        std::time::Duration::from_millis(500),
        contender.query_row(SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        }),
    )
    .await
    .expect("abandoned SQLite statement did not return the sole reader promptly")
    .expect("reader probe failed after cancellation");

    let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::SeqCst),
        stopped_at,
        "SQLite progress kept advancing after the abandoned request returned its reader"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_deadline_interrupts_statement_without_outer_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_request_deadline.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(500),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let result = crate::scope_test_read_progress(
        Arc::clone(&progress),
        crate::scope_request_read_deadline(std::time::Duration::from_millis(25), async move {
            reader.query_all(deliberately_slow_read_statement()).await
        }),
    )
    .await;
    assert!(
        matches!(result, Err(StorageError::Timeout { .. })),
        "deadline must surface as a typed timeout, got {result:?}"
    );

    let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
    assert!(stopped_at > 0, "deadline test never exercised SQLite work");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::SeqCst),
        stopped_at,
        "deadline returned while SQLite kept consuming work"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_handler_cleanup_failure_discards_pooled_connection() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cleanup_failure.db")),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let pool_for_read = Arc::clone(&pool);
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let result =
        crate::read_cancellation::scope_test_read_cleanup_failure(crate::scope_test_read_progress(
            Arc::clone(&progress),
            crate::read_cancellation::run_interruptible_read(
                StorageCapability::Sql,
                "cleanup_failure_probe",
                move |scope| {
                    let mut guard = pool_for_read.reader().map_err(|error| {
                        StorageError::driver(StorageCapability::Sql, "cleanup_failure_probe", error)
                    })?;
                    scope.run_pooled_reader(&mut guard, |conn| {
                        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                            .map_err(|error| map_rusqlite_err(error, "cleanup_failure_probe"))
                    })
                },
            ),
        ))
        .await;
    assert!(
        matches!(result, Err(StorageError::Internal(ref message)) if message.contains("clear failure")),
        "injected cleanup failure must be surfaced; got {result:?}"
    );
    assert_eq!(
        pool.available_readers(),
        1,
        "discard must install a replacement"
    );

    let calls_after_failed_read = progress.load(std::sync::atomic::Ordering::SeqCst);
    let guard = pool.reader().unwrap();
    let sum: i64 = guard
        .conn()
        .query_row(
            "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x + 1 FROM n WHERE x < 10000) \
                 SELECT sum(x) FROM n",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sum, 50_005_000);
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::SeqCst),
        calls_after_failed_read,
        "a connection whose handler could not be cleared was reused"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_pooled_reader_quarantines_cleanup_failure_during_unwind() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("raw_reader_unwind_cleanup.db")),
            max_readers: 1,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    let worker_pool = Arc::clone(&pool);
    let result = crate::read_cancellation::scope_test_read_cleanup_failure(
        crate::read_cancellation::run_interruptible_read(
            StorageCapability::Sql,
            "raw_reader_unwind_cleanup",
            move |scope| {
                let mut guard = worker_pool.reader().map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "raw_reader_unwind_cleanup", error)
                })?;
                scope.with_pooled_reader(&mut guard, |conn| {
                    scope.run(conn, || -> khive_storage::types::StorageResult<()> {
                        panic!("injected raw reader panic after progress registration")
                    })
                })
            },
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "blocking panic must surface as a join error"
    );
    assert_eq!(
        pool.available_readers(),
        pool.max_readers(),
        "unwind cleanup failure must close and replace the raw pooled reader"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_pooled_writer_retires_cleanup_failure_during_unwind() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
    let worker_pool = Arc::clone(&pool);
    let result = crate::read_cancellation::scope_test_read_cleanup_failure(
        crate::read_cancellation::run_interruptible_read(
            StorageCapability::Sql,
            "raw_writer_unwind_cleanup",
            move |scope| {
                let guard = worker_pool.try_writer().map_err(|error| {
                    StorageError::driver(StorageCapability::Sql, "raw_writer_unwind_cleanup", error)
                })?;
                scope.with_pooled_writer(&worker_pool, &guard, |conn| {
                    scope.run(conn, || -> khive_storage::types::StorageResult<()> {
                        panic!("injected raw writer panic after progress registration")
                    })
                })
            },
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "blocking panic must surface as a join error"
    );
    assert!(
        pool.try_writer().is_err(),
        "unwind cleanup failure must retire the raw pooled writer"
    );
}

#[test]
fn query_row_converts_only_the_first_matching_row() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let statement = SqlStatement {
        sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 99\
                  ) SELECT value FROM rows ORDER BY value"
            .into(),
        params: vec![],
        label: None,
    };

    ROW_CONVERSIONS.with(|count| count.set(0));
    let row = execute_query_row(&conn, &statement).unwrap().unwrap();

    assert!(matches!(row.get("value"), Some(SqlValue::Integer(0))));
    ROW_CONVERSIONS.with(|count| assert_eq!(count.get(), 1));
}

#[test]
fn query_page_bounds_owned_rows_before_full_materialization() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let statement = SqlStatement {
        sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 99\
                  ) SELECT value FROM rows ORDER BY value"
            .into(),
        params: vec![],
        label: None,
    };

    ROW_CONVERSIONS.with(|count| count.set(0));
    let rows = execute_query_page(
        &conn,
        &statement,
        &PageRequest {
            offset: 40,
            limit: 3,
        },
    )
    .unwrap();

    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0].get("value"), Some(SqlValue::Integer(40))));
    assert!(matches!(rows[2].get("value"), Some(SqlValue::Integer(42))));
    ROW_CONVERSIONS.with(|count| assert_eq!(count.get(), 3));
}

#[test]
fn query_page_zero_limit_converts_no_rows_but_still_validates_sql() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let statement = SqlStatement {
        sql: "WITH RECURSIVE rows(value) AS (\
                  SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 99\
                  ) SELECT value FROM rows ORDER BY value"
            .into(),
        params: vec![],
        label: None,
    };

    ROW_CONVERSIONS.with(|count| count.set(0));
    let rows = execute_query_page(
        &conn,
        &statement,
        &PageRequest {
            offset: 0,
            limit: 0,
        },
    )
    .unwrap();

    assert!(rows.is_empty());
    ROW_CONVERSIONS.with(|count| assert_eq!(count.get(), 0));

    let invalid = SqlStatement {
        sql: "SELECT FROM WHERE".into(),
        params: vec![],
        label: None,
    };
    assert!(
        execute_query_page(
            &conn,
            &invalid,
            &PageRequest {
                offset: 0,
                limit: 0
            }
        )
        .is_err(),
        "a zero-limit page must still fail on invalid SQL at prepare time"
    );
}

#[test]
fn cached_writer_prepare_preserves_the_single_statement_boundary() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    assert!(matches!(
        prepare_cached_sql_statement(&conn, "SELECT 1; SELECT 2"),
        Err(rusqlite::Error::MultipleStatement)
    ));
}

#[tokio::test]
async fn queue_backed_execute_reuses_the_persistent_connection_statement_cache() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("sql_bridge_writer_cache.db")),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE writer_cache_test (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .unwrap();

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("file-backed queue-enabled pool must expose its writer task");
    let prepare_count = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&prepare_count);
    writer_task
        .send_top_level(move |conn| {
            conn.authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Insert { table_name } if table_name == "writer_cache_test"
                ) {
                    hook_count.fetch_add(1, Ordering::SeqCst);
                }
                Authorization::Allow
            }))
            .map_err(|error| map_rusqlite_err(error, "test.install_authorizer"))
        })
        .await
        .unwrap();

    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();
    for id in [1, 2] {
        khive_storage::SqlWriter::execute(
            &mut *writer,
            SqlStatement {
                sql: "INSERT INTO writer_cache_test (id, value) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(id), SqlValue::Text(format!("value-{id}"))],
                label: None,
            },
        )
        .await
        .unwrap();
    }

    assert_eq!(
        prepare_count.load(Ordering::SeqCst),
        1,
        "the second identical execute on the writer task's persistent connection must reuse \
             the cached SQLite statement instead of compiling it again"
    );
    writer_task
        .send_top_level(|conn| {
            conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .map_err(|error| map_rusqlite_err(error, "test.remove_authorizer"))
        })
        .await
        .unwrap();
}

#[test]
fn inline_execute_batch_prepares_each_statement_once() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE single_prepare_test (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
    )
    .unwrap();
    let prepare_count = Arc::new(AtomicUsize::new(0));
    let hook_count = Arc::clone(&prepare_count);
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(
            context.action,
            AuthAction::Insert { table_name } if table_name == "single_prepare_test"
        ) {
            hook_count.fetch_add(1, Ordering::SeqCst);
        }
        Authorization::Allow
    }))
    .unwrap();

    let mut writer = InlineWriter {
        event_rows: None,
        conn: &conn as *const rusqlite::Connection,
    };
    let affected = block_on_sync(khive_storage::SqlWriter::execute_batch(
        &mut writer,
        vec![SqlStatement {
            sql: "INSERT INTO single_prepare_test (id, value) VALUES (?1, ?2)".into(),
            params: vec![SqlValue::Integer(1), SqlValue::Text("once".into())],
            label: None,
        }],
    ))
    .expect("InlineWriter operations must resolve on their first poll")
    .expect("valid batch must execute");

    assert_eq!(affected, 1);
    assert_eq!(
        prepare_count.load(Ordering::SeqCst),
        1,
        "classification and execution must share one prepared statement handle"
    );
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
}

#[test]
fn inline_execute_batch_preserves_schema_dependencies_between_statements() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let mut writer = InlineWriter {
        event_rows: None,
        conn: &conn as *const rusqlite::Connection,
    };

    let affected = block_on_sync(khive_storage::SqlWriter::execute_batch(
        &mut writer,
        vec![
            SqlStatement {
                sql: "CREATE TABLE dependent_prepare_test (id INTEGER PRIMARY KEY)".into(),
                params: vec![],
                label: None,
            },
            SqlStatement {
                sql: "INSERT INTO dependent_prepare_test (id) VALUES (1)".into(),
                params: vec![],
                label: None,
            },
        ],
    ))
    .expect("InlineWriter operations must resolve on their first poll")
    .expect("a later statement must be prepared after its prerequisite schema change");

    assert_eq!(affected, 1);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM dependent_prepare_test", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn pool_backed_query_page_beyond_result_set_returns_empty() {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE page_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL);\
                     INSERT INTO page_test (id, val) VALUES (1, 'a'), (2, 'b'), (3, 'c');",
            )
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), false);

    let statement = || SqlStatement {
        sql: "SELECT val FROM page_test ORDER BY id".into(),
        params: vec![],
        label: None,
    };

    let mut reader = bridge.reader().await.unwrap();
    let page = reader
        .query_page(
            statement(),
            PageRequest {
                offset: 1,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert!(matches!(page[0].get("val"), Some(SqlValue::Text(v)) if v == "b"));
    assert!(matches!(page[1].get("val"), Some(SqlValue::Text(v)) if v == "c"));

    let empty = reader
        .query_page(
            statement(),
            PageRequest {
                offset: 99,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        empty.is_empty(),
        "offset past the last row must return an empty page, got {empty:?}"
    );
    drop(reader);

    let mut writer = bridge.writer().await.unwrap();
    let empty = writer
        .query_page(
            statement(),
            PageRequest {
                offset: 99,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        empty.is_empty(),
        "offset past the last row must return an empty page, got {empty:?}"
    );
}

#[tokio::test]
async fn file_bridge_scopes_reader_permits_to_operations_and_caps_writer_handles() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_handle_cap.db")),
        write_queue_enabled: Some(false),
        max_readers: 2,
        checkout_timeout: std::time::Duration::from_millis(20),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let second_bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut retained_readers = Vec::new();
    for expected in 0..3 {
        let mut reader = second_bridge.reader().await.unwrap();
        let value = reader
            .query_scalar(SqlStatement {
                sql: format!("SELECT {expected}"),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert!(matches!(value, Some(SqlValue::Integer(value)) if value == expected));
        retained_readers.push(reader);
    }
    assert_eq!(retained_readers.len(), 3);

    let mut additional_reader = bridge.reader().await.unwrap();
    let page = additional_reader
        .query_page(
            SqlStatement {
                sql: "WITH RECURSIVE rows(value) AS (\
                          SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 9\
                          ) SELECT value FROM rows ORDER BY value"
                    .into(),
                params: vec![],
                label: None,
            },
            PageRequest {
                offset: 7,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert!(matches!(page[0].get("value"), Some(SqlValue::Integer(7))));
    assert!(matches!(page[1].get("value"), Some(SqlValue::Integer(8))));
    drop((additional_reader, retained_readers));

    let writer = bridge.writer().await.unwrap();
    let writer_error = match second_bridge.writer().await {
        Ok(_) => panic!("a second live writer handle exceeded the one-handle cap"),
        Err(error) => error,
    };
    assert!(matches!(
        writer_error,
        StorageError::AdmissionTimeout { ref operation, .. }
            if operation.as_ref() == "sql_bridge.writer_handle"
    ));
    drop(writer);
    let writer_after_release = bridge.writer().await.unwrap();
    drop(writer_after_release);
}

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn file_bridge_attributes_ordinary_pool_reads_and_explicit_transaction_exception() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_routes.db")),
            max_readers: 1,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();

    assert_eq!(
        pool.reader_acquisition_snapshot(),
        crate::pool::ReaderAcquisitionSnapshot {
            reader_admission_capacity: 1,
            available_reader_admission_slots: 1,
            ..crate::pool::ReaderAcquisitionSnapshot::default()
        },
        "constructing or retaining an idle raw-SQL reader must open nothing"
    );

    let value = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(value, Some(SqlValue::Integer(1))));
    let ordinary = pool.reader_acquisition_snapshot();
    assert_eq!(ordinary.pooled_checkouts, 1);
    assert_eq!(ordinary.completed_pooled_checkouts, 1);
    assert_eq!(ordinary.standalone_opens, 0);

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("the documented explicit transaction exception opens");
    let begun = pool.reader_acquisition_snapshot();
    assert_eq!(begun.pooled_checkouts, 1);
    assert_eq!(begun.standalone_opens, 1);

    reader
        .query_scalar(SqlStatement {
            sql: "SELECT 2".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("transaction query reuses its one exceptional connection");
    reader
        .query_all(SqlStatement {
            sql: "COMMIT".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("COMMIT closes the explicit transaction exception");
    let committed = pool.reader_acquisition_snapshot();
    assert_eq!(committed.reader_admission_capacity, 1);
    assert_eq!(committed.available_reader_admission_slots, 1);
    assert_eq!(committed.acquisitions, begun.acquisitions);
    assert_eq!(committed.pooled_checkouts, begun.pooled_checkouts);
    assert_eq!(committed.standalone_opens, begun.standalone_opens);
    assert_eq!(
        committed.completed_pooled_checkouts, begun.completed_pooled_checkouts,
        "queries and COMMIT inside one explicit transaction must not acquire another reader"
    );

    reader
        .query_scalar(SqlStatement {
            sql: "SELECT 3".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("ordinary traffic returns to the reader pool after COMMIT");
    let after = pool.reader_acquisition_snapshot();
    assert_eq!(after.pooled_checkouts, 2);
    assert_eq!(after.completed_pooled_checkouts, 2);
    assert_eq!(after.standalone_opens, 1);
    assert_eq!(after.active_pooled_checkouts, 0);
}

#[tokio::test]
async fn explicit_reader_open_timeout_is_visible_without_a_standalone_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("sql_bridge_reader_open_timeout.db")),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(20),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut logical_reader = bridge.reader().await.unwrap();
    let held = pool.reader().expect("hold the shared reader budget");

    let blocked = logical_reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &blocked,
            Err(StorageError::Timeout { operation })
                if operation.as_ref() == "sql_bridge.reader_open"
        ),
        "the compatible explicit-transaction open phase must stay visible; got {blocked:?}"
    );

    let snapshot = pool.reader_acquisition_snapshot();
    assert_eq!(snapshot.checkout_timeouts, 1);
    assert_eq!(snapshot.pooled_checkouts, 1);
    assert_eq!(snapshot.standalone_opens, 0);
    assert_eq!(snapshot.active_pooled_checkouts, 1);
    assert_eq!(snapshot.available_reader_admission_slots, 0);
    drop(held);
}

/// A pooled raw-SQL read that SQLite refuses with SQLITE_BUSY after the
/// busy handler gives up is counted per pool. WAL readers are never
/// blocked by a writer, so the fixture uses a rollback-journal database,
/// where a connection holding an exclusive lock refuses every other reader.
#[tokio::test]
async fn pooled_raw_sql_read_counts_busy_handler_timeouts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sql_bridge_busy_timeouts.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path.clone()),
            wal_mode: false,
            write_queue_enabled: Some(false),
            busy_timeout: std::time::Duration::from_millis(50),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE busy_fixture (id INTEGER PRIMARY KEY)")
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();
    let count = || SqlStatement {
        sql: "SELECT count(*) FROM busy_fixture".into(),
        params: vec![],
        label: None,
    };

    // Control: with no lock held the read succeeds and nothing is counted.
    let value = reader.query_scalar(count()).await.unwrap();
    assert!(matches!(value, Some(SqlValue::Integer(0))), "{value:?}");
    assert_eq!(pool.reader_acquisition_snapshot().busy_timeouts, 0);

    let holder = rusqlite::Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let refused = reader.query_scalar(count()).await.unwrap_err();
    assert!(
        matches!(
            &refused,
            StorageError::Driver { source, .. }
                if source
                    .downcast_ref::<rusqlite::Error>()
                    .and_then(|error| error.sqlite_error_code())
                    == Some(rusqlite::ErrorCode::DatabaseBusy)
        ),
        "a read behind an exclusive lock must surface SQLITE_BUSY, got {refused:?}"
    );
    let snapshot = pool.reader_acquisition_snapshot();
    assert_eq!(snapshot.busy_timeouts, 1);
    assert_eq!(
        snapshot.checkout_timeouts, 0,
        "a busy-handler refusal after checkout is not a checkout timeout"
    );
    holder.execute_batch("ROLLBACK").unwrap();
}

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn cached_read_transaction_retains_one_permit_until_commit_or_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_tx_control.db")),
        write_queue_enabled: Some(true),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(20),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin = pool.origin();
    let origin_view = database_tx_view(&pool);
    let unrelated_view = khive_storage::tx_registry::TxOriginFilter::Secondary(
        khive_storage::tx_registry::DbIdentity::new("unrelated-sql-bridge.db"),
    );
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();
    let mut contender = bridge.reader().await.unwrap();

    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "an idle cached reader must not register a transaction"
    );

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("BEGIN DEFERRED must open an admitted cached-reader snapshot");
    let opened = khive_storage::tx_registry::oldest_for(&origin_view)
        .expect("successful BEGIN must register the cached-reader transaction");
    assert_eq!(opened.label.as_deref(), Some(CACHED_READ_TRANSACTION_LABEL));
    assert_eq!(opened.origin, origin);
    assert!(
        khive_storage::tx_registry::oldest_for(&unrelated_view).is_none(),
        "the read transaction must be attributed only to its own backend"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        0,
        "the successful BEGIN must retain its operation permit"
    );

    let value = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 7".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("a query inside the admitted transaction must reuse its retained permit");
    assert!(matches!(value, Some(SqlValue::Integer(7))));
    assert_eq!(
        khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("queries must retain the transaction registration")
            .id,
        opened.id,
        "queries inside the transaction must retain the original span"
    );

    let blocked = contender
        .query_scalar(SqlStatement {
            sql: "SELECT 8".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &blocked,
            Err(StorageError::AdmissionTimeout { operation, .. })
                if operation.as_ref() == "query_row"
        ),
        "a second logical read must contend with the admitted transaction \
             and fail at the bounded pooled-admission stage; got {blocked:?}"
    );

    reader
        .query_all(SqlStatement {
            sql: "COMMIT".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("COMMIT must close the admitted cached-reader snapshot");
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "COMMIT must deregister after SQLite returns to autocommit"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "COMMIT may release the permit only after autocommit is restored"
    );
    let value = contender
        .query_scalar(SqlStatement {
            sql: "SELECT 8".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("the contender must run after COMMIT releases admission");
    assert!(matches!(value, Some(SqlValue::Integer(8))));

    reader
        .query_all(SqlStatement {
            sql: "BEGIN TRANSACTION".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("plain deferred BEGIN TRANSACTION must also be admitted");
    let reopened = khive_storage::tx_registry::oldest_for(&origin_view)
        .expect("the second successful BEGIN must register a fresh span");
    assert_ne!(reopened.id, opened.id);
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
    let nested = reader
        .query_all(SqlStatement {
            sql: "ROLLBACK TO stale_snapshot".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(&nested, Err(StorageError::InvalidInput { .. })),
        "ROLLBACK TO requires unsupported nested state; got {nested:?}"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        0,
        "rejected nested control must not release the still-live transaction admission"
    );
    assert_eq!(
        khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("ROLLBACK TO rejection must retain the live span")
            .id,
        reopened.id
    );
    let savepoint = reader
        .query_all(SqlStatement {
            sql: "SAVEPOINT nested_snapshot".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(&savepoint, Err(StorageError::InvalidInput { .. })),
        "SAVEPOINT must be rejected inside the admitted transaction; got {savepoint:?}"
    );
    assert_eq!(
        khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("SAVEPOINT rejection must retain the live span")
            .id,
        reopened.id
    );
    reader
        .query_all(SqlStatement {
            sql: "ROLLBACK".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("ROLLBACK must close the admitted cached-reader snapshot");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "full ROLLBACK must deregister after SQLite returns to autocommit"
    );
}

#[tokio::test]
async fn failed_cached_reader_begin_does_not_register_a_transaction() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    fn deny_begin(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Begin,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_failed_begin.db")),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin_view = database_tx_view(&pool);
    let conn = open_standalone_reader(&pool).unwrap();
    conn.authorizer(Some(deny_begin)).unwrap();
    let mut reader = SqliteReader {
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: None,
            read_transaction_slot: None,
        }),
        pool: Arc::clone(&pool),
        poisoned: false,
    };

    let begin = reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(begin.is_err(), "the authorizer must reject BEGIN");
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "a failed BEGIN must never enter the transaction registry"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "a failed BEGIN must return the operation permit"
    );
}

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn failed_cached_reader_rollback_deregisters_only_when_connection_is_discarded() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_failed_rollback.db")),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin_view = database_tx_view(&pool);
    let conn = open_standalone_reader(&pool).unwrap();
    let mut reader = SqliteReader {
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: None,
            read_transaction_slot: None,
        }),
        pool: Arc::clone(&pool),
        poisoned: false,
    };

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("BEGIN must establish the registered transaction");
    let opened = khive_storage::tx_registry::oldest_for(&origin_view)
        .expect("the admitted transaction must be registered");
    reader
        .handle
        .as_ref()
        .expect("reader must retain its connection")
        .conn
        .authorizer(Some(deny_rollback))
        .unwrap();

    let rollback = reader
        .query_all(SqlStatement {
            sql: "ROLLBACK".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(rollback.is_err(), "the authorizer must reject ROLLBACK");
    assert_eq!(
        khive_storage::tx_registry::oldest_for(&origin_view)
            .expect("failed ROLLBACK must retain registry evidence")
            .id,
        opened.id
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        0,
        "failed ROLLBACK must retain reader admission"
    );

    drop(reader);
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "discarding the connection must not leak its registry entry"
    );
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
}

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn cached_read_only_handles_reject_unsupported_transaction_control_without_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(
            dir.path()
                .join("sql_bridge_reader_unsupported_tx_control.db"),
        ),
        write_queue_enabled: Some(true),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut reader = bridge.reader().await.unwrap();
    for (sql, keyword) in [
        ("BEGIN IMMEDIATE", "BEGIN"),
        ("BEGIN EXCLUSIVE", "BEGIN"),
        // Trailing-mode spellings parse in SQLite as a NAMED deferred
        // transaction, but the mode keyword in name position reads as
        // lock intent; the classifier must refuse rather than launder
        // them into a deferred start the cached reader would then hold.
        ("BEGIN TRANSACTION IMMEDIATE", "BEGIN"),
        ("BEGIN TRANSACTION EXCLUSIVE", "BEGIN"),
        ("BEGIN DEFERRED TRANSACTION trailing", "BEGIN"),
        // Quoted/bracketed tails tokenize as no identifier at all, so a
        // classifier that stops at the tokenizer's `None` reads them as
        // an accepted form's end. They must be refused exactly like the
        // bare-word spellings.
        ("BEGIN TRANSACTION \"IMMEDIATE\"", "BEGIN"),
        ("BEGIN TRANSACTION [IMMEDIATE]", "BEGIN"),
        ("BEGIN TRANSACTION `IMMEDIATE`", "BEGIN"),
        ("BEGIN TRANSACTION 'IMMEDIATE'", "BEGIN"),
        ("BEGIN \"DEFERRED\"", "BEGIN"),
        ("BEGIN; COMMIT", "BEGIN"),
        ("START TRANSACTION", "START"),
        ("COMMIT", "COMMIT"),
    ] {
        let rejected = reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await;
        assert!(
            matches!(
                &rejected,
                Err(StorageError::InvalidInput { message, .. })
                    if message.contains(keyword)
            ),
            "unsupported cached-reader control {sql:?} must fail closed; got {rejected:?}"
        );
        assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
    }

    let mut queue_backed_writer = bridge.writer().await.unwrap();
    let rejected = queue_backed_writer
        .query_all(SqlStatement {
            sql: "SAVEPOINT stale_snapshot".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &rejected,
            Err(StorageError::InvalidInput {
                operation,
                message,
                ..
            }) if operation.as_ref() == "writer.query_all"
                && message.contains("transaction control")
                && message.contains("SAVEPOINT")
        ),
        "a queue-backed writer without an explicit read transaction must reject nested \
             transaction control; got {rejected:?}"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "queue-backed rejection must leave the operation permit available"
    );
    let value = queue_backed_writer
        .query_scalar(SqlStatement {
            sql: "SELECT 8".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("transaction-control rejection must not consume the queue-backed handle");
    assert!(matches!(value, Some(SqlValue::Integer(8))));

    queue_backed_writer
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("queue-backed cached reader must share explicit read-transaction admission");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
    queue_backed_writer
        .query_all(SqlStatement {
            sql: "END".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("END must release queue-backed cached-reader admission");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
}

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn dropping_cached_reader_transaction_closes_snapshot_before_releasing_permit() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_tx_drop.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(20),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin_view = database_tx_view(&pool);
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();
    let mut contender = bridge.reader().await.unwrap();

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("begin admitted transaction");
    reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("materialize read snapshot");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
        "the live snapshot must remain registered until handle drop"
    );

    drop(reader);
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "handle drop must close SQLite before deregistering the snapshot"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "dropping the handle must close its transaction before returning admission"
    );
    contender
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("a new operation must run after the transactional handle drops");
}

/// #1846 regression: a cached-reader explicit read transaction that is
/// never explicitly finished (a stuck/leaked caller that keeps reusing
/// the handle without COMMIT/ROLLBACK) would otherwise pin the WAL
/// snapshot open for as long as the caller kept calling in. Without the
/// age check this reddens: the second `query_all` would return the row
/// materialized inside the still-open transaction instead of an error,
/// and `tx_registry::oldest_for` would keep reporting the same span
/// open past `read_tx_max_age`.
#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn expired_cached_reader_transaction_is_rolled_back_on_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_tx_max_age.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(20),
        read_tx_max_age: std::time::Duration::from_millis(20),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin_view = database_tx_view(&pool);
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("begin admitted transaction");
    reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("materialize read snapshot");
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
        "the open transaction must be registered before it ages out"
    );

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;

    let evictions_before = crate::checkpoint::read_tx_max_age_evictions();
    let error = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect_err("reusing a transaction past read_tx_max_age must be refused");
    assert!(
        error.is_retryable(),
        "an evicted-transaction error must be retryable so the caller can open a fresh \
             snapshot: {error}"
    );
    match &error {
        StorageError::ReadTransactionAgeEvicted {
            operation,
            max_age_secs,
        } => {
            assert_eq!(operation.as_ref(), "query_all");
            assert_eq!(
                *max_age_secs, 0,
                "a 20ms read_tx_max_age truncates to 0 whole seconds"
            );
        }
        other => panic!(
            "a clean age-triggered rollback must surface the dedicated \
                 ReadTransactionAgeEvicted variant, not a generic classification: {other:?}"
        ),
    }
    assert_eq!(
        crate::checkpoint::read_tx_max_age_evictions(),
        evictions_before + 1,
        "the eviction must be counted in the #1846 diagnostics gauge"
    );
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "the expired transaction must be rolled back and deregistered rather than \
             continuing to pin the WAL snapshot"
    );

    reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("the handle must remain usable for a fresh autocommit read after eviction");
}

/// #1846 follow-up: the age-eviction branch
/// has a rollback-failure path distinct from
/// `failed_cached_reader_rollback_deregisters_only_when_connection_is_discarded`
/// above (which covers an explicit caller-issued `ROLLBACK`, not the
/// age-triggered cleanup rollback). When SQLite denies the age-triggered
/// `ROLLBACK`, the branch must still discard the poisoned connection,
/// deregister the expired transaction span, and release the reader
/// admission permit rather than leaking either.
#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn expired_cached_reader_transaction_rollback_denial_discards_connection_and_releases_admission(
) {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(
            dir.path()
                .join("sql_bridge_reader_tx_max_age_rollback_denied.db"),
        ),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(20),
        read_tx_max_age: std::time::Duration::from_millis(20),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin_view = database_tx_view(&pool);
    let conn = open_standalone_reader(&pool).unwrap();
    let mut reader = SqliteReader {
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: None,
            read_transaction_slot: None,
        }),
        pool: Arc::clone(&pool),
        poisoned: false,
    };

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("begin admitted transaction");
    reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("materialize read snapshot");
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
        "the open transaction must be registered before it ages out"
    );

    reader
        .handle
        .as_ref()
        .expect("reader must retain its connection")
        .conn
        .authorizer(Some(deny_rollback))
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(40)).await;

    let evictions_before = crate::checkpoint::read_tx_max_age_evictions();
    let error = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect_err("a denied rollback on an expired transaction must surface an error");
    assert!(
        error.is_retryable(),
        "even a failed cleanup rollback must remain classified retryable so callers open a \
             fresh handle: {error}"
    );
    match &error {
        StorageError::ReadTransactionAgeEvictionCleanupFailed {
            operation,
            max_age_secs,
            message,
        } => {
            assert_eq!(operation.as_ref(), "query_all");
            assert_eq!(
                *max_age_secs, 0,
                "a 20ms read_tx_max_age truncates to 0 whole seconds"
            );
            assert!(
                message.contains("rollback failed"),
                "the failure must be attributable to the denied ROLLBACK, not silent \
                     success: {message}"
            );
        }
        other => panic!(
            "a denied cleanup rollback must surface the dedicated \
                 ReadTransactionAgeEvictionCleanupFailed variant, not a generic Transaction \
                 error the caller cannot machine-detect: {other:?}"
        ),
    }
    assert_eq!(
        crate::checkpoint::read_tx_max_age_evictions(),
        evictions_before + 1,
        "the eviction attempt must still be counted even though cleanup failed"
    );
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_none(),
        "a denied rollback must discard the connection and deregister the expired \
             transaction span rather than leaking it"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "discarding the poisoned connection must release the reader admission slot"
    );

    let reuse = reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM sqlite_schema".into(),
            params: vec![],
            label: None,
        })
        .await;
    let message = match reuse {
        Err(StorageError::Pool { message, .. }) => message,
        other => panic!(
            "reusing this discarded reader must fail loudly with 'connection already \
                 consumed' rather than silently reopening; got {other:?}"
        ),
    };
    assert!(
        message.contains("connection already consumed"),
        "expected the discarded reader's reuse error to name the pinned failure; got \
             {message:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(tx_registry)]
async fn cancelled_cached_reader_transaction_releases_guards_after_connection_closes() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_tx_drop_cancel.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(50),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let origin_view = database_tx_view(&pool);
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = SqliteReader {
        handle: Some(
            open_explicit_read_transaction_handle(Arc::clone(&pool))
                .await
                .unwrap(),
        ),
        pool: Arc::clone(&pool),
        poisoned: false,
    };
    let mut contender = bridge.reader().await.unwrap();

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("begin admitted transaction");
    assert!(
        khive_storage::tx_registry::oldest_for(&origin_view).is_some(),
        "the explicit transaction must be registered before cancellation"
    );
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);

    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let query = tokio::spawn(crate::scope_test_read_progress(
        Arc::clone(&progress),
        async move { reader.query_all(deliberately_slow_read_statement()).await },
    ));
    wait_for_progress(progress.as_ref()).await;
    query.abort();
    assert!(matches!(query.await, Err(error) if error.is_cancelled()));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while khive_storage::tx_registry::oldest_for(&origin_view).is_some()
            || pool.sql_bridge_reader_slots().available_permits() != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection cleanup leaked transaction evidence or reader admission");
    contender
        .query_all(SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("admission must recover after the cancelled connection closes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(tx_registry)]
async fn cancelled_cached_reader_rolls_back_releases_wal_and_clears_handler() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_tx_request_cancel.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(500),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let writer = open_standalone_writer(&pool).unwrap();
    writer
        .execute_batch(
            "CREATE TABLE snapshot_probe(id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO snapshot_probe(value) VALUES ('seed');",
        )
        .unwrap();
    let mut reader = bridge.reader().await.unwrap();
    let mut contender = bridge.reader().await.unwrap();

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("begin admitted transaction");
    reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM snapshot_probe".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("materialize a real WAL snapshot");
    writer
        .execute_batch(
            "WITH RECURSIVE rows(value) AS (\
                 SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 100\
                 ) INSERT INTO snapshot_probe(value) SELECT printf('row-%d', value) FROM rows;",
        )
        .unwrap();
    let (_, log_before, checkpointed_before) = passive_checkpoint(&writer);
    assert!(
        log_before > checkpointed_before,
        "the explicit reader snapshot must pin WAL frames before cancellation"
    );

    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let query = tokio::spawn(crate::scope_test_read_progress(
        Arc::clone(&progress),
        crate::scope_request_read_cancellation(cancel_rx, async move {
            let result = reader.query_all(deliberately_slow_read_statement()).await;
            (reader, result)
        }),
    ));
    wait_for_progress(progress.as_ref()).await;
    cancel_tx.send(true).unwrap();
    let (mut reader, result) = tokio::time::timeout(std::time::Duration::from_secs(1), query)
        .await
        .expect("interrupted explicit read transaction did not stop promptly")
        .unwrap();
    assert!(
        matches!(result, Err(StorageError::Timeout { .. })),
        "request cancellation must surface as a typed timeout; got {result:?}"
    );
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);

    let (_, log_after, checkpointed_after) = passive_checkpoint(&writer);
    assert_eq!(
        log_after, checkpointed_after,
        "cancellation must release the explicit reader's WAL snapshot"
    );
    contender
        .query_all(SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("the sole reader permit must be reusable after rollback");

    let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
    reader
        .query_all(SqlStatement {
            sql: "WITH RECURSIVE rows(value) AS (\
                      SELECT 0 UNION ALL SELECT value + 1 FROM rows WHERE value < 10000\
                      ) SELECT SUM(value) FROM rows"
                .into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("same connection must remain usable after handler teardown");
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::SeqCst),
        stopped_at,
        "the cancelled request's progress callback bled into the next borrower"
    );
}

/// A statement that blocks inside a single SQLite VM step for longer
/// than [`crate::read_cancellation::DEFAULT_SQLITE_INTERRUPT_GRACE_MS`].
/// Unlike a recursive CTE (interrupt-checked every 1,000 VM
/// instructions, so it stops promptly), a UDF call is one opcode: SQLite
/// cannot observe the interrupt flag until the call returns. This is the
/// only way to deterministically force a worker past the grace window
/// rather than merely past `wait_for_progress`.
fn register_khive_test_slow_udf(
    conn: &rusqlite::Connection,
    sleep_ms: u64,
    started: Arc<std::sync::atomic::AtomicBool>,
) {
    conn.create_scalar_function(
        "khive_test_slow_udf",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        move |_| {
            started.store(true, std::sync::atomic::Ordering::Release);
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
            Ok(0i64)
        },
    )
    .unwrap();
}

async fn wait_for_flag(flag: &std::sync::atomic::AtomicBool) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !flag.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("slow UDF never started");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_read_past_grace_recovers_admission_after_bounded_join() {
    // Regression for the PR #1897 review blocker: a `spawn_blocking`
    // read worker that outlives `KHIVE_SQLITE_INTERRUPT_GRACE_MS` must
    // not be treated as reaped just because the async side detached
    // from it. This forces the worker past grace with a slow UDF (so
    // the interrupt genuinely cannot be observed mid-call) and asserts
    // that admission and the WAL snapshot are only reported recovered
    // once the real worker has actually joined.
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_grace_exceeded.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(2_000),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let writer = open_standalone_writer(&pool).unwrap();
    writer
        .execute_batch(
            "CREATE TABLE grace_probe(id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
                 INSERT INTO grace_probe(value) VALUES ('seed');",
        )
        .unwrap();

    let mut reader = SqliteReader {
        handle: Some(
            open_explicit_read_transaction_handle(Arc::clone(&pool))
                .await
                .unwrap(),
        ),
        pool: Arc::clone(&pool),
        poisoned: false,
    };
    let mut contender = SqliteReader {
        handle: Some(
            open_explicit_read_transaction_handle(Arc::clone(&pool))
                .await
                .unwrap(),
        ),
        pool: Arc::clone(&pool),
        poisoned: false,
    };
    let udf_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    register_khive_test_slow_udf(
        &reader.handle.as_ref().unwrap().conn,
        900,
        Arc::clone(&udf_started),
    );

    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("begin admitted transaction");
    reader
        .query_all(SqlStatement {
            sql: "SELECT * FROM grace_probe".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("materialize a real WAL snapshot");
    writer
        .execute_batch(
            "WITH RECURSIVE rows(value) AS (\
                 SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 100\
                 ) INSERT INTO grace_probe(value) SELECT printf('row-%d', value) FROM rows;",
        )
        .unwrap();
    let (_, log_before, checkpointed_before) = passive_checkpoint(&writer);
    assert!(
        log_before > checkpointed_before,
        "the explicit reader snapshot must pin WAL frames before cancellation"
    );

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let query = tokio::spawn(crate::scope_request_read_cancellation(
        cancel_rx,
        async move {
            let result = reader
                .query_all(SqlStatement {
                    sql: "SELECT khive_test_slow_udf()".into(),
                    params: vec![],
                    label: None,
                })
                .await;
            (reader, result)
        },
    ));
    // Wait for the UDF to actually be running (not just scheduled) so
    // registration has completed and the worker is provably blocked
    // inside SQLite before cancelling — the same proof `wait_for_progress`
    // gives the recursive-CTE tests, since a progress callback (fired
    // between opcodes) never runs during the UDF's own blocking call.
    wait_for_flag(udf_started.as_ref()).await;
    cancel_tx.send(true).unwrap();

    let (mut reader, result) = tokio::time::timeout(std::time::Duration::from_secs(3), query)
        .await
        .expect("a worker that settles within the grace+hard-cap bound must not hang the caller")
        .unwrap();
    assert!(
        matches!(result, Err(StorageError::Timeout { .. })),
        "request cancellation must still surface as a typed timeout even after grace \
             was exceeded; got {result:?}"
    );

    // The response was only returned above because the real worker
    // joined — expected-fail arm: this assertion would fail (permit
    // still 0) under the pre-fix behavior, which detached and returned
    // Timeout at the grace boundary while the worker (and its permit)
    // were still live.
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "the sole reader permit must be visible again once the bounded join completes"
    );

    let (_, log_after, checkpointed_after) = passive_checkpoint(&writer);
    assert_eq!(
        log_after, checkpointed_after,
        "the abandoned explicit read transaction must release its WAL snapshot by the \
             time the caller observes the timeout"
    );

    contender
        .query_all(SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("a fresh reader must be admitted once the zombie worker has settled");

    // The settled connection itself must also be reusable (not
    // quarantined) and must not still be carrying the slow UDF's
    // progress callback into a later borrower.
    reader
        .query_all(SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("the interrupted connection must remain usable after settling");
}

#[tokio::test]
#[serial_test::serial(tx_registry)]
async fn cached_reader_transaction_lifecycle_survives_sqlite_empty_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_prefixed_tx_control.db")),
        write_queue_enabled: Some(true),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut reader = bridge.reader().await.unwrap();

    reader
        .query_all(SqlStatement {
            sql: " ; -- empty statement\n /* leading comment */ \u{feff} BEGIN DEFERRED".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("prefixed BEGIN must enter the admitted transaction state");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 0);
    reader
        .query_all(SqlStatement {
            sql: " /* leading comment */ \u{feff} ; COMMIT".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("prefixed COMMIT must end the admitted transaction state");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);

    let rejected = reader
        .query_all(SqlStatement {
            sql: " ; /* no active transaction */ \u{feff} COMMIT".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &rejected,
            Err(StorageError::InvalidInput {
                operation,
                message,
                ..
            }) if operation.as_ref() == "query_all"
                && message.contains("transaction control")
                && message.contains("COMMIT")
        ),
        "a prefixed COMMIT without an admitted transaction must still fail closed; \
             got {rejected:?}"
    );

    let mut queue_backed_writer = bridge.writer().await.unwrap();
    let rejected = queue_backed_writer
        .query_all(SqlStatement {
            sql: "-- leading comment\n \u{feff} ; /* empty */ SAVEPOINT pinned".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &rejected,
            Err(StorageError::InvalidInput {
                operation,
                message,
                ..
            }) if operation.as_ref() == "writer.query_all"
                && message.contains("transaction control")
                && message.contains("SAVEPOINT")
        ),
        "a queue-backed cached reader must classify transaction control through \
             comments, BOMs, and empty statements; got {rejected:?}"
    );

    let value = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 10".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("prefixed transaction lifecycle must preserve the cached reader");
    assert!(matches!(value, Some(SqlValue::Integer(10))));
}

#[tokio::test]
async fn cached_reader_restores_autocommit_before_releasing_its_operation_permit() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_reader_autocommit.db")),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let conn = open_standalone_reader(&pool).unwrap();
    conn.execute_batch("BEGIN DEFERRED; SELECT * FROM sqlite_schema")
        .unwrap();
    assert!(
        !conn.is_autocommit(),
        "the regression precondition needs a live read transaction"
    );
    let mut reader = SqliteReader {
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: None,
            read_transaction_slot: None,
        }),
        pool: Arc::clone(&pool),
        poisoned: false,
    };

    let rejected = reader
        .query_all(SqlStatement {
            // The stale-state cleanup must take precedence over the
            // ordinary transaction-control rejection. Otherwise an idle
            // snapshot could survive every rejected ROLLBACK attempt.
            sql: "ROLLBACK".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &rejected,
            Err(StorageError::InvalidInput {
                operation,
                message,
                ..
            }) if operation.as_ref() == "query_all"
                && message.contains("outside autocommit")
        ),
        "a cached reader that reaches the boundary outside autocommit must fail closed; \
             got {rejected:?}"
    );
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        1,
        "the permit may be released only after the stale transaction is gone"
    );
    assert!(
        reader.handle.is_none(),
        "the restored connection must close instead of surviving as an idle standalone cache"
    );

    let value = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 9".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("the cleaned reader must remain usable through the pooled route");
    assert!(matches!(value, Some(SqlValue::Integer(9))));
}

#[tokio::test]
async fn standalone_writer_read_preserves_manual_atomic_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_writer_atomic_read.db")),
        write_queue_enabled: Some(false),
        max_readers: 1,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE atomic_read_test \
                     (id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
            )
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let observed = bridge
        .atomic_unit(Box::new(|writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO atomic_read_test (id, value) VALUES (1, 'pending')"
                            .into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                let count = writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM atomic_read_test".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
            })
        }))
        .await
        .expect("manual atomic read must not be mistaken for an idle reader snapshot");
    let observed = match observed.downcast::<Option<SqlValue>>() {
        Ok(observed) => observed,
        Err(_) => panic!("unexpected atomic result type"),
    };
    assert!(matches!(*observed, Some(SqlValue::Integer(1))));

    let mut reader = bridge.reader().await.unwrap();
    let committed = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM atomic_read_test".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(committed, Some(SqlValue::Integer(1))));
}

#[tokio::test]
async fn request_cancellation_preserves_file_backed_manual_atomic_read_and_commit() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("sql_bridge_writer_tx_cancel.db")),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE writer_tx_cancel_probe(\
                 id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    let observed = crate::scope_request_read_cancellation(
        cancel_rx,
        bridge.atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO writer_tx_cancel_probe VALUES (1, 'before')".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                cancel_tx.send(true).unwrap();
                let count = writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM writer_tx_cancel_probe".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO writer_tx_cancel_probe VALUES (2, 'after')".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
            })
        })),
    )
    .await
    .expect("request cancellation must not interrupt an admitted manual write transaction");
    let observed = match observed.downcast::<Option<SqlValue>>() {
        Ok(observed) => observed,
        Err(_) => panic!("unexpected atomic result type"),
    };
    assert!(matches!(*observed, Some(SqlValue::Integer(1))));

    let reader = pool.reader().unwrap();
    let rows: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM writer_tx_cancel_probe", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 2, "both writes around the SELECT must commit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_standalone_writer_transaction_retains_active_reader_admission() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("sql_bridge_writer_tx_admission.db")),
            write_queue_enabled: Some(false),
            max_readers: 1,
            checkout_timeout: std::time::Duration::from_millis(250),
            // The unit lease is held across the gated read below, so it
            // takes a lock namespace of its own instead of the one the
            // other tests in this binary share.
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    let writer_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let (entered, release, _completed) = blocking_non_interrupting_progress_gate(&conn);
    // A manual transaction holds its unit lease for the whole span, as
    // `atomic_unit`'s manual branch does; without it `execute` refuses BEGIN.
    let unit_lease = acquire_unit_lease(Arc::clone(&pool)).await.unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(writer_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: unit_lease,
    };
    khive_storage::SqlWriter::execute(
        &mut writer,
        SqlStatement {
            sql: "BEGIN IMMEDIATE".into(),
            params: vec![],
            label: None,
        },
    )
    .await
    .unwrap();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    cancel_tx.send(true).unwrap();

    let query = tokio::spawn(crate::scope_request_read_cancellation(
        cancel_rx,
        async move {
            let result =
                khive_storage::SqlReader::query_all(&mut writer, progress_gate_statement()).await;
            let rollback = khive_storage::SqlWriter::execute(
                &mut writer,
                SqlStatement {
                    sql: "ROLLBACK".into(),
                    params: vec![],
                    label: None,
                },
            )
            .await;
            (result, rollback)
        },
    ));
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .expect("cancelled writer-transaction SELECT never reached SQLite");
    assert_eq!(
        pool.sql_bridge_reader_slots().available_permits(),
        0,
        "a writer-supertrait SELECT must retain ordinary active-reader admission"
    );

    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    let (rows, rollback) = tokio::time::timeout(std::time::Duration::from_secs(2), query)
        .await
        .expect("writer-transaction SELECT did not finish after its gate opened")
        .unwrap();
    assert_eq!(
        rows.expect("request cancellation interrupted the admitted writer transaction")
            .len(),
        1
    );
    rollback.expect("writer transaction did not return to autocommit");
    assert_eq!(pool.sql_bridge_reader_slots().available_permits(), 1);
}

#[tokio::test]
async fn expired_deadline_preserves_pool_backed_manual_atomic_read_and_commit() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE pool_writer_tx_deadline_probe(\
                 id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);

    let observed = crate::scope_request_read_deadline(
        std::time::Duration::ZERO,
        bridge.atomic_unit(Box::new(|writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO pool_writer_tx_deadline_probe VALUES (1, 'before')"
                            .into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                let count = writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM pool_writer_tx_deadline_probe".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO pool_writer_tx_deadline_probe VALUES (2, 'after')".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                Ok(Box::new(count) as Box<dyn std::any::Any + Send>)
            })
        })),
    )
    .await
    .expect("an expired read deadline must not interrupt an admitted manual write transaction");
    let observed = match observed.downcast::<Option<SqlValue>>() {
        Ok(observed) => observed,
        Err(_) => panic!("unexpected atomic result type"),
    };
    assert!(matches!(*observed, Some(SqlValue::Integer(1))));

    let reader = pool.reader().unwrap();
    let rows: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM pool_writer_tx_deadline_probe",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rows, 2, "both writes around the SELECT must commit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_standalone_open_retains_slot_until_open_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cancelled_open.db")),
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let slots = pool.sql_bridge_reader_slots();
    let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
    assert_eq!(slots.available_permits(), 0);

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let open = tokio::spawn(open_standalone_on_blocking(
        Arc::clone(&pool),
        slot,
        "test_open_reader",
        move |pool| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            open_standalone_reader(pool)
        },
    ));
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .unwrap()
        .unwrap();

    open.abort();
    assert!(matches!(open.await, Err(error) if error.is_cancelled()));
    assert_eq!(
        slots.available_permits(),
        0,
        "the permit must remain in the detached open closure"
    );
    let contender = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        Arc::clone(&slots).acquire_owned(),
    )
    .await;
    assert!(contender.is_err(), "an in-flight open must retain the cap");

    release_tx.send(()).unwrap();
    let recovered = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        Arc::clone(&slots).acquire_owned(),
    )
    .await
    .expect("the detached open did not release its permit")
    .unwrap();
    assert_eq!(slots.available_permits(), 0);
    drop(recovered);
    assert_eq!(slots.available_permits(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_writer_read_interrupts_and_releases_writer_handle() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cancelled_writer.db")),
        write_queue_enabled: Some(false),
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let handle_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };
    let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let query = tokio::spawn(crate::scope_test_read_progress(
        Arc::clone(&progress),
        async move {
            khive_storage::SqlReader::query_all(&mut writer, deliberately_slow_read_statement())
                .await
        },
    ));

    wait_for_progress(progress.as_ref()).await;
    query.abort();
    assert!(matches!(query.await, Err(error) if error.is_cancelled()));
    let writer_after = tokio::time::timeout(std::time::Duration::from_millis(500), bridge.writer())
        .await
        .expect("abandoned SQLite read did not release the writer handle promptly")
        .expect("writer handle remained unavailable after read interruption");
    drop(writer_after);
    let stopped_at = progress.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        progress.load(std::sync::atomic::Ordering::SeqCst),
        stopped_at,
        "writer-backed SQLite read kept consuming work after cancellation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_cancellation_never_interrupts_admitted_execute_batch() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cancelled_writer_batch.db")),
        write_queue_enabled: Some(false),
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE cancellation_write_probe(\
                     id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
            )
            .unwrap();
    }

    let handle_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let (entered, release, completed) = blocking_non_interrupting_progress_gate(&conn);
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let query = tokio::spawn(crate::scope_request_read_cancellation(
        cancel_rx,
        async move {
            khive_storage::SqlWriter::execute_batch(&mut writer, vec![slow_insert_statement()])
                .await
        },
    ));

    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .expect("mutating execute_batch never reached SQLite VM work");
    cancel_tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !query.is_finished(),
        "request-read cancellation must not interrupt an admitted batch"
    );

    let contender = bridge.writer().await;
    let retained_slot = matches!(
        &contender,
        Err(StorageError::AdmissionTimeout { operation, .. })
            if operation.as_ref() == "sql_bridge.writer_handle"
    );
    drop(contender);

    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    let affected = tokio::time::timeout(std::time::Duration::from_secs(2), query)
        .await
        .expect("admitted batch did not finish after its gate was released")
        .unwrap()
        .expect("request cancellation must preserve the batch result");
    assert_eq!(affected, 10_000);
    tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
        .await
        .expect("completed batch did not release its connection");
    assert!(
        retained_slot,
        "request cancellation released the writer slot before the admitted batch stopped"
    );
    let reader = pool.reader().unwrap();
    let count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM cancellation_write_probe", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 10_000, "the admitted batch must commit every row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_cancellation_never_interrupts_dml_returning_via_sql_reader() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_dml_returning_cancel.db")),
        write_queue_enabled: Some(false),
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE returning_write_probe(\
                     id INTEGER PRIMARY KEY, value INTEGER NOT NULL)",
            )
            .unwrap();
    }

    let handle_slot = pool
        .sql_bridge_writer_slots()
        .acquire_owned()
        .await
        .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let (entered, release, completed) = blocking_non_interrupting_progress_gate(&conn);
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let query = tokio::spawn(crate::scope_request_read_cancellation(
        cancel_rx,
        async move {
            khive_storage::SqlReader::query_all(
                &mut writer,
                SqlStatement {
                    sql: "INSERT INTO returning_write_probe(value) \
                          WITH RECURSIVE rows(value) AS (\
                          SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 10000\
                          ) SELECT value FROM rows RETURNING id"
                        .into(),
                    params: vec![],
                    label: Some("non-interruptible-returning-probe".into()),
                },
            )
            .await
        },
    ));

    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .expect("DML RETURNING never reached admitted SQLite work");
    cancel_tx.send(true).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert!(
        !query.is_finished(),
        "request-read cancellation interrupted DML RETURNING"
    );

    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    let rows = tokio::time::timeout(std::time::Duration::from_secs(2), query)
        .await
        .expect("DML RETURNING did not finish after its gate was released")
        .unwrap()
        .expect("request cancellation must preserve DML RETURNING's result");
    assert_eq!(rows.len(), 10_000);
    tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
        .await
        .expect("completed DML RETURNING did not release its connection");

    let reader = pool.reader().unwrap();
    let count: i64 = reader
        .conn()
        .query_row("SELECT COUNT(*) FROM returning_write_probe", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 10_000, "DML RETURNING must commit every row");
}

/// Cancelling an in-flight call permanently invalidates the boxed handle:
/// the call took the handle's connection into the detached blocking task,
/// so every subsequent call on the SAME handle fails loudly with
/// "connection already consumed" instead of silently operating on a
/// connection that may still be running the cancelled statement.
/// Callers that cancel or time out a bridge call must drop the handle and
/// acquire a fresh one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_call_invalidates_handle_reuse_fails_loud() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_cancelled_reuse.db")),
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    let handle_slot = acquire_handle_slot(
        pool.sql_bridge_writer_slots(),
        pool.config().checkout_timeout,
        "sql_bridge.writer_handle",
        SlotTimeoutClass::Admission,
    )
    .await
    .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let (entered, release, completed) = blocking_non_interrupting_progress_gate(&conn);
    let writer = Arc::new(tokio::sync::Mutex::new(SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    }));
    let writer_clone = Arc::clone(&writer);
    let query = tokio::spawn(async move {
        khive_storage::SqlWriter::execute_batch(
            &mut *writer_clone.lock().await,
            vec![progress_gate_statement()],
        )
        .await
    });

    entered.notified().await;
    query.abort();
    let cancelled = matches!(query.await, Err(error) if error.is_cancelled());

    let reuse = khive_storage::SqlWriter::execute(
        &mut *writer.lock().await,
        SqlStatement {
            sql: "CREATE TABLE cancelled_reuse_probe (id INTEGER PRIMARY KEY)".into(),
            params: vec![],
            label: None,
        },
    )
    .await;
    let message = match reuse {
        Err(StorageError::Pool { message, .. }) => message,
        other => panic!(
            "reusing a cancelled writer handle must fail loudly with \
                 'connection already consumed'; got {other:?}"
        ),
    };
    assert!(
        message.contains("connection already consumed"),
        "expected the cancelled handle's reuse error to name the pinned \
             failure; got {message:?}"
    );

    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
        .await
        .expect("cancelled writer's detached SQLite call did not finish");
    assert!(cancelled, "writer batch task did not report cancellation");
}

/// Transaction-control statements (`BEGIN`/`START`/`COMMIT`/`END`/
/// `ROLLBACK`/`SAVEPOINT`/`RELEASE`) in `execute_batch` input are rejected with a
/// typed invalid-input error BEFORE anything executes, on the standalone
/// path: a caller `COMMIT` inside the batch's own `BEGIN IMMEDIATE`
/// would commit early and break the all-or-nothing contract. The
/// rejection must leave the handle untouched and fully reusable, and no
/// statement (not even the valid ones before the offending one) may
/// have run.
#[tokio::test]
async fn execute_batch_rejects_transaction_control_before_executing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_tx_control_reject.db")),
        checkout_timeout: std::time::Duration::from_millis(250),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE tx_reject_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }

    let handle_slot = acquire_handle_slot(
        pool.sql_bridge_writer_slots(),
        pool.config().checkout_timeout,
        "sql_bridge.writer_handle",
        SlotTimeoutClass::Admission,
    )
    .await
    .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };

    for tail in ["COMMIT", "BEGIN"] {
        let multi = khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![SqlStatement {
                sql: format!("INSERT INTO tx_reject_test (id, val) VALUES (10, 'tail'); {tail}"),
                params: vec![],
                label: None,
            }],
        )
        .await;
        let message = multi
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(
            message.contains("Multiple statements"),
            "a SqlStatement with trailing {tail} must be rejected before execution; got {message}"
        );
    }

    // A valid INSERT first, then a bare COMMIT: the whole batch must be
    // rejected and the INSERT must NOT have run.
    let batch = khive_storage::SqlWriter::execute_batch(
        &mut writer,
        vec![
            SqlStatement {
                sql: "INSERT INTO tx_reject_test (id, val) VALUES (1, 'a')".into(),
                params: vec![],
                label: None,
            },
            SqlStatement {
                sql: "COMMIT".into(),
                params: vec![],
                label: None,
            },
        ],
    )
    .await;
    match &batch {
        Err(StorageError::InvalidInput {
            operation, message, ..
        }) => {
            assert_eq!(operation.as_ref(), "execute_batch");
            assert!(
                message.contains("transaction control") && message.contains("COMMIT"),
                "the rejection must name the offending statement head; got {message:?}"
            );
        }
        other => {
            panic!("a batch containing a bare COMMIT must be rejected up front; got {other:?}")
        }
    }

    // Every transaction-control head is rejected, case-insensitively and
    // through leading whitespace and `--`/`/* */` comments.
    for sql in [
        "BEGIN IMMEDIATE",
        "START TRANSACTION",
        "commit",
        "End transaction",
        "ROLLBACK",
        "SAVEPOINT sp1",
        "RELEASE sp1",
        "  -- leading comment\nCOMMIT",
        "/* block */ rollback to savepoint sp1",
    ] {
        let rejected = khive_storage::SqlWriter::execute_batch(
            &mut writer,
            vec![SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            }],
        )
        .await;
        assert!(
            matches!(&rejected, Err(StorageError::InvalidInput { .. })),
            "transaction-control head {sql:?} must be rejected; got {rejected:?}"
        );
    }

    // The rejection ran before the handle was taken: no statement
    // executed (the INSERT above did not land), and the handle is still
    // fully reusable.
    let count: i64 = {
        let guard = pool.reader().unwrap();
        guard
            .conn()
            .query_row("SELECT COUNT(*) FROM tx_reject_test", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count, 0, "a rejected batch must not have executed anything");

    let affected = khive_storage::SqlWriter::execute(
        &mut writer,
        SqlStatement {
            sql: "INSERT INTO tx_reject_test (id, val) VALUES (2, 'b')".into(),
            params: vec![],
            label: None,
        },
    )
    .await
    .expect("the handle must survive a rejected batch untouched");
    assert_eq!(affected, 1);
}

#[tokio::test]
async fn standalone_execute_batch_rejects_prefixed_commit_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_prefixed_commit_standalone.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch("CREATE TABLE prefixed_commit (id INTEGER PRIMARY KEY)")
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();

    let rejected = writer
        .execute_batch(vec![
            SqlStatement {
                sql: "INSERT INTO prefixed_commit (id) VALUES (1)".into(),
                params: vec![],
                label: None,
            },
            SqlStatement {
                sql: " ; -- empty statement\n /* leading comment */ \u{feff} ; COMMIT".into(),
                params: vec![],
                label: None,
            },
        ])
        .await;
    assert!(
        matches!(
            &rejected,
            Err(StorageError::InvalidInput {
                operation,
                message,
                ..
            }) if operation.as_ref() == "execute_batch"
                && message.contains("transaction control")
                && message.contains("COMMIT")
        ),
        "standalone execute_batch must reject a prefixed COMMIT before the INSERT; \
             got {rejected:?}"
    );

    let mut reader = bridge.reader().await.unwrap();
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM prefixed_commit".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(count, Some(SqlValue::Integer(0))),
        "prefixed COMMIT rejection must happen before the earlier INSERT; got {count:?}"
    );

    let affected = writer
        .execute(SqlStatement {
            sql: "INSERT INTO prefixed_commit (id) VALUES (2)".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("prefixed COMMIT rejection must leave the standalone handle reusable");
    assert_eq!(affected, 1);
}

#[tokio::test]
async fn execute_batch_rejects_multi_statement_on_pool_backed_path() {
    let pool = Arc::new(ConnectionPool::new(PoolConfig::default()).unwrap());
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch("CREATE TABLE multi_statement_pool_test (id INTEGER PRIMARY KEY, val TEXT)")
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let mut writer = bridge.writer().await.unwrap();

    let result = khive_storage::SqlWriter::execute_batch(
        &mut *writer,
        vec![SqlStatement {
            sql: "INSERT INTO multi_statement_pool_test (id, val) VALUES (1, 'x'); COMMIT".into(),
            params: vec![],
            label: None,
        }],
    )
    .await;
    let message = result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        message.contains("Multiple statements"),
        "pool-backed execute_batch must reject a trailing COMMIT; got {message}"
    );
    let count: i64 = pool
        .reader()
        .unwrap()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM multi_statement_pool_test",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn inline_execute_batch_rejects_multi_statement_sql() {
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join("sql_bridge_multi_statement_inline.db")),
            write_queue_enabled: Some(true),
            write_routing_strict: true,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(
            "CREATE TABLE multi_statement_inline_test (id INTEGER PRIMARY KEY, val TEXT)",
        )
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let result = bridge
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute_batch(vec![SqlStatement {
                            sql: "INSERT INTO multi_statement_inline_test (id, val) VALUES (1, 'x'); BEGIN"
                                .into(),
                            params: vec![],
                            label: None,
                        }])
                        .await
                        .map(|_| Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await;
    let message = result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        message.contains("Multiple statements"),
        "InlineWriter must reject a trailing BEGIN; got {message}"
    );
    let count: i64 = pool
        .reader()
        .unwrap()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM multi_statement_inline_test",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

/// Unit matrix for [`transaction_control_head`]: statement heads are
/// classified case-insensitively through leading whitespace and
/// comments; non-transaction-control heads (including identifiers that
/// merely START with a keyword) never match.
#[test]
fn transaction_control_head_classification_matrix() {
    for (sql, expected) in [
        ("BEGIN", Some("BEGIN")),
        ("begin immediate", Some("BEGIN")),
        ("START TRANSACTION", Some("START")),
        ("start transaction", Some("START")),
        ("COMMIT", Some("COMMIT")),
        ("commit;", Some("COMMIT")),
        ("END", Some("END")),
        ("end transaction", Some("END")),
        ("ROLLBACK", Some("ROLLBACK")),
        ("rollback to savepoint sp1", Some("ROLLBACK")),
        ("SAVEPOINT sp1", Some("SAVEPOINT")),
        ("RELEASE sp1", Some("RELEASE")),
        ("release savepoint sp1", Some("RELEASE")),
        ("   \t COMMIT", Some("COMMIT")),
        ("\u{feff}BEGIN", Some("BEGIN")),
        (" ; BEGIN", Some("BEGIN")),
        (" ; ; -- empty\n /* comment */ COMMIT", Some("COMMIT")),
        ("  \u{feff} SAVEPOINT sp1", Some("SAVEPOINT")),
        ("/* comment */ \u{feff} ; RELEASE sp1", Some("RELEASE")),
        ("\u{feff} ; \u{feff} -- empty\n ROLLBACK", Some("ROLLBACK")),
        ("-- a comment\nCOMMIT", Some("COMMIT")),
        // SQLite does not nest block comments: the comment ends at the
        // first `*/`, leaving `*/ COMMIT`, which is not a statement head.
        ("/* /* nested? no */ */ COMMIT", None),
        ("-- one\n-- two\n  /* x */ begin", Some("BEGIN")),
        ("INSERT INTO t VALUES (1)", None),
        ("UPDATE t SET x = 1", None),
        ("DELETE FROM t", None),
        ("SELECT * FROM commit_log", None),
        ("CREATE TABLE rollback_audit (id INTEGER)", None),
        ("/* comment only */", None),
        (" ; /* empty statements only */ ; ", None),
        (" ; SELECT 1", None),
        ("", None),
    ] {
        assert_eq!(
            transaction_control_head(sql),
            expected,
            "classification mismatch for {sql:?}"
        );
    }
}

#[test]
fn cached_read_transaction_control_classification_matrix() {
    use CachedReadTransactionControl::{BeginDeferred, Finish, Unsupported};

    for (sql, expected) in [
        ("BEGIN", Some(BeginDeferred)),
        ("begin transaction", Some(BeginDeferred)),
        (
            "/* p */ \u{feff} ; BEGIN /* mode */ DEFERRED",
            Some(BeginDeferred),
        ),
        ("BEGIN DEFERRED TRANSACTION", Some(BeginDeferred)),
        ("BEGIN IMMEDIATE", Some(Unsupported("BEGIN"))),
        ("BEGIN /* lock */ EXCLUSIVE", Some(Unsupported("BEGIN"))),
        ("BEGIN TRANSACTION IMMEDIATE", Some(Unsupported("BEGIN"))),
        ("begin transaction exclusive", Some(Unsupported("BEGIN"))),
        ("BEGIN TRANSACTION DEFERRED", Some(Unsupported("BEGIN"))),
        ("BEGIN IMMEDIATE TRANSACTION", Some(Unsupported("BEGIN"))),
        ("BEGIN TRANSACTION named_txn", Some(Unsupported("BEGIN"))),
        (
            "BEGIN DEFERRED TRANSACTION trailing",
            Some(Unsupported("BEGIN")),
        ),
        ("BEGIN DEFERRED DEFERRED", Some(Unsupported("BEGIN"))),
        // Non-identifier tails: the tokenizer yields no token for a
        // quoted, bracketed, or backticked tail, which must read as a
        // refused remainder, never as end-of-statement.
        (
            "BEGIN TRANSACTION \"IMMEDIATE\"",
            Some(Unsupported("BEGIN")),
        ),
        ("BEGIN TRANSACTION [IMMEDIATE]", Some(Unsupported("BEGIN"))),
        ("BEGIN TRANSACTION `IMMEDIATE`", Some(Unsupported("BEGIN"))),
        ("BEGIN TRANSACTION 'IMMEDIATE'", Some(Unsupported("BEGIN"))),
        ("BEGIN \"DEFERRED\"", Some(Unsupported("BEGIN"))),
        ("BEGIN; COMMIT", Some(Unsupported("BEGIN"))),
        // Trailing empty statements and trivia remain an accepted end.
        ("BEGIN;", Some(BeginDeferred)),
        ("BEGIN DEFERRED ; -- done", Some(BeginDeferred)),
        ("BEGIN TRANSACTION /* t */ ;;", Some(BeginDeferred)),
        ("START TRANSACTION", Some(Unsupported("START"))),
        ("COMMIT", Some(Finish("COMMIT"))),
        ("END TRANSACTION", Some(Finish("END"))),
        ("ROLLBACK", Some(Finish("ROLLBACK"))),
        ("ROLLBACK TRANSACTION", Some(Finish("ROLLBACK"))),
        ("ROLLBACK TO sp", Some(Unsupported("ROLLBACK"))),
        (
            "ROLLBACK /* nested */ TRANSACTION /* target */ TO sp",
            Some(Unsupported("ROLLBACK")),
        ),
        ("SAVEPOINT sp", Some(Unsupported("SAVEPOINT"))),
        ("SELECT 1", None),
    ] {
        assert_eq!(
            cached_read_transaction_control(sql),
            expected,
            "cached-reader transaction classification mismatch for {sql:?}"
        );
    }
}

#[test]
fn sqlite_accepts_utf8_bom_before_transaction_control() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE bom_transaction_test (id INTEGER)")
        .unwrap();
    conn.execute_batch("\u{feff}BEGIN IMMEDIATE").unwrap();
    conn.execute_batch("ROLLBACK").unwrap();
}

/// The queue-backed `execute_batch` path rejects transaction-control
/// statements too, and the rejection protects the writer task: a caller
/// `COMMIT` that reached the task would close its per-request `BEGIN
/// IMMEDIATE` and terminate the task permanently. After the typed
/// rejection, a legitimate batch must still succeed through the SAME
/// writer task (it was never touched).
#[tokio::test]
async fn execute_batch_rejects_transaction_control_on_queue_backed_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_tx_reject_queue.db")),
        checkout_timeout: std::time::Duration::from_millis(250),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE tx_reject_queue_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writer = bridge.writer().await.unwrap();

    let rejected = khive_storage::SqlWriter::execute_batch(
        &mut *writer,
        vec![
            SqlStatement {
                sql: "INSERT INTO tx_reject_queue_test (id, val) VALUES (1, 'a')".into(),
                params: vec![],
                label: None,
            },
            SqlStatement {
                sql: "COMMIT".into(),
                params: vec![],
                label: None,
            },
        ],
    )
    .await;
    assert!(
        matches!(&rejected, Err(StorageError::InvalidInput { .. })),
        "a bare COMMIT in a queue-backed batch must be rejected up front; got {rejected:?}"
    );

    let prefixed = khive_storage::SqlWriter::execute_batch(
        &mut *writer,
        vec![
            SqlStatement {
                sql: "INSERT INTO tx_reject_queue_test (id, val) VALUES (3, 'prefixed')".into(),
                params: vec![],
                label: None,
            },
            SqlStatement {
                sql: "/* leading */ \u{feff} ; -- empty\n ; COMMIT".into(),
                params: vec![],
                label: None,
            },
        ],
    )
    .await;
    assert!(
        matches!(
            &prefixed,
            Err(StorageError::InvalidInput {
                operation,
                message,
                ..
            }) if operation.as_ref() == "execute_batch"
                && message.contains("transaction control")
                && message.contains("COMMIT")
        ),
        "a prefixed COMMIT must be rejected before touching the writer task; got {prefixed:?}"
    );

    let affected = khive_storage::SqlWriter::execute_batch(
        &mut *writer,
        vec![SqlStatement {
            sql: "INSERT INTO tx_reject_queue_test (id, val) VALUES (2, 'b')".into(),
            params: vec![],
            label: None,
        }],
    )
    .await
    .expect("the writer task must survive the rejected batch");
    assert_eq!(affected, 1);

    let count: i64 = {
        let guard = pool.reader().unwrap();
        guard
            .conn()
            .query_row("SELECT COUNT(*) FROM tx_reject_queue_test", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert_eq!(
        count, 1,
        "exactly the post-rejection batch's row may have landed"
    );
}

/// A failed ROLLBACK after a statement failure poisons the handle: the
/// connection may be in an unknown transaction state, so it is dropped
/// instead of restored, and every subsequent call on the same handle
/// fails loudly with "connection already consumed". The caller sees the
/// ORIGINAL statement error with the poison context attached (the
/// rollback failure is never hidden, but never replaces the original).
///
/// Forcing the arm legitimately (the pre-round-2 version smuggled a
/// bare `COMMIT` into the batch, which `execute_batch` now rejects up
/// front): a connection authorizer denies the `ROLLBACK` transaction
/// operation, so the error path's `ROLLBACK` genuinely fails while the
/// batch's own `BEGIN IMMEDIATE` and the statements run normally.
#[tokio::test]
async fn failed_rollback_poisons_handle_reuse_fails_loud() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    fn deny_rollback(ctx: AuthContext<'_>) -> Authorization {
        match ctx.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_rollback_poison.db")),
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE rollback_poison_test (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }

    let handle_slot = acquire_handle_slot(
        pool.sql_bridge_writer_slots(),
        pool.config().checkout_timeout,
        "sql_bridge.writer_handle",
        SlotTimeoutClass::Admission,
    )
    .await
    .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    conn.authorizer(Some(deny_rollback)).unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };

    let batch = khive_storage::SqlWriter::execute_batch(
        &mut writer,
        vec![
            SqlStatement {
                sql: "INSERT INTO rollback_poison_test (id, val) VALUES (1, 'a')".into(),
                params: vec![],
                label: None,
            },
            SqlStatement {
                sql: "SELECT FROM WHERE".into(),
                params: vec![],
                label: None,
            },
        ],
    )
    .await;
    let batch_error = batch.expect_err("invalid second statement must fail the batch");
    assert_eq!(
        batch_error.sqlite_write_failure(),
        Some(khive_storage::error::SqliteWriteFailure {
            stage: khive_storage::error::SqliteWriteStage::Statement,
            primary_code: rusqlite::ffi::SQLITE_ERROR,
            extended_code: rusqlite::ffi::SQLITE_ERROR,
            settlement_unknown: true,
        })
    );
    let poison = match batch_error.without_sqlite_write_stage() {
        StorageError::Driver { source, .. } => source
            .downcast_ref::<PoisonedBatchError>()
            .expect("failed rollback must retain its typed poison wrapper"),
        other => panic!("failed rollback must return a driver error; got {other:?}"),
    };
    assert!(
        matches!(&poison.poison_reason, BatchPoisonReason::RollbackFailed(_)),
        "the poison cause must be compiler-checked as RollbackFailed; got {poison:?}"
    );
    let batch_message = batch_error.to_string();
    assert!(
        batch_message.contains("ROLLBACK after statement failure failed"),
        "the caller must see the poison context naming the failed \
             rollback; got {batch_message:?}"
    );
    assert!(
        batch_message.contains("original error"),
        "the original statement error must stay visible alongside the \
             poison context; got {batch_message:?}"
    );

    let reuse = khive_storage::SqlWriter::execute(
        &mut writer,
        SqlStatement {
            sql: "CREATE TABLE rollback_poison_probe (id INTEGER PRIMARY KEY)".into(),
            params: vec![],
            label: None,
        },
    )
    .await;
    let message = match reuse {
        Err(StorageError::Pool { message, .. }) => message,
        other => panic!(
            "reusing a poisoned writer handle must fail loudly with \
                 'connection already consumed'; got {other:?}"
        ),
    };
    assert!(
        message.contains("connection already consumed"),
        "expected the poisoned handle's reuse error to name the pinned \
             failure; got {message:?}"
    );
}

/// A NON-TRANSIENT `BEGIN IMMEDIATE` failure poisons the handle instead
/// of restoring it: the connection's transaction state is suspect (here
/// a caller-driven transaction is already open on the same connection,
/// so SQLite answers "cannot start a transaction within a transaction"),
/// and the returned error carries the poison context.
#[tokio::test]
async fn non_transient_begin_failure_poisons_handle() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_begin_poison.db")),
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    let handle_slot = acquire_handle_slot(
        pool.sql_bridge_writer_slots(),
        pool.config().checkout_timeout,
        "sql_bridge.writer_handle",
        SlotTimeoutClass::Admission,
    )
    .await
    .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    // A caller-driven open transaction on the same connection: the
    // batch's own `BEGIN IMMEDIATE` fails non-transiently.
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };

    let batch = khive_storage::SqlWriter::execute_batch(
        &mut writer,
        vec![SqlStatement {
            sql: "SELECT 1".into(),
            params: vec![],
            label: None,
        }],
    )
    .await;
    let batch_error = batch.expect_err("BEGIN inside an open transaction must fail");
    assert_eq!(
        batch_error.sqlite_write_failure(),
        Some(khive_storage::error::SqliteWriteFailure {
            stage: khive_storage::error::SqliteWriteStage::Begin,
            primary_code: rusqlite::ffi::SQLITE_ERROR,
            extended_code: rusqlite::ffi::SQLITE_ERROR,
            settlement_unknown: false,
        })
    );
    let poison = match batch_error.without_sqlite_write_stage() {
        StorageError::Driver { source, .. } => source
            .downcast_ref::<PoisonedBatchError>()
            .expect("failed BEGIN must retain its typed poison wrapper"),
        other => panic!("failed BEGIN must return a driver error; got {other:?}"),
    };
    assert!(
        matches!(&poison.poison_reason, BatchPoisonReason::BeginFailed),
        "the poison cause must be compiler-checked as BeginFailed; got {poison:?}"
    );
    let batch_message = batch_error.to_string();
    assert!(
        batch_message.contains("BEGIN IMMEDIATE failed non-transiently"),
        "a non-transient BEGIN failure must surface the poison context; \
             got {batch_message:?}"
    );
    assert!(
        batch_message.contains("cannot start a transaction within a transaction"),
        "the original BEGIN error must stay visible; got {batch_message:?}"
    );

    let reuse = khive_storage::SqlWriter::execute(
        &mut writer,
        SqlStatement {
            sql: "CREATE TABLE begin_poison_probe (id INTEGER PRIMARY KEY)".into(),
            params: vec![],
            label: None,
        },
    )
    .await;
    assert!(
        matches!(
            &reuse,
            Err(StorageError::Pool { message, .. })
                if message.contains("connection already consumed")
        ),
        "a handle poisoned by a non-transient BEGIN failure must be \
             dropped, not restored; got {reuse:?}"
    );
}

/// A BUSY/LOCKED `BEGIN IMMEDIATE` failure is transient contention: the
/// connection itself is untouched, so the handle is restored as
/// reusable, and the next call succeeds once the contending lock is
/// released.
#[tokio::test]
async fn busy_begin_failure_restores_handle_reusable() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_begin_busy.db")),
        checkout_timeout: std::time::Duration::from_millis(250),
        busy_timeout: std::time::Duration::from_millis(100),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch("CREATE TABLE begin_busy_test (id INTEGER PRIMARY KEY)")
            .unwrap();
    }

    // Hold SQLite's write lock from a separate connection so the batch's
    // `BEGIN IMMEDIATE` genuinely fails with SQLITE_BUSY after the short
    // busy timeout.
    let lock_conn = pool.open_standalone_writer().unwrap();
    lock_conn.execute_batch("BEGIN IMMEDIATE").unwrap();

    let handle_slot = acquire_handle_slot(
        pool.sql_bridge_writer_slots(),
        pool.config().checkout_timeout,
        "sql_bridge.writer_handle",
        SlotTimeoutClass::Admission,
    )
    .await
    .unwrap();
    let conn = open_standalone_writer(&pool).unwrap();
    let mut writer = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: Some(StandaloneHandle {
            conn,
            _retained_slot: Some(handle_slot),
            read_transaction_slot: None,
        }),
        writer_task: None,
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };

    let batch = khive_storage::SqlWriter::execute_batch(
        &mut writer,
        vec![SqlStatement {
            sql: "INSERT INTO begin_busy_test (id) VALUES (1)".into(),
            params: vec![],
            label: None,
        }],
    )
    .await;
    let batch_error = batch.expect_err("BEGIN IMMEDIATE under a held write lock must fail");
    assert!(
        batch_error.to_string().contains("database is locked"),
        "the busy BEGIN failure must surface SQLite's busy error; got {batch_error:?}"
    );

    lock_conn.execute_batch("ROLLBACK").unwrap();
    drop(lock_conn);

    let affected = khive_storage::SqlWriter::execute(
        &mut writer,
        SqlStatement {
            sql: "INSERT INTO begin_busy_test (id) VALUES (2)".into(),
            params: vec![],
            label: None,
        },
    )
    .await
    .expect("a busy BEGIN failure must restore the handle as reusable");
    assert_eq!(affected, 1);
}

/// The manual `atomic_unit` path (write queue off) shares the pool's
/// one-permit writer-handle budget with `writer()`: while a boxed writer
/// handle is live, `atomic_unit` times out; after the handle drops, the
/// next `atomic_unit` succeeds on the same pool.
#[tokio::test]
async fn manual_atomic_unit_shares_writer_permit_budget_with_writer_handle() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("sql_bridge_atomic_unit_budget.db")),
        checkout_timeout: std::time::Duration::from_millis(50),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS atomic_unit_budget_test \
                     (id INTEGER PRIMARY KEY, val INTEGER NOT NULL)",
            )
            .unwrap();
    }

    fn insert_op(id: i64) -> AtomicUnitOp {
        Box::new(move |writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO atomic_unit_budget_test (id, val) VALUES (?1, ?2)".into(),
                        params: vec![SqlValue::Integer(id), SqlValue::Integer(id)],
                        label: None,
                    })
                    .await
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            StorageCapability::Sql,
                            "atomic_unit_budget_test_insert",
                            e,
                        )
                    })?;
                Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
            })
        })
    }

    let writer_handle = bridge.writer().await.unwrap();
    let blocked = bridge.atomic_unit(insert_op(1)).await;
    assert!(
        matches!(
            &blocked,
            Err(StorageError::AdmissionTimeout { operation, .. })
                if operation.as_ref() == "sql_bridge.atomic_unit_handle"
        ),
        "atomic_unit must time out on the shared writer permit while a \
             writer handle is live; got {blocked:?}"
    );

    drop(writer_handle);
    let unblocked = bridge.atomic_unit(insert_op(2)).await;
    assert!(
        unblocked.is_ok(),
        "atomic_unit must succeed once the writer handle releases the \
             shared writer permit; got {unblocked:?}"
    );

    let mut reader = bridge.reader().await.unwrap();
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM atomic_unit_budget_test".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(count, Some(SqlValue::Integer(1))),
        "only the post-drop atomic_unit call may have committed; got {count:?}"
    );
}

/// ADR-067 Component A entry 10: with `KHIVE_WRITE_QUEUE=1`,
/// `SqliteWriter::execute_batch` (reached via `SqlBridge::writer()`)
/// routes the whole statement list through the WriterTask channel
/// instead of opening its own `BEGIN IMMEDIATE` on the standalone
/// connection, and the row is actually committed and readable back.
#[tokio::test]
async fn execute_batch_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_execute_batch.db");
    let config = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS write_queue_batch_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }

    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut writer = bridge.writer().await.unwrap();
    let affected = writer
        .execute_batch(vec![
            SqlStatement {
                sql: "INSERT INTO write_queue_batch_test (id, val) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(1), SqlValue::Text("a".into())],
                label: None,
            },
            SqlStatement {
                sql: "INSERT INTO write_queue_batch_test (id, val) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(2), SqlValue::Text("b".into())],
                label: None,
            },
        ])
        .await
        .unwrap();
    assert_eq!(affected, 2);

    let mut reader = bridge.reader().await.unwrap();
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM write_queue_batch_test".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(count, Some(SqlValue::Integer(2))),
        "expected 2 rows, got {count:?}"
    );
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}

/// ADR-067 Component A entry 10, atomicity: a batch whose second
/// statement fails (duplicate primary key) must roll back the WHOLE
/// request — including the first statement's otherwise-successful
/// INSERT — because the WriterTask commits or rolls back one
/// `WriteRequest` as a single unit (ADR-067 Component A). Zero rows must
/// land, not one.
#[tokio::test]
async fn execute_batch_rolls_back_atomically_on_mid_sequence_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_execute_batch_rollback.db");
    let config = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS write_queue_rollback_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }

    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut writer = bridge.writer().await.unwrap();
    let result = writer
        .execute_batch(vec![
            // Statement 1: succeeds on its own.
            SqlStatement {
                sql: "INSERT INTO write_queue_rollback_test (id, val) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(1), SqlValue::Text("first".into())],
                label: None,
            },
            // Statement 2: duplicate primary key — fails mid-sequence.
            SqlStatement {
                sql: "INSERT INTO write_queue_rollback_test (id, val) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(1), SqlValue::Text("duplicate".into())],
                label: None,
            },
            // Statement 3: never reached.
            SqlStatement {
                sql: "INSERT INTO write_queue_rollback_test (id, val) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Integer(2), SqlValue::Text("third".into())],
                label: None,
            },
        ])
        .await;
    assert!(
        result.is_err(),
        "a batch with a mid-sequence PK conflict must return an error"
    );

    let mut reader = bridge.reader().await.unwrap();
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM write_queue_rollback_test".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(count, Some(SqlValue::Integer(0))),
        "the whole request must roll back — including statement 1's \
             otherwise-successful INSERT — not just the failing statement; \
             got {count:?}"
    );
}

#[tokio::test]
async fn in_memory_atomic_unit_terminal_fault_retires_writer() {
    use khive_storage::WriterTaskRequestState;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    // Exercise operation, commit, and panic failures, including a clean
    // panic rollback. Terminal connections must never reenter the pool.
    for (mode, deny_rollback, expected) in [
        ("error", true, WriterTaskRequestState::SideEffectsUnknown),
        ("commit", true, WriterTaskRequestState::SideEffectsUnknown),
        ("panic", true, WriterTaskRequestState::SideEffectsUnknown),
        (
            "panic",
            false,
            WriterTaskRequestState::TransactionRolledBack,
        ),
    ] {
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: None,
                ..PoolConfig::default()
            })
            .unwrap(),
        );
        {
            let guard = pool.writer().unwrap();
            guard
                .execute_batch("CREATE TABLE atomic_terminal_probe (id INTEGER PRIMARY KEY)")
                .unwrap();
            guard
                .authorizer(Some(move |ctx: AuthContext<'_>| match ctx.action {
                    AuthAction::Transaction {
                        operation: TransactionOperation::Rollback,
                    } if deny_rollback => Authorization::Deny,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Unknown,
                    } if mode == "commit" => Authorization::Deny,
                    _ => Authorization::Allow,
                }))
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), false);
        let result = bridge
            .atomic_unit(Box::new(move |writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement {
                            sql: "INSERT INTO atomic_terminal_probe VALUES (1)".into(),
                            params: vec![],
                            label: None,
                        })
                        .await?;
                    match mode {
                        "error" => Err(StorageError::Internal("terminal probe".into())),
                        "panic" => panic!("terminal probe"),
                        _ => Ok(Box::new(()) as Box<dyn Any + Send>),
                    }
                })
            }))
            .await;
        if mode == "commit" {
            assert_eq!(
                result
                    .as_ref()
                    .err()
                    .and_then(StorageError::sqlite_write_failure),
                Some(khive_storage::error::SqliteWriteFailure {
                    stage: khive_storage::error::SqliteWriteStage::Commit,
                    primary_code: rusqlite::ffi::SQLITE_AUTH,
                    extended_code: rusqlite::ffi::SQLITE_AUTH,
                    settlement_unknown: true,
                })
            );
        }
        assert!(
            matches!(result.as_ref().map_err(StorageError::without_sqlite_write_stage),
                Err(StorageError::WriterTaskTerminated { request_state, .. })
                if *request_state == expected),
            "{mode}: {result:?}"
        );
        assert!(
            pool.try_checkpoint_nowait().is_err(),
            "{mode}: writer was not retired"
        );
        let mut writer = bridge.writer().await.unwrap();
        assert!(
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO atomic_terminal_probe VALUES (2)".into(),
                    params: vec![],
                    label: None,
                })
                .await
                .is_err(),
            "{mode}: ordinary write reused a terminal connection"
        );
    }
}

#[tokio::test]
async fn in_memory_atomic_unit_holds_writer_guard_through_rollback() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .execute_batch("CREATE TABLE atomic_guard_probe (id INTEGER PRIMARY KEY)")
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let excluded = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&excluded);
    let probe_pool = Arc::clone(&pool);
    let result = bridge
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO atomic_guard_probe VALUES (1)".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                observed.store(
                    probe_pool.try_checkpoint_nowait().is_err(),
                    Ordering::SeqCst,
                );
                Err(StorageError::Internal("rollback guard probe".into()))
            })
        }))
        .await;
    assert!(
        matches!(result, Err(StorageError::WriterTaskRequestFailed {
                request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                ref source,
            }) if matches!(source.as_ref(), StorageError::Internal(message)
                if message == "rollback guard probe")),
        "{result:?}"
    );
    assert!(
        excluded.load(Ordering::SeqCst),
        "atomic unit released its writer guard"
    );
    let mut writer = bridge.writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO atomic_guard_probe VALUES (2)".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    let rows = writer
        .query_all(SqlStatement {
            sql: "SELECT id FROM atomic_guard_probe ORDER BY id".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(matches!(rows[0].columns[0].value, SqlValue::Integer(2)));
}

#[tokio::test]
async fn in_memory_atomic_unit_pending_op_rolls_back_and_releases_guard() {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .execute_batch("CREATE TABLE atomic_pending_probe (id INTEGER PRIMARY KEY)")
        .unwrap();
    let bridge = SqlBridge::new(Arc::clone(&pool), false);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        bridge.atomic_unit(Box::new(|writer| {
            Box::pin(async move {
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO atomic_pending_probe VALUES (1)".into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;
                std::future::pending::<khive_storage::types::StorageResult<Box<dyn Any + Send>>>()
                    .await
            })
        })),
    )
    .await
    .expect("suspending atomic unit must return promptly");
    assert!(
        matches!(result, Err(StorageError::WriterTaskRequestFailed {
                request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                ref source,
            }) if matches!(source.as_ref(), StorageError::Internal(message)
                if message.contains("future suspended"))),
        "{result:?}"
    );
    let mut writer = bridge.writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO atomic_pending_probe VALUES (2)".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    let sum = writer
        .query_scalar(SqlStatement {
            sql: "SELECT SUM(id) FROM atomic_pending_probe".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(sum, Some(SqlValue::Integer(2))), "{sum:?}");
    assert!(pool.writer().unwrap().is_autocommit());
}

/// ADR-067 Component A: before
/// this fix, `block_on_sync` (this file) `unreachable!()`-panicked if
/// an `atomic_unit` closure's future was `Pending` on its first poll.
/// That panic ran inside the writer task's own `spawn_blocking` frame
/// (see `atomic_unit`'s flag-on branch), and `run_writer_task` treats
/// any `spawn_blocking` `JoinError` as fatal — the whole writer task
/// exits, taking down every subsequent write for this pool. Proves the
/// fix: an `atomic_unit` op built to suspend on first poll (via
/// `std::future::pending`, never actually resolving) now returns a
/// clean `Err` from `atomic_unit` — no panic — AND the writer task
/// survives to serve a completely unrelated, well-behaved `atomic_unit`
/// call immediately afterward.
///
/// Not `#[serial]` / no env var: builds the pool directly with
/// `write_queue_enabled: Some(true)` in the `PoolConfig` literal, same
/// technique as this round's other new routing tests.
#[tokio::test]
async fn atomic_unit_pending_future_errors_without_killing_writer_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("atomic_unit_pending_future.db");
    let config = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS atomic_unit_pending_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }
    assert!(
        pool.writer_task_handle().unwrap().is_some(),
        "writer task must be spawned with the flag on for a file-backed pool"
    );

    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    // A closure whose future never resolves on first poll — the exact
    // misuse `block_on_sync` must reject instead of panicking on.
    let pending_op: AtomicUnitOp = Box::new(|_writer| {
        Box::pin(std::future::pending::<
            khive_storage::types::StorageResult<Box<dyn std::any::Any + Send>>,
        >())
    });

    let pending_result = bridge.atomic_unit(pending_op).await;
    assert!(
        pending_result.is_err(),
        "a Pending-on-first-poll atomic_unit closure must return Err, \
             not panic; got {pending_result:?}"
    );

    // If the panic had instead killed the writer task, every subsequent
    // write on this pool (including a completely unrelated, correctly
    // non-blocking atomic_unit call) would now fail with a channel-closed
    // error. Prove the task is still alive and serving requests.
    let ok_op: AtomicUnitOp = Box::new(|writer| {
        Box::pin(async move {
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO atomic_unit_pending_test (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(1), SqlValue::Text("survived".into())],
                    label: None,
                })
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        StorageCapability::Sql,
                        "atomic_unit_pending_future_test_insert",
                        e,
                    )
                })?;
            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });
    let ok_result = bridge.atomic_unit(ok_op).await;
    assert!(
        ok_result.is_ok(),
        "writer task must survive a Pending misuse and keep serving \
             subsequent well-behaved atomic_unit requests; got {ok_result:?}"
    );

    let mut reader = bridge.reader().await.unwrap();
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM atomic_unit_pending_test".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(count, Some(SqlValue::Integer(1))),
        "the well-behaved atomic_unit call after the Pending misuse must \
             have actually committed its write; got {count:?}"
    );
}

/// ADR-136 D1 gate 1/3: with `KHIVE_WRITE_ROUTING=strict` and no writer
/// task available, `SqlBridge::writer()` must error instead of silently
/// degrading to a standalone connection — even when the reason no handle
/// exists is simply that the queue itself was never enabled. Strict
/// routing without an enabled queue is a caller misconfiguration this
/// gate refuses rather than silently no-ops: an operator who set
/// `KHIVE_WRITE_ROUTING=strict` believing every write is single-admission
/// must be told loudly if `KHIVE_WRITE_QUEUE` was never turned on, not
/// left thinking strict routing is in effect when it is not.
#[tokio::test]
async fn writer_strict_routing_fails_closed_without_writer_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict_writer.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let result = bridge.writer().await;
    let err = match result {
        Ok(_) => panic!(
            "KHIVE_WRITE_ROUTING=strict with no writer task must fail closed, not \
                 silently degrade to a standalone connection"
        ),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("strict"),
        "error must name strict routing, got: {err}"
    );
}

/// ADR-136 D1 gate 3/4: with `KHIVE_WRITE_ROUTING=strict` and no writer
/// task available, `SqlBridge::atomic_unit` must error instead of
/// silently falling back to a manual `BEGIN IMMEDIATE` on a standalone
/// connection.
#[tokio::test]
async fn atomic_unit_strict_routing_fails_closed_without_writer_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strict_atomic_unit.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(false),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let op: AtomicUnitOp = Box::new(|_writer| {
        Box::pin(async move { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) })
    });
    let result = bridge.atomic_unit(op).await;
    assert!(
        result.is_err(),
        "KHIVE_WRITE_ROUTING=strict but the queue is off (no writer task handle) must \
             fail closed instead of falling back to a manual BEGIN IMMEDIATE; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("strict"),
        "error must name strict routing, got: {msg}"
    );
}

/// ADR-136 D1 gate 3 amendment: production DOES read through a
/// queue-backed `writer()` handle — `khive-pack-comm`'s handlers obtain
/// a writer then call `w.query_row(...)` cursor-style, and
/// `khive-pack-gtd`'s bootstrap calls `w.query_all("PRAGMA
/// table_info...")` on one. Exercise that exact shape under a strict,
/// queue-enabled pool: write through the handle, then read the same row
/// back through it before it is dropped.
#[tokio::test]
async fn writer_handle_supports_read_after_write_under_strict_queue() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_read_after_write.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS writer_cursor_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }

    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut w = bridge.writer().await.unwrap();
    w.execute(SqlStatement {
        sql: "INSERT INTO writer_cursor_test (id, val) VALUES (?1, ?2)".into(),
        params: vec![SqlValue::Integer(1), SqlValue::Text("via-writer".into())],
        label: None,
    })
    .await
    .unwrap();

    let row = w
        .query_row(SqlStatement {
            sql: "SELECT val FROM writer_cursor_test WHERE id = ?1".into(),
            params: vec![SqlValue::Integer(1)],
            label: None,
        })
        .await
        .unwrap()
        .expect("row inserted through the same writer handle must be visible to it");
    assert!(
        matches!(&row.columns[0].value, SqlValue::Text(v) if v == "via-writer"),
        "query_row through a queue-backed writer handle must see its own \
             committed write; got {:?}",
        row.columns[0].value
    );
}

/// Queue-backed ordinary reads use the pooled reader budget rather than a
/// cached standalone connection. Arm 1 saturates the one reader permit and
/// proves the failure is a visible pooled-admission timeout. Arm 2 proves
/// a handle with no exceptional transaction connection remains reusable
/// through the pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_backed_read_uses_pool_budget_and_remains_reusable_after_saturation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("queue_backed_reader_budget.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        max_readers: 1,
        checkout_timeout: std::time::Duration::from_millis(250),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS reopen_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL)",
            )
            .unwrap();
    }
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut w = bridge.writer().await.unwrap();
    w.execute(SqlStatement {
        sql: "INSERT INTO reopen_test (id, val) VALUES (1, 'seed')".into(),
        params: vec![],
        label: None,
    })
    .await
    .unwrap();

    // Arm 1: with the sole reader permit held, the queue-backed read
    // must time out on the reader budget.
    let held = pool
        .sql_bridge_reader_slots()
        .acquire_owned()
        .await
        .unwrap();
    let starved = w
        .query_row(SqlStatement {
            sql: "SELECT val FROM reopen_test WHERE id = 1".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        matches!(
            &starved,
            Err(StorageError::AdmissionTimeout { operation, .. })
                if operation.as_ref() == "writer.query_row"
        ),
        "queue-backed read with reader permits saturated must time out \
             at the shared pooled-reader admission stage; \
             got {starved:?}"
    );
    let saturated = pool.reader_acquisition_snapshot();
    assert_eq!(saturated.checkout_timeouts, 1);
    assert_eq!(saturated.standalone_opens, 0);
    drop(held);

    // Arm 2: a queue-backed handle in the exact post-cancelled-read
    // state — `handle: None` — must serve the next ordinary read through
    // the pool, never a hard "connection already consumed" failure (that
    // contract is specific to a poisoned explicit-transaction reader).
    // Constructed directly so the state is deterministic.
    let writer_task = pool
        .writer_task_handle()
        .expect("queue-enabled file pool must offer a writer task")
        .expect("writer task present under write_queue_enabled");
    let mut post_cancel = SqliteWriter {
        observe_direct_errors: true,
        event_rows: None,
        handle: None,
        writer_task: Some(writer_task),
        origin: pool.origin(),
        db: crate::timeout_sink::db_label(&pool),
        pool: Arc::clone(&pool),
        held_lease: None,
    };
    let row = post_cancel
        .query_row(SqlStatement {
            sql: "SELECT val FROM reopen_test WHERE id = 1".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("read on a queue-backed handle with no transaction connection must pool")
        .expect("seeded row must be visible");
    assert!(
        matches!(&row.columns[0].value, SqlValue::Text(v) if v == "seed"),
        "pooled read must return the seeded row; got {:?}",
        row.columns[0].value
    );
}

/// ADR-136 D1 gate 3 amendment: `SqlWriter::query_row`/`query_all` carry
/// no read-only restriction at the trait level — a caller could hand a
/// DML-with-RETURNING statement to `query_row`. A queue-backed handle now
/// sends ordinary reads through a pooled read-only connection, so SQLite
/// rejects the statement instead of mutating on an untracked writer.
#[tokio::test]
async fn writer_query_row_rejects_dml_with_returning_on_queue_backed_handle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_readonly_returning.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS writer_returning_test \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL);
                     INSERT INTO writer_returning_test (id, val) VALUES (1, 'original');",
            )
            .unwrap();
    }

    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut w = bridge.writer().await.unwrap();
    let result = w
        .query_row(SqlStatement {
            sql: "UPDATE writer_returning_test SET val = 'mutated' \
                      WHERE id = ?1 RETURNING val"
                .into(),
            params: vec![SqlValue::Integer(1)],
            label: None,
        })
        .await;
    assert!(
        result.is_err(),
        "a DML-with-RETURNING statement through query_row on a \
             queue-backed writer handle must be rejected, not executed on \
             an untracked read-write connection; got {result:?}"
    );

    let mut reader = bridge.reader().await.unwrap();
    let val = reader
        .query_scalar(SqlStatement {
            sql: "SELECT val FROM writer_returning_test WHERE id = ?1".into(),
            params: vec![SqlValue::Integer(1)],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(&val, Some(SqlValue::Text(v)) if v == "original"),
        "the rejected UPDATE...RETURNING must not have altered the row; got {val:?}"
    );
}

/// The three queue-backed `SqlReader` methods on `SqliteWriter` classify
/// transaction control, but a queue-backed handle with no cached
/// transaction and no transaction-control statement falls through to the
/// same pooled reader route `PoolBackedReader`/`SqliteReader` use
/// (`run_pool_reader_query`) — without first running the statement
/// through `admit_reader_capability_sql`. A setting `PRAGMA` (unlike a
/// DML write) does not touch the main database file, so SQLite's own
/// read-only connection flag does not refuse it; without the admission
/// gate it would silently change connection-local state on a connection
/// this same pool later hands back out.
#[tokio::test]
async fn writer_query_row_rejects_setting_pragma_on_queue_backed_reader_route() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writer_reader_route_pragma_admission.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let mut w = bridge.writer().await.unwrap();
    let result = w
        .query_row(SqlStatement {
            sql: "PRAGMA cache_size = -999".into(),
            params: vec![],
            label: None,
        })
        .await;
    assert!(
        result.is_err(),
        "a setting PRAGMA through the queue-backed writer's reader route must be \
             refused, exactly like it is through PoolBackedReader/SqliteReader; \
             got {result:?}"
    );
}

/// ADR-136 D1 acceptance arm: a 5-op batch shaped like `[send, mark,
/// mark, mark, mark]` at the storage layer — every "mark" op is a real
/// `UPDATE` against its own pre-seeded row, so (like `send`) it routes
/// through the writer task rather than bypassing it as a `SELECT` would
/// — issued while 3 concurrent writers contend the write path, must
/// complete every op — no checkout timeout — once routing is strict and
/// the queue is on. An occupier holds the writer task's single drain
/// slot until all 8 requests (3 contenders + send + 4 marks) are
/// provably enqueued behind it (`queue_depth() >= 8`, the same
/// occupier/`queue_depth()` discriminator the migrated-call-site tests
/// use), so this proves genuine contention instead of a scheduler that
/// happens to drain the tiny writes before the others even enqueue.
/// Mirrors the measured production failure ADR-136's Context section
/// documents (middle ops of a batch starving while a sibling write wins
/// under the legacy fixed-deadline pool mutex). Red-proofed: reverting
/// the marks back to `bridge.reader()` `SELECT`s (the pre-fix shape)
/// makes the `queue_depth() >= 8` wait time out and fail, since a read
/// never reaches the writer task's channel — confirming this version
/// actually requires all four marks to be real writes.
#[tokio::test]
async fn acceptance_five_op_batch_completes_under_concurrent_write_contention() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("acceptance_batch.db");
    let config = PoolConfig {
        path: Some(path),
        write_queue_enabled: Some(true),
        write_routing_strict: true,
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let guard = pool.writer().unwrap();
        guard
            .conn()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS acceptance_batch \
                     (id INTEGER PRIMARY KEY, val TEXT NOT NULL);
                     INSERT INTO acceptance_batch (id, val) VALUES \
                     (200, 'seed-0'), (201, 'seed-1'), (202, 'seed-2'), (203, 'seed-3');",
            )
            .unwrap();
    }

    let bridge = Arc::new(SqlBridge::new(Arc::clone(&pool), true));

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned for a file-backed pool with the flag on");

    // Occupier: holds the single writer-task drain slot until released,
    // so every op below is provably queued behind it rather than racing
    // to finish before the others even enqueue (same technique as
    // `rename_namespace_routes_through_writer_task_when_flag_enabled` in
    // `stores::text_tests`).
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };
    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    // 3 concurrent writers contending the write path — each a
    // self-contained `execute()` through `SqlBridge::writer()`, matching
    // the "several short acquisitions" shape ADR-136's Context section
    // measures (a logical write is many short holds, not one long one).
    let contenders: Vec<_> = (0..3)
        .map(|i| {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move {
                let mut writer = bridge.writer().await?;
                writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO acceptance_batch (id, val) VALUES (?1, ?2)".into(),
                        params: vec![
                            SqlValue::Integer(100 + i),
                            SqlValue::Text(format!("contender-{i}")),
                        ],
                        label: None,
                    })
                    .await
            })
        })
        .collect();

    // The 5-op batch: [send, mark, mark, mark, mark].
    let send = {
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move {
            let mut writer = bridge.writer().await?;
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO acceptance_batch (id, val) VALUES (?1, ?2)".into(),
                    params: vec![SqlValue::Integer(1), SqlValue::Text("send".into())],
                    label: None,
                })
                .await
        })
    };
    let marks: Vec<_> = (0..4)
        .map(|i| {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move {
                let mut writer = bridge.writer().await?;
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE acceptance_batch SET val = ?2 WHERE id = ?1".into(),
                        params: vec![
                            SqlValue::Integer(200 + i),
                            SqlValue::Text(format!("marked-{i}")),
                        ],
                        label: None,
                    })
                    .await
            })
        })
        .collect();

    // All 8 requests must actually reach the writer task's channel
    // while the occupier still holds the single drain slot.
    let mut saw_all_enqueued = false;
    for _ in 0..200 {
        if writer_task.queue_depth() >= 8 {
            saw_all_enqueued = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        saw_all_enqueued,
        "not all 8 contending writes (3 contenders + send + 4 marks) reached \
             the writer task's channel while the occupier held the single drain \
             slot — got depth {}",
        writer_task.queue_depth()
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");

    for c in contenders {
        c.await
            .expect("contender task must not panic")
            .expect("contender write must complete without a checkout timeout");
    }
    send.await
        .expect("send task must not panic")
        .expect("send op must complete without a checkout timeout");
    for (i, m) in marks.into_iter().enumerate() {
        let affected = m
            .await
            .expect("mark task must not panic")
            .expect("mark op must complete without a checkout timeout — no starvation");
        assert_eq!(
            affected, 1,
            "mark {i} must have updated exactly its own row"
        );
    }

    let mut reader = bridge.reader().await.unwrap();
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM acceptance_batch".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(count, Some(SqlValue::Integer(8))),
        "the 4 seeded mark rows plus 3 contenders plus the batch's own send \
             must all be present; got {count:?}"
    );

    for i in 0..4i64 {
        let mut reader = bridge.reader().await.unwrap();
        let val = reader
            .query_scalar(SqlStatement {
                sql: "SELECT val FROM acceptance_batch WHERE id = ?1".into(),
                params: vec![SqlValue::Integer(200 + i)],
                label: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(&val, Some(SqlValue::Text(v)) if *v == format!("marked-{i}")),
            "mark row {i} must reflect the persisted UPDATE after release; got {val:?}"
        );
    }
}

#[tokio::test]
async fn standalone_writer_handle_resamples_reserve_before_each_operation() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for operation in [
        "execute",
        "execute_batch",
        "execute_script",
        "top_level_vacuum",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut pool = ConnectionPool::new(PoolConfig {
            path: Some(dir.path().join(format!("reserve-{operation}.db"))),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .unwrap();
        let samples = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&samples);
        pool.set_test_write_admission(100, move |_| {
            match observed.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(101), // First operation; handle open does not sample.
                1 => Ok(100), // The same handle's next operation.
                extra => panic!("unexpected capacity sample {extra}"),
            }
        });
        let bridge = SqlBridge::new(Arc::new(pool), true);
        let mut writer = bridge.writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "CREATE TABLE reserve_test (id INTEGER PRIMARY KEY)".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();

        let insert = || SqlStatement {
            sql: "INSERT INTO reserve_test (id) VALUES (1)".into(),
            params: vec![],
            label: None,
        };
        let error = match operation {
            "execute" => writer.execute(insert()).await.map(|_| ()),
            "execute_batch" => writer.execute_batch(vec![insert()]).await.map(|_| ()),
            "execute_script" => {
                writer
                    .execute_script("INSERT INTO reserve_test (id) VALUES (1)".into())
                    .await
            }
            "top_level_vacuum" => {
                writer
                    .execute_script_top_level(TopLevelMaintenance::Vacuum)
                    .await
            }
            _ => unreachable!(),
        }
        .expect_err("the second operation must see the new reserve sample");
        assert!(
            matches!(
                &error,
                StorageError::CapacityFloor {
                    available_bytes,
                    floor_bytes,
                    ..
                } if *available_bytes == 100 && *floor_bytes == 100
            ),
            "{operation} must retain typed capacity-floor classification: {error:?}"
        );
        assert_eq!(samples.load(Ordering::SeqCst), 2, "{operation}");
        assert!(
            matches!(
                writer
                    .query_scalar(SqlStatement {
                        sql: "SELECT COUNT(*) FROM reserve_test".into(),
                        params: vec![],
                        label: None,
                    })
                    .await
                    .unwrap(),
                Some(SqlValue::Integer(0))
            ),
            "{operation} must not write after the reserve refusal"
        );
    }
}

#[tokio::test]
async fn file_backed_bridge_counts_writer_and_flag_off_atomic_unit_acquisitions() {
    let dir = tempfile::tempdir().unwrap();
    let config = PoolConfig {
        path: Some(dir.path().join("bridge_writer_acquisitions.db")),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let before = pool.writer_acquisition_snapshot();

    drop(bridge.writer().await.unwrap());
    let after_writer = pool.writer_acquisition_snapshot();
    assert_eq!(
        after_writer.standalone_acquisitions,
        before.standalone_acquisitions + 1
    );
    assert_eq!(after_writer.acquisitions, before.acquisitions + 1);
    assert_eq!(after_writer.pooled_acquisitions, before.pooled_acquisitions);
    assert_eq!(
        after_writer.writer_task_acquisitions,
        before.writer_task_acquisitions
    );

    let op: AtomicUnitOp =
        Box::new(|_writer| Box::pin(async { Ok(Box::new(()) as Box<dyn std::any::Any + Send>) }));
    bridge.atomic_unit(op).await.unwrap();

    let after_atomic_unit = pool.writer_acquisition_snapshot();
    assert_eq!(
        after_atomic_unit.standalone_acquisitions,
        before.standalone_acquisitions + 2
    );
    assert_eq!(after_atomic_unit.acquisitions, before.acquisitions + 2);
    assert_eq!(
        after_atomic_unit.pooled_acquisitions,
        before.pooled_acquisitions
    );
    assert_eq!(
        after_atomic_unit.writer_task_acquisitions,
        before.writer_task_acquisitions
    );
}

/// `max_completed_hold_operation` must name the caller's own read, so a
/// recorded maximum hold can be attributed to the query that caused it.
///
/// The pool's attribution machinery was already correct and covered by
/// `longest_completed_hold_names_its_operation` in `pool.rs`. What defeated
/// it was this bridge: every pooled read handed the slot one shared
/// constant, so the field was two-valued across the whole product while
/// still returning a plausible-looking name (#2793).
///
/// The discriminating assertion is the last one, that the two arms DIFFER.
/// A single constant satisfies any per-arm expectation you write; only a
/// pair of distinct labels shows that the caller's name reached the slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_completed_hold_operation_names_the_caller_read_not_a_bridge_constant() {
    async fn recorded_hold_operation(through_writer: bool) -> (Option<&'static str>, u64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hold_attribution.db");
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                write_queue_enabled: Some(true),
                write_routing_strict: true,
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        {
            let guard = pool.writer().unwrap();
            guard
                .conn()
                .execute_batch("CREATE TABLE IF NOT EXISTS hold_attr (id INTEGER PRIMARY KEY)")
                .unwrap();
        }
        let bridge = SqlBridge::new(Arc::clone(&pool), true);
        let statement = SqlStatement {
            sql: "SELECT id FROM hold_attr".into(),
            params: vec![],
            label: None,
        };
        if through_writer {
            let mut w = bridge.writer().await.unwrap();
            w.query_row(statement).await.unwrap();
        } else {
            let mut r = bridge.reader().await.unwrap();
            r.query_all(statement).await.unwrap();
        }
        let snapshot = pool.reader_acquisition_snapshot();
        (
            snapshot.max_completed_hold_operation,
            snapshot.completed_pooled_checkouts,
        )
    }

    let (read_side, read_completed) = recorded_hold_operation(false).await;
    let (write_side, write_completed) = recorded_hold_operation(true).await;

    assert!(
        read_completed >= 1 && write_completed >= 1,
        "both arms must complete a pooled checkout, or the attribution \
             below is reading an empty population; got {read_completed} and \
             {write_completed}"
    );
    // The discriminating assertion runs FIRST, deliberately. Under the
    // shared-constant behaviour both arms collapse to one value, and if a
    // per-arm equality ran ahead of this it would fire instead, so the
    // assertion that actually separates "attributed" from "constant" would
    // never execute in the case it exists for.
    assert_ne!(
        read_side, write_side,
        "the recorded operation must tell two different reads apart; a \
             shared bridge constant makes these equal while still looking \
             like an answer"
    );
    assert_eq!(
        read_side,
        Some("query_all"),
        "a pooled read drawn through the reader must record its own operation"
    );
    assert_eq!(
        write_side,
        Some("writer.query_row"),
        "a pooled read drawn through the writer must record its own operation"
    );
}
