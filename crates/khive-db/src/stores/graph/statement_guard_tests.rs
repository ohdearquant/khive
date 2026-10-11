//! Real-backend controls for ordered SELECT guards and create-only composition.

use super::*;
use crate::pool::PoolConfig;
use crate::StorageBackend;
use khive_storage::{
    EdgeInsertIfAbsentOutcome, GraphGuardValueKind, GraphStatementGuard,
    GraphStatementGuardFailure, GraphStatementGuardFailureReason,
};
use khive_types::{EventKind, SubstrateKind};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

struct GuardBarrier {
    id: LinkId,
    reached: Sender<()>,
    proceed: Receiver<()>,
}

static AFTER_GUARDS: Mutex<Option<GuardBarrier>> = Mutex::new(None);

// Single-use and edge-keyed: unrelated graph writes never wait on this seam.
// Disconnecting the sender on a failed assertion also releases the writer.
pub(super) fn after_guards(edge: &Edge) {
    let barrier = {
        let mut slot = AFTER_GUARDS.lock().unwrap();
        if slot.as_ref().is_some_and(|barrier| barrier.id == edge.id) {
            slot.take()
        } else {
            None
        }
    };
    if let Some(barrier) = barrier {
        let _ = barrier.reached.send(());
        barrier
            .proceed
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
    }
}

fn prepared(backend: StorageBackend) -> StorageBackend {
    backend.prepare_core_schema().unwrap();
    backend.graph().unwrap();
    backend.events().unwrap();
    backend
}

fn file_backend(path: &Path, queued: bool) -> StorageBackend {
    prepared(
        StorageBackend::sqlite_with_pool_config(
            path,
            PoolConfig {
                write_queue_enabled: Some(queued),
                write_routing_strict: false,
                disk_guard_config: Some(crate::disk_guard_config::EffectiveDiskGuardConfig {
                    reserve_bytes: 0,
                    ..Default::default()
                }),
                volume_lock_dir: Some(path.with_extension("locks")),
                ..PoolConfig::for_test()
            },
            None,
        )
        .unwrap(),
    )
}

fn edge() -> Edge {
    let now = Utc
        .timestamp_micros(Utc::now().timestamp_micros())
        .single()
        .unwrap();
    Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id: Uuid::new_v4(),
        target_id: Uuid::new_v4(),
        relation: EdgeRelation::Extends,
        weight: 0.375,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: Some(serde_json::json!({"revision": 1})),
        target_backend: Some("fixture-backend".into()),
    }
}

fn guard(label: &str, sql: &str) -> GraphStatementGuard {
    GraphStatementGuard {
        label: label.into(),
        statement: SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("different-diagnostic-label".into()),
        },
    }
}

fn conditions(guards: Vec<GraphStatementGuard>) -> GraphMutationPreconditions {
    GraphMutationPreconditions {
        statement_guards: guards,
        ..Default::default()
    }
}

fn snapshot(backend: &StorageBackend) -> (Vec<String>, Vec<String>) {
    let reader = backend.pool().reader().unwrap();
    let rows = |sql| {
        reader
            .conn()
            .prepare(sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    (
        rows("SELECT json_array(namespace,id,source_id,target_id,relation,weight,created_at,updated_at,deleted_at,metadata,target_backend) FROM graph_edges ORDER BY namespace,id"),
        rows("SELECT json_array(id,namespace,verb,substrate,actor,kind,outcome,payload,target_id,created_at) FROM events ORDER BY id"),
    )
}

fn events(outcome: &GraphMutationEventOutcome) -> StorageResult<Vec<Event>> {
    let GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::Created(edge)) =
        &outcome.mutation
    else {
        panic!("event callback is only allowed for a created edge");
    };
    Ok(vec![Event::new(
        "local",
        "link",
        EventKind::LinkCreated,
        SubstrateKind::Edge,
        "fixture",
    )
    .with_payload(
        serde_json::json!({"source": edge.source_id, "target": edge.target_id}),
    )])
}

async fn insert(backend: &StorageBackend, edge: Edge) -> GraphMutationEventOutcome {
    compose_graph_mutation_events(
        backend,
        GraphMutationRequest::InsertIfAbsent { edge },
        conditions(vec![guard("ready", "SELECT 1")]),
        vec![],
        events,
    )
    .await
    .unwrap()
}

fn assert_created(outcome: GraphMutationEventOutcome) -> Edge {
    let GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::Created(edge)) =
        outcome.mutation
    else {
        panic!("expected Created");
    };
    edge
}

