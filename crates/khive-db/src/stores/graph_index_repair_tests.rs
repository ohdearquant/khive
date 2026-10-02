use super::*;
use crate::backend::StorageBackend;
use serial_test::serial;
use std::time::Duration;

#[tokio::test]
#[serial(tx_registry)]
async fn external_index_drop_during_walk_stays_typed_without_bfs_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mid-walk-index.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    let graph = backend.graph().unwrap();
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let grandchild = Uuid::new_v4();
    for (source, target) in [(root, child), (child, grandchild)] {
        let now = chrono::Utc::now();
        graph
            .upsert_edge(Edge {
                id: Uuid::new_v4().into(),
                namespace: "local".into(),
                source_id: source,
                target_id: target,
                relation: EdgeRelation::Extends,
                weight: 1.0,
                created_at: now,
                updated_at: now,
                deleted_at: None,
                metadata: None,
                target_backend: None,
            })
            .await
            .unwrap();
    }
    let before = backend.pool().writer_acquisition_snapshot();
    let (reached, proceed) = tests::traverse_snapshot_seam::install(root);
    struct RestoreSnapshotSeam;
    impl Drop for RestoreSnapshotSeam {
        fn drop(&mut self) {
            tests::traverse_snapshot_seam::uninstall();
        }
    }
    let _restore_seam = RestoreSnapshotSeam;
    let budget = TraversalExecutionBudget::new(3, Duration::from_secs(5));
    let observed_budget = budget.clone();
    let worker = tokio::spawn(async move {
        graph
            .traverse(TraversalRequest {
                roots: vec![root],
                options: TraversalOptions::new(2).with_direction(Direction::Out),
                include_roots: false,
                include_properties: false,
                execution_budget: budget,
            })
            .await
    });
    // The seam fires after SQLite has produced the first root adjacency row.
    // DROP commits through a genuinely distinct connection while that cursor
    // retains its old statement snapshot. The next BFS statement sees the drop.
    let drop_path = path.clone();
    let dropper = tokio::task::spawn_blocking(move || {
        let reached_result = reached.recv_timeout(Duration::from_secs(5));
        let drop_result = if reached_result.is_ok() {
            rusqlite::Connection::open(drop_path).and_then(|connection| {
                connection.execute_batch("DROP INDEX idx_graph_edges_ns_src_rel")
            })
        } else {
            Err(rusqlite::Error::InvalidQuery)
        };
        // Release a reached hook even on a failed DROP. If the walk never
        // reaches it, do not block a zero-capacity sender with no receiver.
        if reached_result.is_ok() {
            let _ = proceed.send(());
        }
        reached_result.unwrap();
        drop_result.unwrap();
    });
    dropper.await.unwrap();
    let error = worker.await.unwrap().unwrap_err();
    let StorageError::Driver {
        capability,
        operation,
        source,
    } = error
    else {
        panic!("{error}");
    };
    assert_eq!(capability, StorageCapability::Graph);
    assert_eq!(operation.as_ref(), "traverse");
    let sqlite = source
        .downcast_ref::<rusqlite::Error>()
        .expect("original SQLite error");
    let message = match sqlite {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message,
        rusqlite::Error::SqlInputError { msg, .. } => msg,
        _ => panic!("unexpected cause: {sqlite}"),
    };
    assert_eq!(message, "no such index: idx_graph_edges_ns_src_rel");
    assert_eq!(
        observed_budget.remaining_work(),
        2,
        "the produced first row is consumed exactly once"
    );
    assert_eq!(observed_budget.max_duration(), Duration::from_secs(5));
    assert_eq!(
        backend
            .pool()
            .writer_acquisition_snapshot()
            .pooled_acquisitions,
        before.pooled_acquisitions,
        "mid-walk failure must not re-ensure behind the writer"
    );
    let external = rusqlite::Connection::open(path).unwrap();
    let exists: bool = external
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='idx_graph_edges_ns_src_rel')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!exists);
}
