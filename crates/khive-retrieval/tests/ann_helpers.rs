#![cfg(feature = "ann")]

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use khive_retrieval::ann::{
    acquire_checkpoint_lock, acquire_checkpoint_lock_async, rotation_watch_loop,
};
use tokio_util::sync::CancellationToken;

#[test]
fn empty_tail_preserves_candidate_order_duplicates_and_score_bits() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let high_id = Uuid::from_u128(9);
    let low_id = Uuid::from_u128(1);
    let nan = f64::from_bits(0x7ff8_0000_0000_0042);
    let candidates = vec![(high_id, 0.25_f64), (high_id, nan), (low_id, 0.75)];
    let expected: Vec<_> = candidates
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    let merged = merge_fresh_tail(candidates, Vec::new(), |_| -> Result<f64, &'static str> {
        panic!("empty tail must not score")
    })
    .expect("empty tail");
    let actual: Vec<_> = merged
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn tail_merge_preserves_f64_precision_and_descending_order() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let low = 0.5_f64;
    let high = f64::from_bits(low.to_bits() + 1);
    let carried_high = f64::from_bits(low.to_bits() + 2);
    assert_eq!(
        (low as f32).to_bits(),
        (high as f32).to_bits(),
        "fixture detects narrowing"
    );
    assert_eq!((low as f32).to_bits(), (carried_high as f32).to_bits());
    let low_id = Uuid::from_u128(1);
    let high_id = Uuid::from_u128(9);
    let carried_id = Uuid::from_u128(7);
    let mut calls = 0;
    let merged = merge_fresh_tail(
        vec![(low_id, low), (carried_id, carried_high)],
        vec![(high_id, Some(vec![1.0]))],
        |embedding| {
            assert_eq!(embedding, &[1.0]);
            calls += 1;
            Ok::<_, &'static str>(high)
        },
    )
    .expect("score tail");
    assert_eq!(calls, 1);
    let actual: Vec<_> = merged
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    assert_eq!(
        actual,
        vec![
            (carried_id, carried_high.to_bits()),
            (high_id, high.to_bits()),
            (low_id, low.to_bits())
        ]
    );
}

#[test]
fn tail_merge_scores_repeated_upserts_in_order_and_stops_at_first_error() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let repeated = Uuid::from_u128(1);
    let deleted = Uuid::from_u128(2);
    let last = Uuid::from_u128(3);
    let ops = vec![
        (repeated, Some(vec![1.0])),
        (deleted, None),
        (repeated, Some(vec![2.0])),
        (last, Some(vec![3.0])),
    ];
    let mut calls = Vec::new();
    let merged = merge_fresh_tail(Vec::new(), ops.clone(), |embedding| {
        calls.push(embedding[0]);
        Ok::<_, (&'static str, u32)>(f64::from(embedding[0]))
    })
    .expect("successful callback");
    assert_eq!(calls, vec![1.0, 2.0, 3.0]);
    assert_eq!(merged, vec![(last, 3.0), (repeated, 2.0)]);

    calls.clear();
    let error = merge_fresh_tail(vec![(deleted, 9.0)], ops, |embedding| {
        calls.push(embedding[0]);
        if embedding[0] == 2.0 {
            Err(("score rejected", 42_u32))
        } else {
            Ok(f64::from(embedding[0]))
        }
    })
    .expect_err("first failure must propagate");
    assert_eq!(error, ("score rejected", 42));
    assert_eq!(calls, vec![1.0, 2.0]);
}

#[test]
fn tail_merge_preserves_replacement_delete_and_uncoalesced_upsert_semantics() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let updated = Uuid::from_u128(1);
    let deleted = Uuid::from_u128(2);
    let untouched = Uuid::from_u128(3);
    let upsert_then_delete = Uuid::from_u128(4);
    let delete_then_upsert = Uuid::from_u128(5);
    let candidates = vec![
        (updated, 0.1),
        (updated, 0.2),
        (deleted, 0.9),
        (untouched, 0.75),
        (untouched, 0.5),
        (upsert_then_delete, 0.1),
        (delete_then_upsert, 0.1),
    ];
    let ops = vec![
        (upsert_then_delete, Some(vec![4.0])),
        (upsert_then_delete, None),
        (delete_then_upsert, None),
        (delete_then_upsert, Some(vec![5.0])),
        (updated, Some(vec![6.0])),
        (updated, Some(vec![7.0])),
        (deleted, None),
    ];
    let mut calls = Vec::new();
    let merged = merge_fresh_tail(candidates, ops, |embedding| {
        calls.push(embedding[0]);
        Ok::<_, &'static str>(f64::from(embedding[0]))
    })
    .expect("score upserts");
    assert_eq!(calls, vec![4.0, 5.0, 6.0, 7.0]);
    assert_eq!(
        merged,
        vec![
            (updated, 7.0),
            (delete_then_upsert, 5.0),
            (upsert_then_delete, 4.0),
            (untouched, 0.75),
            (untouched, 0.5),
        ]
    );
}