fn assert_failure(
    outcome: GraphMutationEventOutcome,
    label: &str,
    reason: GraphStatementGuardFailureReason,
) {
    assert!(outcome.retired.is_empty());
    let GraphMutationOutcome::GuardFailed(failure) = outcome.mutation else {
        panic!("expected a top-level guard refusal");
    };
    assert_eq!(
        failure,
        GraphStatementGuardFailure {
            label: label.into(),
            reason
        }
    );
}

#[tokio::test]
async fn strict_shape_refusals_keep_the_caller_label_and_never_construct_events() {
    use GraphGuardValueKind as Kind;
    use GraphStatementGuardFailureReason as Reason;
    let backend = prepared(StorageBackend::memory().unwrap());
    let before = snapshot(&backend);
    for (sql, reason) in [
        ("SELECT 0", Reason::PredicateFalse),
        ("SELECT 1 WHERE 0", Reason::NoRows),
        ("SELECT 1 UNION ALL SELECT 1", Reason::MultipleRows),
        ("SELECT 0 UNION ALL SELECT 1", Reason::MultipleRows),
        ("SELECT NULL UNION ALL SELECT 1", Reason::MultipleRows),
        ("SELECT 0, 1", Reason::ColumnCount { actual: 2 }),
        ("SELECT 1, 1 WHERE 0", Reason::ColumnCount { actual: 2 }),
        (
            "SELECT 2",
            Reason::NonBoolean {
                kind: Kind::Integer,
            },
        ),
        (
            "SELECT -1",
            Reason::NonBoolean {
                kind: Kind::Integer,
            },
        ),
        ("SELECT 1.0", Reason::NonBoolean { kind: Kind::Real }),
        ("SELECT '1'", Reason::NonBoolean { kind: Kind::Text }),
        ("SELECT x'01'", Reason::NonBoolean { kind: Kind::Blob }),
        ("SELECT NULL", Reason::NonBoolean { kind: Kind::Null }),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let outcome = compose_graph_mutation_events(
            &backend,
            GraphMutationRequest::InsertIfAbsent { edge: edge() },
            conditions(vec![guard("caller-predicate", sql)]),
            vec![],
            move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            },
        )
        .await
        .unwrap();
        assert_failure(outcome, "caller-predicate", reason);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{sql}");
        assert_eq!(snapshot(&backend), before, "{sql}");
    }
}

#[tokio::test]
async fn guards_are_ordered_and_apply_to_single_and_empty_batch_requests() {
    let backend = prepared(StorageBackend::memory().unwrap());
    let before = snapshot(&backend);
    for mutation in [
        GraphMutationRequest::InsertIfAbsent { edge: edge() },
        GraphMutationRequest::Single {
            request: EdgeUpsertRequest {
                edge: edge(),
                resurrect: false,
            },
            guard_endpoints: false,
        },
        GraphMutationRequest::Batch {
            requests: vec![],
            guard_endpoints: false,
        },
    ] {
        let outcome = compose_graph_mutation_events(
            &backend,
            mutation,
            conditions(vec![
                guard("first-pass", "SELECT 1"),
                guard("first-refusal", "SELECT 0"),
                guard(
                    "must-not-prepare",
                    "SELECT missing FROM nonexistent_guard_table",
                ),
            ]),
            vec![],
            |_| panic!("refused mutation must not construct events"),
        )
        .await
        .unwrap();
        assert_failure(
            outcome,
            "first-refusal",
            GraphStatementGuardFailureReason::PredicateFalse,
        );
        assert_eq!(snapshot(&backend), before);
    }
}

