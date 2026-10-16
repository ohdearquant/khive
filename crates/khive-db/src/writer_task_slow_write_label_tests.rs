//! `slow_write` labels: a queued write that meets the slow-write threshold
//! must name the statements the writer ran (or the submit site's operation
//! name), so a writer held for tens of seconds can be attributed.
//!
//! Every timing test drives a real `WriterTask` and reads the emitted row back
//! through the sink's process-global capture (`timeout_sink::SlowWriteCapture`),
//! armed only for the duration of one test. The slow-write threshold is a 1 ms
//! env override read at writer-task spawn, so each timing test runs its body in
//! an isolated child process (`crate::test_process`), the same seam the crate's
//! other environment-dependent tests use. The op sleeps, so the span is
//! over-threshold without depending on scheduler timing.
//!
//! A second group drives the queue-backed `SqlAccess::writer()` handle, so the
//! labels a submit site holds are shown to reach the `slow_write` row through
//! the SQL bridge's `execute` / `execute_batch`, not only through the
//! `WriterTaskHandle` entry points the first group exercises.

use super::*;
use crate::pool::PoolConfig;
use crate::timeout_sink::{SlowWriteCapture, SlowWriteLabels, SLOW_WRITE_CAPTURE_LOCK};
use crate::SqlBridge;
use khive_storage::types::SqlStatement;
use khive_storage::SqlAccess as _;

/// A file-backed pool for one test database. `super::tests`'s equivalent is
/// private to that module, so the sibling `#[path]` module keeps its own.
fn file_pool(path: &std::path::Path) -> ConnectionPool {
    ConnectionPool::new(PoolConfig {
        path: Some(path.to_path_buf()),
        ..PoolConfig::for_test()
    })
    .expect("pool open")
}

/// A file-backed pool that already holds `table`, so a labelled write issued
/// through the bridge has a real destination. The bridge's `writer()` spawns
/// the pool's writer task on first use, which is where the slow-write
/// threshold override is read.
fn pool_with_slow_write_table(path: &std::path::Path, table: &str) -> Arc<ConnectionPool> {
    let pool = Arc::new(file_pool(path));
    {
        let guard = pool.writer().expect("pool writer");
        guard
            .conn()
            .execute_batch(&format!("CREATE TABLE {table} (value INTEGER)"))
            .expect("create slow-write destination");
    }
    pool
}

/// A labelled INSERT whose recursive CTE writes enough rows to push the whole
/// queued request past the 1 ms threshold this suite installs. `label` is what
/// the writer-stage observation and the `slow_write` row must name.
fn slow_labelled_insert(table: &str, label: &str) -> SqlStatement {
    SqlStatement {
        sql: format!(
            "INSERT INTO {table}(value) \
             WITH RECURSIVE rows(value) AS (\
             SELECT 1 UNION ALL SELECT value + 1 FROM rows WHERE value < 100000\
             ) SELECT value FROM rows"
        ),
        params: vec![],
        label: Some(label.to_string()),
    }
}

/// Force the slow-write threshold to 1 ms for this test only by re-running its
/// body in an isolated child process. Returns `true` in the parent (the caller
/// returns early to let the child do the work) and `false` in the child, whose
/// environment `configure` has already redirected: `KHIVE_*` cleared, an
/// isolated `HOME` and sink directory, and the threshold override set.
fn slow_write_threshold_in_child() -> bool {
    let root = tempfile::tempdir().unwrap();
    crate::test_process::run_in_child(|command| {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_SLOW_WRITE_THRESHOLD_MS", "1")
            .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", root.path().join("sink"));
    })
}

/// The single captured row for `name`. The capture is process-global, so a test
/// selects its own db path rather than assuming it is the only entry.
fn row_for<'a>(rows: &'a [(String, SlowWriteLabels)], name: &str) -> &'a SlowWriteLabels {
    let matched: Vec<&(String, SlowWriteLabels)> =
        rows.iter().filter(|(db, _)| db.contains(name)).collect();
    assert_eq!(
        matched.len(),
        1,
        "expected exactly one captured slow_write row for {name}"
    );
    &matched[0].1
}

#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn slow_labelled_write_names_the_statement_label() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_positive.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let capture = SlowWriteCapture::arm();
    handle
        .send_labelled(
            |_conn| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(42u8)
            },
            vec!["resolve_prefix".to_string()],
        )
        .await
        .expect("slow write should succeed");
    let rows = capture.drain();
    let row = row_for(&rows, "slow_label_positive.db");

    assert_eq!(row.labels.as_ref(), ["resolve_prefix"]);
    assert_eq!(row.labels_truncated, None);
}