#[test]
fn tail_merge_orders_equal_scores_and_incomparable_scores_by_uuid() {
    use khive_retrieval::ann::merge_fresh_tail;
    use uuid::Uuid;

    let low = Uuid::from_u128(1);
    let middle = Uuid::from_u128(2);
    let high = Uuid::from_u128(3);
    let merged = merge_fresh_tail(
        vec![(middle, 1.0_f32)],
        vec![(high, Some(vec![1.0])), (low, Some(vec![1.0]))],
        |embedding| Ok::<_, &'static str>(embedding[0]),
    )
    .expect("equal scores");
    assert_eq!(merged, vec![(low, 1.0), (middle, 1.0), (high, 1.0)]);

    let low_nan = f64::from_bits(0x7ff8_0000_0000_0011);
    let high_nan = f64::from_bits(0x7ff8_0000_0000_0022);
    let merged = merge_fresh_tail(
        vec![(high, high_nan), (low, low_nan)],
        vec![(middle, None)],
        |_| -> Result<f64, &'static str> { panic!("deletes must not score") },
    )
    .expect("incomparable scores");
    let actual: Vec<_> = merged
        .iter()
        .map(|(id, score)| (*id, score.to_bits()))
        .collect();
    assert_eq!(
        actual,
        vec![(low, low_nan.to_bits()), (high, high_nan.to_bits())]
    );
}

/// A scratch directory under the system temp dir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let unique = format!("khive-retrieval-ann-{}-{name}", std::process::id());
        let path = std::env::temp_dir().join(unique);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A path whose parent is a regular file, so the directory cannot be created.
fn blocked_directory(scratch: &ScratchDir) -> PathBuf {
    let blocker = scratch.path().join("blocker");
    std::fs::write(&blocker, b"regular file").expect("write blocker file");
    blocker.join("segment")
}

#[test]
fn create_directory_error_carries_the_prefix() {
    let scratch = ScratchDir::new("create-error");
    let dir = blocked_directory(&scratch);
    let shown = dir.display();

    for prefix in ["memory ANN", "ANN bridge"] {
        let result = acquire_checkpoint_lock(&dir, prefix);
        let error = result.expect_err("lock must fail");
        let expected = format!("create {prefix} checkpoint directory {shown}: ");
        assert!(error.starts_with(&expected), "got: {error}");
    }
}

#[test]
fn open_error_carries_the_prefix() {
    let scratch = ScratchDir::new("open-error");
    let lock_path = scratch.path().join(".bridge-checkpoint.lock");
    std::fs::create_dir(&lock_path).expect("occupy the lock path");
    let shown = lock_path.display();

    for prefix in ["memory ANN", "ANN bridge"] {
        let result = acquire_checkpoint_lock(scratch.path(), prefix);
        let error = result.expect_err("lock must fail");
        let expected = format!("open {prefix} lock {shown}: ");
        assert!(error.starts_with(&expected), "got: {error}");
    }
}

#[tokio::test]
async fn async_lock_forwards_the_prefix() {
    let scratch = ScratchDir::new("async-error");
    let dir = blocked_directory(&scratch);
    let shown = dir.display();

    let result = acquire_checkpoint_lock_async(dir.clone(), "memory ANN").await;
    let error = result.expect_err("lock must fail");
    let expected = format!("create memory ANN checkpoint directory {shown}: ");
    assert!(error.starts_with(&expected), "got: {error}");
}

#[test]
fn second_acquisition_waits_for_the_first_to_release() {
    let scratch = ScratchDir::new("serialize");
    let attempt = acquire_checkpoint_lock(scratch.path(), "ANN bridge");
    let first = attempt.expect("first lock");

    let dir = scratch.path().to_path_buf();
    let (sender, receiver) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let attempt = acquire_checkpoint_lock(&dir, "ANN bridge");
        let second = attempt.expect("second lock");
        sender.send(()).expect("report acquisition");
        drop(second);
    });

    let early = receiver.recv_timeout(Duration::from_millis(300));
    assert!(early.is_err(), "lock granted while held");

    drop(first);
    let late = receiver.recv_timeout(Duration::from_secs(10));
    assert!(late.is_ok(), "lock not granted after release");
    waiter.join().expect("waiter thread");
}

