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

const REPAIR_WATCHDOG: Duration = Duration::from_secs(1);
const REPAIR_SETTLE: Duration = Duration::from_millis(100);

struct HeldRepairFixture {
    _dir: tempfile::TempDir,
    backend: StorageBackend,
    graph: std::sync::Arc<dyn khive_storage::GraphStore>,
    external: rusqlite::Connection,
    root: Uuid,
    child: Uuid,
    direction: Direction,
    index: &'static str,
}

impl HeldRepairFixture {
    fn index_present(&self) -> bool {
        self.external
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?1)",
                [self.index],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn request(&self, budget: TraversalExecutionBudget) -> TraversalRequest {
        let mut options = TraversalOptions::new(1).with_direction(self.direction.clone());
        options.limit = Some(1);
        TraversalRequest {
            roots: vec![self.root],
            options,
            include_roots: false,
            include_properties: false,
            execution_budget: budget,
        }
    }
}

async fn held_repair_fixture(direction: Direction) -> HeldRepairFixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("held-traversal-repair.db");
    // Own lock namespace: the 100 ms traversal budgets below start before the
    // held writer is checked out, so its lease must not queue behind other
    // tests' writers on this volume.
    let backend = StorageBackend::sqlite_with_pool_config(
        &path,
        crate::pool::PoolConfig {
            volume_lock_dir: Some(dir.path().join("volume-locks")),
            ..crate::pool::PoolConfig::for_test()
        },
        None,
    )
    .unwrap();
    assert!(backend.pool().config().wal_mode);
    assert!(backend.pool().max_readers() > 0);
    assert!(
        backend.pool().config().checkout_timeout > REPAIR_WATCHDOG,
        "fixture checkout timeout must remain independent of the short traversal budget"
    );
    let graph = backend.graph().unwrap();
    let root = Uuid::new_v4();
    let child = Uuid::new_v4();
    let (source_id, target_id, index) = match direction {
        Direction::Out => (root, child, "idx_graph_edges_ns_src_rel"),
        Direction::In => (child, root, "idx_graph_edges_ns_tgt_rel"),
        Direction::Both => unreachable!("each forced adjacency index has its own fixture"),
    };
    let now = chrono::Utc::now();
    graph
        .upsert_edge(Edge {
            id: Uuid::new_v4().into(),
            namespace: "local".into(),
            source_id,
            target_id,
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
    let fixture = HeldRepairFixture {
        _dir: dir,
        backend,
        graph,
        external: rusqlite::Connection::open(&path).unwrap(),
        root,
        child,
        direction,
        index,
    };
    let paths = fixture
        .graph
        .traverse(fixture.request(TraversalExecutionBudget::default()))
        .await
        .unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].nodes.len(), 1);
    assert_eq!(paths[0].nodes[0].node_id, child);
    assert!(
        fixture.index_present(),
        "fixture must begin with the real index"
    );
    fixture
        .external
        .execute_batch(&format!("DROP INDEX {}", fixture.index))
        .unwrap();
    assert!(!fixture.index_present(), "external index drop must commit");
    fixture
}

struct HeldRepairObservation {
    result: Result<Result<Vec<GraphPath>, StorageError>, tokio::time::error::Elapsed>,
    elapsed: Duration,
    watchdog: Duration,
    writer_before: crate::pool::WriterAcquisitionSnapshot,
    writer_after: crate::pool::WriterAcquisitionSnapshot,
    reader_checkouts: u64,
}

async fn observe_held_repair<F>(
    fixture: &HeldRepairFixture,
    budget: &TraversalExecutionBudget,
    future: F,
) -> HeldRepairObservation
where
    F: std::future::Future<Output = Result<Vec<GraphPath>, StorageError>>,
{
    observe_held_repair_until(fixture, budget, REPAIR_WATCHDOG, future).await
}

async fn observe_held_repair_until<F>(
    fixture: &HeldRepairFixture,
    budget: &TraversalExecutionBudget,
    watchdog: Duration,
    future: F,
) -> HeldRepairObservation
where
    F: std::future::Future<Output = Result<Vec<GraphPath>, StorageError>>,
{
    let held = fixture.backend.pool().try_writer().unwrap();
    assert!(
        !budget.is_expired(),
        "budget must be live at the held-writer boundary"
    );
    let writer_before = fixture.backend.pool().writer_acquisition_snapshot();
    let reader_before = fixture
        .backend
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts;
    let started = std::time::Instant::now();
    // This watchdog is a test failure, never a replacement StorageError.
    let result = tokio::time::timeout(watchdog, future).await;
    let elapsed = started.elapsed();
    let writer_after = fixture.backend.pool().writer_acquisition_snapshot();
    let reader_checkouts = fixture
        .backend
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts
        - reader_before;
    // Release even after a watchdog failure, before any result assertion.
    drop(held);
    HeldRepairObservation {
        result,
        elapsed,
        watchdog,
        writer_before,
        writer_after,
        reader_checkouts,
    }
}

