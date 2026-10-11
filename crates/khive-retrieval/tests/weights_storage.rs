#![cfg(feature = "persist")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use khive_db::StorageBackend;
use khive_retrieval::weights::{
    apply_weight_delta, apply_weight_delta_with_eta, batch_load_weights, init_weight_schema,
    WeightChannel,
};
use khive_retrieval::PersistError;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess, StorageError, WriterTaskRequestState};
use tokio::sync::Barrier;
use uuid::Uuid;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("khive-weights-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create private weight fixture");
        Self(path)
    }

    fn queued_backend(&self) -> StorageBackend {
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            self.0.join("weights.db"),
            true,
            Duration::from_secs(10),
            self.0.join("volume-locks"),
        )
        .expect("open file-backed backend");
        backend.prepare_core_schema().expect("migrate core tables");
        assert!(backend.pool().write_queue_active());
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("start writer task")
            .is_some());
        backend
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn snapshot(sql: &dyn SqlAccess) -> serde_json::Value {
    let mut reader = sql.reader().await.expect("read fixture state");
    let weights = reader
        .query_all(SqlStatement::new(
            "SELECT * FROM atom_weights ORDER BY namespace, atom_id",
            vec![],
        ))
        .await
        .expect("read migrated weight table");
    let events = reader
        .query_all(SqlStatement::new(
            "SELECT rowid, * FROM weight_events ORDER BY rowid",
            vec![],
        ))
        .await
        .expect("read migrated audit table");
    serde_json::json!({ "weights": weights, "events": events })
}