#[test]
fn watcher_claims_before_poll_without_retaining_the_ann() {
    use khive_retrieval::ann::rotation_watch_future;
    use std::sync::atomic::AtomicBool;

    let ann = Arc::new(());
    let weak = Arc::downgrade(&ann);
    let started = AtomicBool::new(false);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    // An ordinary test has no Tokio runtime. Construction must not start a timer.
    let watch = rotation_watch_future(
        &ann,
        &started,
        PathBuf::from("unpolled-root"),
        Duration::from_secs(5),
        CancellationToken::new(),
        move |_, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            std::future::ready(())
        },
    )
    .expect("first call claims the watcher");
    assert!(started.load(Ordering::Acquire));
    assert_eq!(Arc::strong_count(&ann), 1);
    assert!(rotation_watch_future(
        &ann,
        &started,
        PathBuf::from("duplicate-root"),
        Duration::from_secs(5),
        CancellationToken::new(),
        |_, _| std::future::ready(()),
    )
    .is_none());
    drop(ann);
    assert!(
        weak.upgrade().is_none(),
        "the unpolled future must not retain the ANN"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(watch);
    assert!(
        started.load(Ordering::Acquire),
        "dropping the future does not reset the one-shot guard"
    );
}

#[tokio::test]
async fn loop_returns_promptly_after_shutdown() {
    let shutdown = CancellationToken::new();
    let ticks = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ticks);
    // The interval is an hour, so a loop that honours shutdown never ticks. A loop that
    // ignored shutdown would tick; breaking on the third tick keeps that failure bounded.
    let tick = move || {
        let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if seen < 3 {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        }
    };
    let watch = rotation_watch_loop(Duration::from_secs(3600), shutdown.clone(), tick);

    let handle = tokio::spawn(watch);
    tokio::task::yield_now().await;
    shutdown.cancel();

    let finished = tokio::time::timeout(Duration::from_secs(10), handle).await;
    assert!(finished.is_ok(), "no return after shutdown");
    assert_eq!(ticks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn loop_stops_when_the_tick_breaks() {
    let shutdown = CancellationToken::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let tick = move || {
        let seen = counter.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            if seen < 3 {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        }
    };
    let watch = rotation_watch_loop(Duration::from_millis(10), shutdown, tick);

    let finished = tokio::time::timeout(Duration::from_secs(10), watch).await;
    assert!(finished.is_ok(), "loop ran past a break");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

mod registry_reader_tests {
    use async_trait::async_trait;
    use khive_db::StorageBackend;
    use khive_retrieval::ann::corpus::{CorpusScope, TailFloor, WatermarkCapture};
    use khive_retrieval::ann::registry::{min_watermark_on, read_watermark_on};
    use khive_storage::types::{SqlColumn, SqlRow, SqlStatement, SqlValue};
    use khive_storage::{SqlAccess, SqlReader, StorageError, StorageResult};

    struct ReaderSpy {
        result: Option<StorageResult<Vec<SqlRow>>>,
        statements: Vec<SqlStatement>,
    }

    #[async_trait]
    impl SqlReader for ReaderSpy {
        async fn query_all(&mut self, statement: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            self.statements.push(statement);
            self.result.take().expect("exactly one read")
        }
        async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
            panic!("must retain query_all routing")
        }
        async fn query_scalar(&mut self, _: SqlStatement) -> StorageResult<Option<SqlValue>> {
            panic!("must retain query_all routing")
        }
        async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
            panic!("must not explain")
        }
    }

    fn row(column: &str, value: SqlValue) -> SqlRow {
        SqlRow {
            columns: vec![SqlColumn {
                name: column.into(),
                value,
            }],
        }
    }

    async fn read(spy: &mut ReaderSpy, minimum: bool) -> StorageResult<Option<i64>> {
        if minimum {
            min_watermark_on(spy, "memory_", "local'quoted", "model").await
        } else {
            read_watermark_on(
                spy,
                "note_search_ann_consumer_snapshot",
                "consumer",
                "local'quoted",
                "model",
            )
            .await
        }
    }

    #[tokio::test]
    async fn existing_reader_decoding_labels_and_errors_are_preserved() {
        for minimum in [false, true] {
            let column = if minimum { "m" } else { "watermark" };
            let mut cases = vec![
                (vec![], None),
                (vec![row("wrong", SqlValue::Integer(8))], None),
            ];
            for value in [0, 17, i64::MAX, -1, -2] {
                cases.push((vec![row(column, SqlValue::Integer(value))], Some(value)));
            }
            for value in [
                SqlValue::Null,
                SqlValue::Bool(true),
                SqlValue::Float(7.0),
                SqlValue::Text("7".into()),
                SqlValue::Blob(vec![7]),
                SqlValue::Json(serde_json::json!(7)),
                SqlValue::Uuid(uuid::Uuid::nil()),
                SqlValue::Timestamp(chrono::DateTime::UNIX_EPOCH),
            ] {
                cases.push((vec![row(column, value)], None));
            }
            cases.push((
                vec![
                    row(column, SqlValue::Integer(7)),
                    row(column, SqlValue::Integer(9)),
                ],
                Some(7),
            ));
            let mut duplicate = row(column, SqlValue::Null);
            duplicate.columns.push(SqlColumn {
                name: column.into(),
                value: SqlValue::Integer(9),
            });
            cases.push((vec![duplicate], None));
            for (rows, expected) in cases {
                let mut spy = ReaderSpy {
                    result: Some(Ok(rows)),
                    statements: vec![],
                };
                assert_eq!(read(&mut spy, minimum).await.expect("read"), expected);
                assert_eq!(spy.statements.len(), 1);
                let statement = &spy.statements[0];
                let (label, params) = if minimum {
                    (
                        "memory_ann_registry_min",
                        vec![
                            SqlValue::Text("local'quoted".into()),
                            SqlValue::Text("model".into()),
                        ],
                    )
                } else {
                    (
                        "note_search_ann_consumer_snapshot",
                        vec![
                            SqlValue::Text("consumer".into()),
                            SqlValue::Text("local'quoted".into()),
                            SqlValue::Text("model".into()),
                        ],
                    )
                };
                assert_eq!(statement.label.as_deref(), Some(label));
                assert_eq!(
                    serde_json::to_value(&statement.params).unwrap(),
                    serde_json::to_value(params).unwrap()
                );
            }
            let mut spy = ReaderSpy {
                result: Some(Err(StorageError::Internal("registry-reader-marker".into()))),
                statements: vec![],
            };
            match read(&mut spy, minimum).await {
                Err(StorageError::Internal(message)) => {
                    assert_eq!(message, "registry-reader-marker")
                }
                other => panic!("backend error must propagate unchanged: {other:?}"),
            }
            assert_eq!(spy.statements.len(), 1);
        }
    }

    fn memory_backend() -> StorageBackend {
        let backend = StorageBackend::memory().expect("private memory backend");
        backend.prepare_core_schema().expect("core schema");
        backend
    }

    async fn execute(sql: &dyn SqlAccess, text: &str, params: Vec<SqlValue>) {
        sql.writer()
            .await
            .expect("writer")
            .execute(SqlStatement::new(text, params).labelled("registry_reader_fixture"))
            .await
            .expect("seed");
    }

    async fn watermark(
        sql: &dyn SqlAccess,
        consumer: &str,
        namespace: &str,
        model: &str,
        value: i64,
    ) {
        execute(sql, "INSERT INTO ann_consumer_watermark (consumer, namespace, embedding_model, watermark) VALUES (?1, ?2, ?3, ?4)",
            vec![SqlValue::Text(consumer.into()), SqlValue::Text(namespace.into()), SqlValue::Text(model.into()), SqlValue::Integer(value)]).await;
    }

    #[tokio::test]
    async fn sqlite_readers_keep_exact_scope_wildcards_and_closed_watermarks() {
        let backend = memory_backend();
        let sql = backend.sql();
        watermark(sql.as_ref(), "own", "local'quoted", "model", 11).await;
        watermark(sql.as_ref(), "peer", "local'quoted", "model", 8).await;
        watermark(sql.as_ref(), "global", "*", "model", 3).await;
        watermark(sql.as_ref(), "foreign", "other", "model", -2).await;
        watermark(sql.as_ref(), "case", "LOCAL'QUOTED", "model", -1).await;
        watermark(
            sql.as_ref(),
            "wrong-model",
            "local'quoted",
            "other-model",
            -2,
        )
        .await;
        watermark(sql.as_ref(), "pending", "local'quoted", "pending-model", -2).await;
        watermark(sql.as_ref(), "recovering", "*", "recovering-model", -1).await;
        // No writer is acquired while this existing reader is held.
        let mut reader = sql.reader().await.expect("reader");
        assert_eq!(
            read_watermark_on(reader.as_mut(), "own", "own", "local'quoted", "model")
                .await
                .unwrap(),
            Some(11)
        );
        assert_eq!(
            read_watermark_on(reader.as_mut(), "missing", "own", "other", "model")
                .await
                .unwrap(),
            None
        );
        for (namespace, model, expected) in [
            ("local'quoted", "model", Some(3)),
            ("*", "model", Some(3)),
            ("local'quoted", "pending-model", Some(-2)),
            ("local'quoted", "recovering-model", Some(-1)),
            ("local'quoted", "missing-model", None),
        ] {
            assert_eq!(
                min_watermark_on(reader.as_mut(), "fixture_", namespace, model)
                    .await
                    .unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn final_tail_reads_registry_minimum_in_each_parameter_layout() {
        let backend = memory_backend();
        let sql = backend.sql();
        watermark(sql.as_ref(), "owner", "registry", "model", 7).await;
        watermark(sql.as_ref(), "peer", "registry", "model", 5).await;
        watermark(sql.as_ref(), "global", "*", "model", 3).await;
        watermark(sql.as_ref(), "foreign", "other", "model", -2).await;
        watermark(sql.as_ref(), "other-model", "registry", "wrong-model", -2).await;
        execute(sql.as_ref(), "CREATE TABLE fixture_vectors (subject_id TEXT PRIMARY KEY, namespace TEXT, embedding_model TEXT, kind TEXT, field TEXT, embedding BLOB)", vec![]).await;
        for (seq, namespace, model, field) in [
            (2, "registry", "model", "note.content"),
            (4, "registry", "model", "note.content"),
            (5, "corpus", "model", "note.content"),
            (6, "other", "model", "note.content"),
            (7, "registry", "wrong-model", "note.content"),
            (8, "registry", "model", "other.field"),
        ] {
            let id = format!("subject-{seq}");
            execute(sql.as_ref(), "INSERT INTO ann_write_log (seq, namespace, embedding_model, kind, field, subject_id, op) VALUES (?1, ?2, ?3, 'note', ?4, ?5, 'upsert')",
                vec![SqlValue::Integer(seq), SqlValue::Text(namespace.into()), SqlValue::Text(model.into()), SqlValue::Text(field.into()), SqlValue::Text(id.clone())]).await;
            execute(sql.as_ref(), "INSERT INTO fixture_vectors (subject_id, namespace, embedding_model, kind, field, embedding) VALUES (?1, ?2, ?3, 'note', ?4, ?5)",
                vec![SqlValue::Text(id), SqlValue::Text(namespace.into()), SqlValue::Text(model.into()), SqlValue::Text(field.into()), SqlValue::Blob(vec![0, 0, 0, 0])]).await;
        }
        let mut reader = sql.reader().await.expect("reader");
        for (namespace, expected) in [
            (Some("registry"), vec!["subject-4"]),
            (Some("corpus"), vec!["subject-5"]),
            (None, vec!["subject-4", "subject-5", "subject-6"]),
        ] {
            let scope = CorpusScope {
                namespace,
                record_kind: Some("note"),
                field: "note.content",
                live_join: None,
                watermark_capture: WatermarkCapture::LogHighWater,
            };
            let floor = TailFloor::RegistryMinimum {
                registry_namespace: "registry",
                consumer: "owner",
            };
            let rows = reader
                .query_all(scope.final_tail(
                    "fixture_vectors",
                    "model",
                    1,
                    None,
                    floor,
                    "tail_layout",
                ))
                .await
                .unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| row.text_or_none("subject_id").unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            for row in &rows {
                assert_eq!(row.i64_or_none("registry_min"), Some(3));
                assert_eq!(row.i64_or_none("own_watermark"), Some(7));
            }
            let empty = reader
                .query_all(scope.final_tail(
                    "fixture_vectors",
                    "model",
                    100,
                    None,
                    floor,
                    "empty_tail_layout",
                ))
                .await
                .unwrap();
            assert_eq!(empty.len(), 1);
            assert!(matches!(empty[0].get("subject_id"), Some(SqlValue::Null)));
            assert_eq!(empty[0].i64_or_none("registry_min"), Some(3));
            assert_eq!(empty[0].i64_or_none("own_watermark"), Some(7));
        }
    }
}