fn assert_held_repair_timeout(observed: &HeldRepairObservation, operation: &str) {
    match &observed.result {
        Ok(Err(StorageError::Timeout { operation: actual })) => {
            assert_eq!(actual.as_ref(), operation);
        }
        other => panic!("expected typed timeout while writer remained held, got {other:?}"),
    }
    assert!(observed.elapsed < observed.watchdog);
    assert!(
        observed.reader_checkouts > 0,
        "preflight must reach the real reader route"
    );
    assert_eq!(observed.writer_after, observed.writer_before);
}

async fn assert_pending_repair_stopped_then_retry(
    fixture: &HeldRepairFixture,
    observed: &HeldRepairObservation,
) {
    // Give the cancelled blocking admission loop a bounded chance to settle
    // after the held writer is released. The following independent traversal
    // must still encounter and repair the absent index itself.
    tokio::time::sleep(REPAIR_SETTLE).await;
    assert!(
        !fixture.index_present(),
        "cancelled pending repair must not run DDL"
    );
    assert_eq!(
        fixture.backend.pool().writer_acquisition_snapshot(),
        observed.writer_before,
        "pending repair must not acquire the released writer"
    );
    let budget = TraversalExecutionBudget::new(1, Duration::from_secs(5));
    let usage = khive_storage::usage::UsageContext::new();
    let paths = khive_storage::usage::scope(
        usage.clone(),
        fixture.graph.traverse(fixture.request(budget.clone())),
    )
    .await
    .unwrap();
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0].nodes.len(), 1);
    assert_eq!(paths[0].nodes[0].node_id, fixture.child);
    assert_eq!(budget.remaining_work(), 0);
    assert_eq!(usage.snapshot()["graph_hops"], 1);
    assert_eq!(usage.snapshot()["db_round_trips"], 1);
    assert!(fixture.index_present());
    assert_eq!(
        fixture
            .backend
            .pool()
            .writer_acquisition_snapshot()
            .pooled_acquisitions,
        observed.writer_before.pooled_acquisitions + 1,
        "only the independent retry acquires a repair writer"
    );
}

#[tokio::test]
async fn missing_index_writer_wait_obeys_original_traversal_budget() {
    for direction in [Direction::Out, Direction::In] {
        let fixture = held_repair_fixture(direction).await;
        let budget = TraversalExecutionBudget::new(1, Duration::from_millis(100));
        let usage = khive_storage::usage::UsageContext::new();
        let observed = observe_held_repair(
            &fixture,
            &budget,
            khive_storage::usage::scope(
                usage.clone(),
                fixture.graph.traverse(fixture.request(budget.clone())),
            ),
        )
        .await;
        assert_held_repair_timeout(&observed, "traverse (100ms execution budget)");
        assert!(budget.is_expired());
        assert_eq!(
            budget.remaining_work(),
            1,
            "preflight must spend no adjacency work"
        );
        assert_eq!(
            usage
                .snapshot()
                .get("graph_hops")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            0
        );
        assert_eq!(
            usage
                .snapshot()
                .get("db_round_trips")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            0
        );
        assert_pending_repair_stopped_then_retry(&fixture, &observed).await;
    }
}

#[tokio::test]
async fn missing_index_writer_wait_keeps_earlier_request_deadline() {
    let fixture = held_repair_fixture(Direction::Out).await;
    let budget = TraversalExecutionBudget::new(1, Duration::from_secs(5));
    let observed = observe_held_repair(
        &fixture,
        &budget,
        khive_storage::scope_request_read_deadline(
            Duration::from_millis(30),
            fixture.graph.traverse(fixture.request(budget.clone())),
        ),
    )
    .await;
    assert_held_repair_timeout(&observed, "traverse");
    assert!(
        !budget.is_expired(),
        "the earlier request deadline must be the cause"
    );
    assert_eq!(budget.remaining_work(), 1);
    assert_pending_repair_stopped_then_retry(&fixture, &observed).await;
}

#[tokio::test]
async fn missing_index_writer_wait_keeps_caller_cancellation() {
    let fixture = held_repair_fixture(Direction::Out).await;
    let budget = TraversalExecutionBudget::new(1, Duration::from_secs(5));
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        sender.send(true).unwrap();
    });
    let observed = observe_held_repair(
        &fixture,
        &budget,
        khive_storage::scope_request_read_cancellation(
            receiver,
            fixture.graph.traverse(fixture.request(budget.clone())),
        ),
    )
    .await;
    cancel.await.unwrap();
    assert_held_repair_timeout(&observed, "traverse");
    assert!(
        !budget.is_expired(),
        "caller cancellation must remain the cause"
    );
    assert_eq!(budget.remaining_work(), 1);
    assert_pending_repair_stopped_then_retry(&fixture, &observed).await;
}