#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn slow_unlabelled_named_write_names_the_site() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_site.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let capture = SlowWriteCapture::arm();
    handle
        .send_at(
            |_conn| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            },
            "append_events",
        )
        .await
        .expect("slow write should succeed");
    let rows = capture.drain();
    let row = row_for(&rows, "slow_label_site.db");

    assert_eq!(row.labels.as_ref(), ["append_events"]);
    assert_eq!(row.labels_truncated, None);
}

#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn slow_opaque_closure_emits_empty_labels() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_opaque.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let capture = SlowWriteCapture::arm();
    handle
        .send(|_conn| {
            std::thread::sleep(Duration::from_millis(50));
            Ok(())
        })
        .await
        .expect("slow write should succeed");
    let rows = capture.drain();
    let row = row_for(&rows, "slow_label_opaque.db");

    assert!(row.labels.is_empty());
    assert_eq!(row.labels_truncated, None);
}

#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn slow_write_deduplicates_labels() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_dedup.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let capture = SlowWriteCapture::arm();
    handle
        .send_labelled(
            |_conn| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            },
            vec![
                "b".to_string(),
                "a".to_string(),
                "b".to_string(),
                "a".to_string(),
            ],
        )
        .await
        .expect("slow write should succeed");
    let rows = capture.drain();
    let row = row_for(&rows, "slow_label_dedup.db");

    assert_eq!(row.labels.as_ref(), ["b", "a"]);
    assert_eq!(row.labels_truncated, None);
}

#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn slow_write_bounds_labels_and_counts_the_overflow() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_overflow.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let provided: Vec<String> = (0..MAX_WRITE_LABELS + 3)
        .map(|n| format!("label-{n}"))
        .collect();
    let expected: Vec<String> = provided[..MAX_WRITE_LABELS].to_vec();

    let capture = SlowWriteCapture::arm();
    handle
        .send_labelled(
            |_conn| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            },
            provided,
        )
        .await
        .expect("slow write should succeed");
    let rows = capture.drain();
    let row = row_for(&rows, "slow_label_overflow.db");

    assert_eq!(row.labels.as_ref(), expected.as_slice());
    assert_eq!(row.labels_truncated, Some(3));
}

/// The default 1 s threshold applies here, so an op that returns immediately is
/// well under it and must emit no row. Runs in the parent (no env override
/// needed); the row filter keeps a concurrent sibling writer's row from
/// satisfying the assertion.
#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn fast_write_emits_no_slow_write_row() {
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_fast.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let capture = SlowWriteCapture::arm();
    handle
        .send_labelled(|_conn| Ok(()), vec!["resolve_prefix".to_string()])
        .await
        .expect("fast write should succeed");
    let rows = capture.drain();

    assert!(
        rows.iter()
            .all(|(db, _)| !db.contains("slow_label_fast.db")),
        "a fast write must not emit a slow_write row"
    );
}

/// The slow-path-only resolution pair, fast arm: a write that never crosses the
/// threshold must leave no labels on its writer-stage observation even when the
/// submit site named them, because `WriteTelemetry::finish` resolves labels only
/// on the slow path. Runs in the parent under the default 1 s threshold; both
/// labelled entry points are exercised. Fails if labels are resolved on every
/// write (`finish`'s slow gate mutated to always resolve).
#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn fast_write_never_resolves_labels_on_the_observation() {
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("fast_label_unresolved.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    handle
        .send_bounded_labelled(|_conn| Ok(()), vec!["resolve_prefix".to_string()])
        .await
        .expect("fast write with a statement-label list should succeed");
    let from_statements = last_writer_stage_observation(&pool).expect("writer stage observation");
    assert!(
        from_statements.labels.is_empty(),
        "a fast write must not resolve labels on its observation; got {:?}",
        from_statements.labels
    );
    assert_eq!(from_statements.labels_truncated, None);

    handle
        .send_bounded_at(|_conn| Ok(()), "resolve_prefix")
        .await
        .expect("fast write with a site name should succeed");
    let from_site = last_writer_stage_observation(&pool).expect("writer stage observation");
    assert!(
        from_site.labels.is_empty(),
        "a fast write must not resolve a site name on its observation; got {:?}",
        from_site.labels
    );
    assert_eq!(from_site.labels_truncated, None);
}