#[tokio::test]
async fn migrated_weight_schema_matches_public_legacy_initializer() {
    const SCHEMA: &str = "SELECT type, name, sql FROM sqlite_schema \
                         WHERE tbl_name IN ('atom_weights', 'weight_events') ORDER BY type, name";
    let legacy: Vec<(String, String, Option<String>)> = {
        let conn = rusqlite::Connection::open_in_memory().expect("legacy fixture");
        init_weight_schema(&conn).expect("public legacy schema initializer");
        let mut statement = conn.prepare(SCHEMA).expect("legacy schema query");
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("legacy schema rows")
            .collect::<rusqlite::Result<_>>()
            .expect("decode legacy schema");
        rows
    };
    assert_eq!(legacy.iter().filter(|row| row.0 == "table").count(), 2);

    let scratch = ScratchDir::new();
    let backend = scratch.queued_backend();
    let mut reader = backend.reader().await.expect("read migrated schema");
    let migrated: Vec<_> = reader
        .query_all(SqlStatement::new(SCHEMA, vec![]))
        .await
        .expect("migrated schema rows")
        .into_iter()
        .map(|row| {
            (
                row.text("type").unwrap().to_owned(),
                row.text("name").unwrap().to_owned(),
                row.opt_text("sql").unwrap().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(migrated, legacy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_weight_updates_serialize_versions_and_matching_audit_receipts() {
    const UPDATES: usize = 16;
    const NAMESPACE: &str = "weights:concurrent";
    let scratch = ScratchDir::new();
    let backend = scratch.queued_backend();
    let sql = backend.sql();
    let atom = Uuid::new_v4();
    let missing = Uuid::new_v4();
    let before = backend.pool().writer_acquisition_snapshot();
    let barrier = Arc::new(Barrier::new(UPDATES + 1));
    let mut tasks = Vec::with_capacity(UPDATES);
    for index in 0..UPDATES {
        let sql = Arc::clone(&sql);
        let barrier = Arc::clone(&barrier);
        tasks.push(tokio::spawn(async move {
            let event_id = Uuid::new_v4();
            let context = format!("concurrent-{index}");
            barrier.wait().await;
            // Exactly representable increments below the ceiling expose lost
            // updates without relying on a floating-point tolerance or task order.
            let (weight, row_id) = apply_weight_delta_with_eta(
                sql.as_ref(),
                NAMESPACE,
                atom,
                0.125,
                WeightChannel::Explicit,
                0.0,
                Some(event_id),
                Some(&context),
            )
            .await
            .expect("queued update");
            (event_id.to_string(), context, weight, row_id)
        }));
    }
    barrier.wait().await;
    let mut receipts = Vec::with_capacity(UPDATES);
    for task in tasks {
        receipts.push(task.await.expect("weight worker"));
    }

    let after = backend.pool().writer_acquisition_snapshot();
    assert_eq!(
        after.writer_task_acquisitions - before.writer_task_acquisitions,
        UPDATES as u64,
        "one queued atomic unit per update"
    );
    assert_eq!(after.pooled_acquisitions, before.pooled_acquisitions);
    assert_eq!(
        after.standalone_acquisitions,
        before.standalone_acquisitions
    );

    let loaded = batch_load_weights(sql.as_ref(), NAMESPACE, &[atom, missing])
        .await
        .expect("load committed weights");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.get(&atom), Some(&3.0));
    assert!(!loaded.contains_key(&missing));

    let mut reader = sql.reader().await.expect("inspect committed rows");
    let weights = reader
        .query_all(SqlStatement::new(
            "SELECT weight, version, updated_at FROM atom_weights \
             WHERE namespace = ?1 AND atom_id = ?2",
            vec![
                SqlValue::Text(NAMESPACE.into()),
                SqlValue::Text(atom.to_string()),
            ],
        ))
        .await
        .expect("weight and version");
    assert_eq!(weights.len(), 1);
    assert_eq!(weights[0].i64("version").unwrap(), UPDATES as i64);
    assert_eq!(weights[0].f64("weight").unwrap(), 3.0);
    let events = reader
        .query_all(SqlStatement::new(
            "SELECT rowid AS audit_id, namespace, atom_id, delta, weight_after, \
             channel, eta, event_id, context_id, ts FROM weight_events ORDER BY rowid",
            vec![],
        ))
        .await
        .expect("audit rows");
    assert_eq!(events.len(), UPDATES);
    for (index, row) in events.iter().enumerate() {
        assert_eq!(row.text("namespace").unwrap(), NAMESPACE);
        assert_eq!(row.uuid("atom_id").unwrap(), atom);
        assert_eq!(row.f64("delta").unwrap(), 0.125);
        assert_eq!(row.f64("eta").unwrap(), 0.0);
        assert_eq!(row.text("channel").unwrap(), "explicit");
        assert_eq!(
            row.f64("weight_after").unwrap(),
            1.0 + (index + 1) as f64 / 8.0
        );
    }
    let mut returned_ids: Vec<_> = receipts.iter().map(|receipt| receipt.3).collect();
    returned_ids.sort_unstable();
    returned_ids.dedup();
    assert_eq!(
        returned_ids.len(),
        UPDATES,
        "each receipt owns one audit row"
    );
    for (event_id, context, weight, row_id) in receipts {
        let row = events
            .iter()
            .find(|row| row.i64("audit_id").unwrap() == row_id)
            .expect("returned row id identifies its committed audit");
        assert_eq!(row.text("event_id").unwrap(), event_id);
        assert_eq!(row.text("context_id").unwrap(), context);
        assert_eq!(row.f64("weight_after").unwrap(), f64::from(weight));
    }
    assert_eq!(
        weights[0].i64("updated_at").unwrap(),
        events.last().unwrap().i64("ts").unwrap(),
        "current row and its final audit share one update timestamp"
    );
}

#[tokio::test]
async fn read_only_backend_loads_weights_and_refuses_both_write_entry_points() {
    let scratch = ScratchDir::new();
    let writable = scratch.queued_backend();
    let atom = Uuid::new_v4();
    let missing = Uuid::new_v4();
    let (weight, _) = apply_weight_delta(
        &writable,
        "weights:read-only",
        atom,
        0.5,
        WeightChannel::Explicit,
        None,
        Some("seed"),
    )
    .await
    .expect("seed writable database");
    let before = snapshot(&writable).await;
    let read_only = StorageBackend::sqlite_read_only_for_test(scratch.0.join("weights.db"))
        .expect("open read-only backend");
    read_only
        .prepare_core_schema()
        .expect("validate migrated schema");
    let before_writers = read_only.pool().writer_acquisition_snapshot();
    let loaded = batch_load_weights(&read_only, "weights:read-only", &[atom, missing])
        .await
        .expect("read-only batch load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded.get(&atom), Some(&weight));
    assert!(!loaded.contains_key(&missing));

    let ordinary = apply_weight_delta(
        &read_only,
        "weights:read-only",
        atom,
        0.25,
        WeightChannel::Ambient,
        None,
        None,
    )
    .await;
    let overridden = apply_weight_delta_with_eta(
        &read_only,
        "weights:read-only",
        missing,
        0.25,
        WeightChannel::GroundTruth,
        0.0,
        None,
        None,
    )
    .await;
    for result in [ordinary, overridden] {
        match result.expect_err("read-only write must fail") {
            PersistError::Storage(StorageError::Pool { operation, message }) => {
                assert_eq!(operation, "atomic_unit");
                assert_eq!(message, "backend is read-only");
            }
            error => panic!("expected typed read-only storage refusal, got {error:?}"),
        }
    }
    assert_eq!(
        read_only.pool().writer_acquisition_snapshot(),
        before_writers
    );
    assert_eq!(snapshot(&read_only).await, before);
    assert_eq!(snapshot(&writable).await, before);
}

#[tokio::test]
async fn rejected_audit_insert_rolls_back_weight_insert_and_update() {
    let scratch = ScratchDir::new();
    let backend = scratch.queued_backend();
    let existing = Uuid::new_v4();
    let absent = Uuid::new_v4();
    apply_weight_delta(
        &backend,
        "weights:rollback",
        existing,
        0.25,
        WeightChannel::Explicit,
        None,
        Some("surviving-seed"),
    )
    .await
    .expect("seed current weight and audit");
    let before = snapshot(&backend).await;
    {
        let mut writer = backend.writer().await.expect("install audit failure");
        writer
            .execute_script(
                "CREATE TRIGGER reject_weight_audit BEFORE INSERT ON weight_events \
                 BEGIN SELECT RAISE(ABORT, 'injected weight audit failure'); END;"
                    .into(),
            )
            .await
            .expect("audit rejection trigger");
    }
    for atom in [existing, absent] {
        let error = apply_weight_delta_with_eta(
            &backend,
            "weights:rollback",
            atom,
            0.5,
            WeightChannel::GroundTruth,
            0.0,
            Some(Uuid::new_v4()),
            Some("must-roll-back"),
        )
        .await
        .expect_err("audit failure must fail the complete update");
        match error {
            PersistError::Storage(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source,
            }) => assert!(
                source.to_string().contains("injected weight audit failure"),
                "retain the actual audit failure: {source:?}"
            ),
            error => panic!("expected proven rollback of the queued update, got {error:?}"),
        }
        assert_eq!(snapshot(&backend).await, before);
    }
    {
        let mut writer = backend.writer().await.expect("remove audit failure");
        writer
            .execute_script("DROP TRIGGER reject_weight_audit".into())
            .await
            .expect("remove rejection trigger");
    }
    apply_weight_delta(
        &backend,
        "weights:rollback",
        absent,
        0.25,
        WeightChannel::Explicit,
        None,
        None,
    )
    .await
    .expect("the writer remains usable after confirmed rollback");
    let mut reader = backend.reader().await.expect("inspect recovery");
    assert_eq!(
        reader
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM atom_weights",
                vec![]
            ))
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        reader
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM weight_events",
                vec![]
            ))
            .await
            .unwrap(),
        2
    );
}