#[tokio::test]
async fn missing_index_writer_wait_does_not_renew_an_aged_traversal_budget() {
    let fixture = held_repair_fixture(Direction::Out).await;
    let budget = TraversalExecutionBudget::new(1, Duration::from_millis(500));
    tokio::time::sleep(Duration::from_millis(350)).await;
    let observed = observe_held_repair_until(
        &fixture,
        &budget,
        Duration::from_millis(300),
        fixture.graph.traverse(fixture.request(budget.clone())),
    )
    .await;
    assert_held_repair_timeout(&observed, "traverse (500ms execution budget)");
    assert!(budget.is_expired());
    assert_eq!(budget.remaining_work(), 1);
    assert_pending_repair_stopped_then_retry(&fixture, &observed).await;
}

struct ReleaseRepairAuthorizer {
    pool: std::sync::Arc<crate::pool::ConnectionPool>,
    proceed: Option<std::sync::mpsc::SyncSender<()>>,
}

impl ReleaseRepairAuthorizer {
    fn finish(&mut self) -> Result<(), crate::SqliteError> {
        if let Some(proceed) = self.proceed.take() {
            let _ = proceed.try_send(());
        }
        // The callback holds this exact writer. Reacquiring it after release
        // joins its admitted DDL ownership before removing the authorizer.
        let writer = self.pool.try_writer()?;
        writer.conn().authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        )?;
        Ok(())
    }
}

impl Drop for ReleaseRepairAuthorizer {
    fn drop(&mut self) {
        // Also release a reached callback when a test assertion unwinds.
        if self.proceed.is_some() {
            let _ = self.finish();
        }
    }
}

#[tokio::test]
async fn admitted_index_repair_keeps_ddl_ownership_after_traversal_timeout() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicBool, Ordering};

    let fixture = held_repair_fixture(Direction::Out).await;
    let (reached_tx, reached_rx) = std::sync::mpsc::sync_channel(1);
    let (proceed_tx, proceed_rx) = std::sync::mpsc::sync_channel(1);
    let fired = std::sync::Arc::new(AtomicBool::new(false));
    let proceeded = std::sync::Arc::new(AtomicBool::new(false));
    let callback_fired = std::sync::Arc::clone(&fired);
    let callback_proceeded = std::sync::Arc::clone(&proceeded);
    let index = fixture.index;
    let mut cleanup = ReleaseRepairAuthorizer {
        pool: fixture.backend.pool_arc(),
        proceed: Some(proceed_tx),
    };
    {
        let writer = fixture.backend.pool().try_writer().unwrap();
        writer
            .conn()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::CreateIndex { index_name, .. } if index_name == index)
                    && !callback_fired.swap(true, Ordering::Relaxed)
                {
                    let _ = reached_tx.send(());
                    callback_proceeded.store(
                        proceed_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
                        Ordering::Relaxed,
                    );
                }
                Authorization::Allow
            }))
            .unwrap();
    }
    let budget = TraversalExecutionBudget::new(1, Duration::from_millis(100));
    let request = fixture.request(budget.clone());
    let graph = std::sync::Arc::clone(&fixture.graph);
    let usage = khive_storage::usage::UsageContext::new();
    let worker_usage = usage.clone();
    let mut worker = tokio::spawn(async move {
        khive_storage::usage::scope(worker_usage, graph.traverse(request)).await
    });
    let reached =
        tokio::task::spawn_blocking(move || reached_rx.recv_timeout(REPAIR_WATCHDOG)).await;
    let returned_while_blocked = tokio::time::timeout(REPAIR_WATCHDOG, &mut worker).await;
    let was_still_blocked = !proceeded.load(Ordering::Relaxed);
    // Release and clear before asserting either watchdog or callback result.
    let cleared = cleanup.finish();
    if returned_while_blocked.is_err() {
        let _ = worker.await;
    }
    cleared.unwrap();
    reached
        .unwrap()
        .expect("actual CREATE INDEX callback must be reached");
    assert!(fired.load(Ordering::Relaxed));
    assert!(
        was_still_blocked,
        "DDL must still be held when the caller returns"
    );
    let result = returned_while_blocked
        .expect("caller must return while admitted DDL is still blocked")
        .unwrap();
    assert!(
        matches!(&result, Err(StorageError::Timeout { operation }) if operation.as_ref() == "traverse (100ms execution budget)"),
        "{result:?}"
    );
    assert!(proceeded.load(Ordering::Relaxed));
    assert_eq!(budget.remaining_work(), 1);
    assert_eq!(
        usage
            .snapshot()
            .get("graph_hops")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        0
    );
    assert_eq!(
        usage
            .snapshot()
            .get("db_round_trips")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        0
    );
    assert!(
        fixture.index_present(),
        "admitted repair DDL may complete independently"
    );
    let paths = fixture
        .graph
        .traverse(fixture.request(TraversalExecutionBudget::default()))
        .await
        .unwrap();
    assert_eq!(paths[0].nodes[0].node_id, fixture.child);
}