/// The slow-path-only resolution pair, slow arm: the same labels on the same
/// child-threshold harness are resolved once the write crosses the threshold,
/// and the row the sink would write projects them. Fails if labels are never
/// resolved (`finish`'s slow branch dropping `resolve_labels`) or if the sink
/// projection drops them.
#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn slow_write_resolves_labels_on_the_observation_and_row() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = file_pool(&dir.path().join("slow_label_observation.db"));
    let handle = spawn(&pool, 8).expect("writer task should spawn");

    let capture = SlowWriteCapture::arm();
    handle
        .send_bounded_labelled(
            |_conn| {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            },
            vec!["resolve_prefix".to_string()],
        )
        .await
        .expect("slow labelled write should succeed");
    let rows = capture.drain();
    let row = row_for(&rows, "slow_label_observation.db");
    assert_eq!(row.labels.as_ref(), ["resolve_prefix"]);
    assert_eq!(row.labels_truncated, None);

    let observation = last_writer_stage_observation(&pool).expect("writer stage observation");
    assert_eq!(observation.labels.as_slice(), ["resolve_prefix"]);
    assert_eq!(observation.labels_truncated, None);
    let projected = crate::timeout_sink::slow_write_labels_for_test(&observation);
    assert_eq!(projected.labels.as_ref(), ["resolve_prefix"]);
    assert_eq!(projected.labels_truncated, None);
}

/// A slow write issued through the bridge's `execute` carries its statement's
/// label onto the `slow_write` row. This is the production line
/// `collect_statement_labels(std::slice::from_ref(&statement))` feeding
/// `send_bounded_labelled`; it fails if the bridge passes an empty label list.
#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn bridge_execute_carries_the_statement_label_onto_the_slow_row() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_with_slow_write_table(
        &dir.path().join("bridge_slow_execute.db"),
        "bridge_slow_execute",
    );
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let capture = SlowWriteCapture::arm();
    let mut writer = bridge.writer().await.expect("queue-backed writer handle");
    let affected = khive_storage::SqlWriter::execute(
        &mut *writer,
        slow_labelled_insert("bridge_slow_execute", "entity-upsert"),
    )
    .await
    .expect("labelled slow write should succeed");
    assert!(affected > 0, "the slow insert must report written rows");
    let rows = capture.drain();
    let row = row_for(&rows, "bridge_slow_execute.db");

    assert_eq!(row.labels.as_ref(), ["entity-upsert"]);
    assert_eq!(row.labels_truncated, None);
}

/// A slow batch issued through the bridge's `execute_batch` carries the labels
/// its labelled statements hold onto the `slow_write` row, deduplicated in
/// first-seen order — the label repeated across the two statements is named
/// once. This is the production line `collect_statement_labels(&statements)`
/// feeding `send_bounded_labelled`; it fails if the bridge passes an empty
/// label list, which is the mutation this test pins.
#[tokio::test]
#[serial_test::serial(writer_timeout_sink_volume)]
async fn bridge_execute_batch_carries_the_statement_labels_onto_the_slow_row() {
    if slow_write_threshold_in_child() {
        return;
    }
    let _guard = SLOW_WRITE_CAPTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_with_slow_write_table(
        &dir.path().join("bridge_slow_batch.db"),
        "bridge_slow_batch",
    );
    let bridge = SqlBridge::new(Arc::clone(&pool), true);

    let capture = SlowWriteCapture::arm();
    let mut writer = bridge.writer().await.expect("queue-backed writer handle");
    let statements = vec![
        slow_labelled_insert("bridge_slow_batch", "entity-upsert"),
        slow_labelled_insert("bridge_slow_batch", "entity-upsert"),
    ];
    let affected = khive_storage::SqlWriter::execute_batch(&mut *writer, statements)
        .await
        .expect("labelled slow batch should succeed");
    assert!(affected > 0, "the slow batch must report written rows");
    let rows = capture.drain();
    let row = row_for(&rows, "bridge_slow_batch.db");

    assert_eq!(row.labels.as_ref(), ["entity-upsert"]);
    assert_eq!(row.labels_truncated, None);
}

/// Pure projection: labels a request ran are deduplicated in first-seen order,
/// with unlabelled statements contributing nothing — no writer task, no
/// capture, no timing.
#[test]
fn statement_labels_dedup_in_first_seen_order() {
    let statements = vec![
        SqlStatement::new("SELECT 1", vec![]).labelled("b"),
        SqlStatement::new("SELECT 2", vec![]).labelled("a"),
        SqlStatement::new("SELECT 3", vec![]).labelled("b"),
        SqlStatement::new("SELECT 4", vec![]),
    ];
    assert_eq!(collect_statement_labels(&statements), vec!["b", "a"]);
}