#[tokio::test]
async fn select_cte_and_bound_boolean_pass_but_other_statement_classes_do_not() {
    let backend = prepared(StorageBackend::memory().unwrap());
    let mut bound = guard("bound", "SELECT ?1");
    bound.statement.params = vec![SqlValue::Bool(true)];
    let created = compose_graph_mutation_events(
        &backend,
        GraphMutationRequest::InsertIfAbsent { edge: edge() },
        conditions(vec![
            guard(
                "cte",
                "WITH check_value AS (SELECT 1 AS ok) SELECT ok FROM check_value",
            ),
            bound,
        ]),
        vec![],
        events,
    )
    .await
    .unwrap();
    assert_created(created);
    let before = snapshot(&backend);
    assert_eq!((before.0.len(), before.1.len()), (1, 1));
    for sql in [
        "PRAGMA query_only",
        "VALUES (1)",
        "EXPLAIN SELECT 1",
        "SELECT 1; SELECT 1",
        "DELETE FROM graph_edges RETURNING 1",
        "WITH check_value AS (SELECT 1) DELETE FROM graph_edges RETURNING 1",
        "SELECT missing FROM nonexistent_guard_table",
        "SELECT json_extract('{', '$.missing')",
    ] {
        let result = compose_graph_mutation_events(
            &backend,
            GraphMutationRequest::InsertIfAbsent { edge: edge() },
            conditions(vec![guard("storage-error", sql)]),
            vec![],
            |_| panic!("rejected statement must not construct events"),
        )
        .await;
        assert!(result.is_err(), "expected statement/driver error for {sql}");
        assert_eq!(snapshot(&backend), before, "{sql}");
    }
    let mut invalid_binding = guard("bind-error", "SELECT 1");
    invalid_binding.statement.params.push(SqlValue::Integer(1));
    assert!(compose_graph_mutation_events(
        &backend,
        GraphMutationRequest::InsertIfAbsent { edge: edge() },
        conditions(vec![invalid_binding]),
        vec![],
        |_| panic!("bind failure must not construct events"),
    )
    .await
    .is_err());
    let mut bound_false = guard("bound-false", "SELECT ?1");
    bound_false.statement.params.push(SqlValue::Bool(false));
    let outcome = compose_graph_mutation_events(
        &backend,
        GraphMutationRequest::InsertIfAbsent { edge: edge() },
        conditions(vec![bound_false]),
        vec![],
        |_| panic!("bound false must not construct events"),
    )
    .await
    .unwrap();
    assert_failure(
        outcome,
        "bound-false",
        GraphStatementGuardFailureReason::PredicateFalse,
    );
    assert_eq!(snapshot(&backend), before);
}

#[tokio::test]
async fn natural_key_winners_are_unchanged_and_take_precedence_over_id_collisions() {
    let backend = prepared(StorageBackend::memory().unwrap());
    let original = assert_created(insert(&backend, edge()).await);
    let unrelated = assert_created(insert(&backend, edge()).await);
    for deleted in [false, true] {
        if deleted {
            backend
                .pool()
                .try_writer()
                .unwrap()
                .conn()
                .execute(
                    "UPDATE graph_edges SET deleted_at=7 WHERE namespace=?1 AND id=?2",
                    rusqlite::params![original.namespace, original.id.to_string()],
                )
                .unwrap();
        }
        let before = snapshot(&backend);
        for colliding_id in [false, true] {
            let mut attempted = original.clone();
            attempted.id = if colliding_id {
                unrelated.id
            } else {
                Uuid::new_v4().into()
            };
            attempted.weight = 0.875;
            attempted.metadata = Some(serde_json::json!({"must_not_replace": true}));
            attempted.target_backend = None;
            let outcome = compose_graph_mutation_events(
                &backend,
                GraphMutationRequest::InsertIfAbsent {
                    edge: attempted.clone(),
                },
                conditions(vec![guard("ready", "SELECT 1")]),
                vec![],
                |_| panic!("existing/tombstoned winners must not emit events"),
            )
            .await
            .unwrap();
            assert!(
                matches!(&outcome.mutation,
                GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::Tombstoned) if deleted)
                    || matches!(&outcome.mutation,
                GraphMutationOutcome::InsertIfAbsent(EdgeInsertIfAbsentOutcome::ExistingLive) if !deleted)
            );
            let refused = compose_graph_mutation_events(
                &backend,
                GraphMutationRequest::InsertIfAbsent { edge: attempted },
                conditions(vec![guard("caller-first", "SELECT 0")]),
                vec![],
                |_| panic!("guard failure must precede a winner probe"),
            )
            .await
            .unwrap();
            assert_failure(
                refused,
                "caller-first",
                GraphStatementGuardFailureReason::PredicateFalse,
            );
            assert_eq!(snapshot(&backend), before);
        }
        let mut id_only = edge();
        id_only.id = original.id;
        let error = compose_graph_mutation_events(
            &backend,
            GraphMutationRequest::InsertIfAbsent { edge: id_only },
            Default::default(),
            vec![],
            |_| panic!("unrelated ID collision cannot emit events"),
        )
        .await;
        assert!(
            error.is_err(),
            "an occupied ID without a natural winner is a storage error"
        );
        assert_eq!(snapshot(&backend), before);
    }
}

#[tokio::test]
async fn created_symmetric_edge_and_event_use_canonical_endpoints() {
    let backend = prepared(StorageBackend::memory().unwrap());
    let mut attempted = edge();
    attempted.source_id = Uuid::from_u128(20);
    attempted.target_id = Uuid::from_u128(10);
    attempted.relation = EdgeRelation::CompetesWith;
    let created = assert_created(insert(&backend, attempted.clone()).await);
    assert_eq!(created.source_id, attempted.target_id);
    assert_eq!(created.target_id, attempted.source_id);
    assert_eq!(created.id, attempted.id);
    assert_eq!(created.metadata, attempted.metadata);
    let stored = backend
        .graph()
        .unwrap()
        .get_edge(created.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&stored).unwrap(),
        serde_json::to_value(&created).unwrap()
    );
    let reader = backend.pool().reader().unwrap();
    let payload: String = reader
        .conn()
        .query_row("SELECT payload FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
        serde_json::json!({"source": created.source_id, "target": created.target_id})
    );
}

#[tokio::test]
async fn event_insert_failure_rolls_back_the_edge_on_both_writer_routes() {
    for queued in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let backend = file_backend(&dir.path().join("rollback.db"), queued);
        assert_eq!(
            backend.pool().writer_task_handle().unwrap().is_some(),
            queued
        );
        assert_created(insert(&backend, edge()).await);
        let before = snapshot(&backend);
        backend.pool().try_writer().unwrap().conn().execute_batch(
            "CREATE TRIGGER refuse_fixture_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'fixture event refused'); END"
        ).unwrap();
        let attempted = edge();
        let id = attempted.id;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let result = compose_graph_mutation_events(
            &backend,
            GraphMutationRequest::InsertIfAbsent { edge: attempted },
            Default::default(),
            vec![],
            move |outcome| {
                observed.fetch_add(1, Ordering::SeqCst);
                events(outcome)
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "failure must occur after edge insertion"
        );
        assert_eq!(snapshot(&backend), before);
        assert!(backend
            .graph()
            .unwrap()
            .get_edge(id)
            .await
            .unwrap()
            .is_none());
        let sequence_rows: i64 = backend
            .pool()
            .reader()
            .unwrap()
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM graph_edges_seq WHERE edge_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            sequence_rows, 0,
            "the list-cursor ledger must roll back too"
        );
        backend
            .pool()
            .try_writer()
            .unwrap()
            .conn()
            .execute_batch("DROP TRIGGER refuse_fixture_event")
            .unwrap();
        assert_created(insert(&backend, edge()).await);
        let after = snapshot(&backend);
        assert_eq!(
            (after.0.len(), after.1.len()),
            (2, 2),
            "rollback must leave the writer reusable"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_and_insert_hold_immediate_against_a_second_writer_on_both_routes() {
    for queued in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("immediate.db");
        let backend = Arc::new(file_backend(&path, queued));
        assert_eq!(
            backend.pool().writer_task_handle().unwrap().is_some(),
            queued
        );
        backend.pool().try_writer().unwrap().conn().execute_batch(
            "CREATE TABLE fixture_guard(ok INTEGER NOT NULL); INSERT INTO fixture_guard VALUES(1)"
        ).unwrap();
        // Open and prepare the independent connection before the rendezvous, so
        // an unrelated setup lock cannot masquerade as the protected UPDATE.
        let racer = rusqlite::Connection::open(&path).unwrap();
        racer.busy_timeout(Duration::ZERO).unwrap();
        let mut racer_update = racer.prepare("UPDATE fixture_guard SET ok=0").unwrap();
        let attempted = edge();
        let (reached_tx, reached_rx) = channel();
        let (proceed_tx, proceed_rx) = channel();
        {
            let mut slot = AFTER_GUARDS.lock().unwrap();
            assert!(slot.is_none());
            *slot = Some(GuardBarrier {
                id: attempted.id,
                reached: reached_tx,
                proceed: proceed_rx,
            });
        }
        let writer_backend = backend.clone();
        let writer = tokio::spawn(async move {
            compose_graph_mutation_events(
                &writer_backend,
                GraphMutationRequest::InsertIfAbsent { edge: attempted },
                conditions(vec![guard(
                    "racer-visible-state",
                    "SELECT ok FROM fixture_guard",
                )]),
                vec![],
                events,
            )
            .await
        });
        tokio::task::spawn_blocking(move || reached_rx.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("actual production guard/insert seam must be reached");
        let blocked = racer_update.execute([]);
        // Always release before asserting, including when the negative control
        // allows this UPDATE. There is no sleep or scheduler-luck assertion.
        proceed_tx.send(()).unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(10), writer)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&blocked, Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(error.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)),
            "the second writer must be refused inside the actual guard/insert window: {blocked:?}"
        );
        assert_created(outcome.unwrap());
        assert_eq!(
            racer_update.execute([]).unwrap(),
            1,
            "the same writer succeeds only after composition commits"
        );
        let rows = snapshot(&backend);
        assert_eq!((rows.0.len(), rows.1.len()), (1, 1));
        assert!(AFTER_GUARDS.lock().unwrap().is_none());
    }
}
