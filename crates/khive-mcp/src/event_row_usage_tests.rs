//! Real storage writes observed through the production usage-envelope stamper.
use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_db::{ConnectionPool, PoolConfig};
use khive_runtime::atomic_prepare::prepare_op;
use khive_runtime::atomic_runner::{run_atomic_unit, AtomicRunOutcome};
use khive_runtime::{
    ContentMergeStrategy, EntityDedupMergePolicy, KhiveRuntime, Namespace, NamespaceToken,
    RuntimeConfig,
};
use khive_storage::types::{Edge, LinkId, PageRequest, SqlStatement};
use khive_storage::usage::{scope, UsageContext, UsageUnit};
use khive_storage::{
    AtomicUnitOp, Event, EventFilter, SqlAccess, StorageError, WriterTaskRequestState,
};
use khive_types::{EdgeRelation, EventKind, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

use super::stamp_usage;

fn runtime() -> (KhiveRuntime, tempfile::TempDir, NamespaceToken) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(directory.path().join("event-usage.db")),
        actor_id: Some("event-usage-fixture".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    (runtime, directory, token)
}

async fn events(runtime: &KhiveRuntime, token: &NamespaceToken) -> Vec<Event> {
    runtime
        .events(token)
        .unwrap()
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items
}

fn assert_envelope_rows(context: &UsageContext, rows: usize) {
    context.freeze();
    let mut envelope = json!({"ok": true});
    stamp_usage(&mut envelope, context);
    assert_eq!(envelope["ok"], true);
    assert_eq!(
        envelope["usage"]
            .get("event_rows")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        rows as u64,
        "the production usage envelope must count exactly the appended event rows"
    );
}

async fn seed_entity(runtime: &KhiveRuntime, token: &NamespaceToken, name: &str) -> Uuid {
    let entity = khive_storage::Entity::new("local", "concept", name);
    let id = entity.id;
    runtime
        .entities(token)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
    id
}

async fn seed_note(runtime: &KhiveRuntime, token: &NamespaceToken, content: &str) -> Uuid {
    let note = khive_storage::Note::new("local", "observation", content);
    let id = note.id;
    runtime
        .notes(token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    id
}

async fn seed_edge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source: Uuid,
    target: Uuid,
    relation: EdgeRelation,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    runtime
        .graph(token)
        .unwrap()
        .upsert_edge(Edge {
            id: LinkId(id),
            namespace: "local".into(),
            source_id: source,
            target_id: target,
            relation,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        })
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn atomic_plan_usage_matches_entity_note_edge_and_link_event_rows() {
    for path in [
        "entity_update",
        "note_update",
        "edge_update",
        "entity_delete",
        "note_delete",
        "edge_delete",
        "link",
    ] {
        let (runtime, _directory, token) = runtime();
        let first = seed_entity(&runtime, &token, "first").await;
        let second = seed_entity(&runtime, &token, "second").await;
        let note = seed_note(&runtime, &token, "before").await;
        let edge = seed_edge(&runtime, &token, first, second, EdgeRelation::Extends).await;
        let (verb, arguments, kind) = match path {
            "entity_update" => (
                "update",
                json!({"id": first, "name": "changed"}),
                EventKind::EntityUpdated,
            ),
            "note_update" => (
                "update",
                json!({"id": note, "content": "changed", "embed": false}),
                EventKind::NoteUpdated,
            ),
            "edge_update" => (
                "update",
                json!({"id": edge, "weight": 0.5}),
                EventKind::EdgeUpdated,
            ),
            "entity_delete" => ("delete", json!({"id": first}), EventKind::EntityDeleted),
            "note_delete" => ("delete", json!({"id": note}), EventKind::NoteDeleted),
            "edge_delete" => ("delete", json!({"id": edge}), EventKind::EdgeDeleted),
            "link" => (
                "link",
                json!({"source_id": first, "target_id": second, "relation": "variant_of"}),
                EventKind::LinkCreated,
            ),
            _ => unreachable!(),
        };
        let before = events(&runtime, &token).await;
        let plan = prepare_op(&runtime, &token, verb, &arguments)
            .await
            .unwrap();
        let context = UsageContext::new();
        let result = scope(
            context.clone(),
            run_atomic_unit(runtime.sql().as_ref(), vec![plan]),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, AtomicRunOutcome::Committed { .. }),
            "{path}: {result:?}"
        );
        let after = events(&runtime, &token).await;
        let appended: Vec<_> = after
            .iter()
            .filter(|event| !before.iter().any(|old| old.id == event.id))
            .collect();
        assert_eq!(appended.len(), 1, "{path}");
        assert_eq!(appended[0].kind, kind, "{path}");
        assert_envelope_rows(&context, appended.len());
    }
}

#[tokio::test]
async fn atomic_lineage_usage_counts_only_warning_rows_actually_written() {
    let protected = [
        EdgeRelation::DerivedFrom,
        EdgeRelation::Supersedes,
        EdgeRelation::Precedes,
        EdgeRelation::Supports,
        EdgeRelation::Refutes,
    ];
    for substrate in ["entity", "note", "edge"] {
        for warning_rows in [0, 1, 5] {
            let (runtime, _directory, token) = runtime();
            let first = seed_entity(&runtime, &token, "first").await;
            let second = seed_entity(&runtime, &token, "second").await;
            let target = match substrate {
                "entity" => first,
                "note" => seed_note(&runtime, &token, "doomed").await,
                "edge" => seed_edge(&runtime, &token, first, second, EdgeRelation::Extends).await,
                _ => unreachable!(),
            };
            for relation in protected.iter().take(warning_rows) {
                seed_edge(&runtime, &token, second, target, *relation).await;
            }
            let before = events(&runtime, &token).await;
            let plan = prepare_op(
                &runtime,
                &token,
                "delete",
                &json!({"id": target, "hard": true}),
            )
            .await
            .unwrap();
            let context = UsageContext::new();
            let result = scope(
                context.clone(),
                run_atomic_unit(runtime.sql().as_ref(), vec![plan]),
            )
            .await
            .unwrap();
            assert!(matches!(result, AtomicRunOutcome::Committed { .. }));
            let after = events(&runtime, &token).await;
            let appended: Vec<_> = after
                .iter()
                .filter(|event| !before.iter().any(|old| old.id == event.id))
                .collect();
            assert_eq!(
                appended.len(),
                warning_rows + 1,
                "{substrate}, {warning_rows}"
            );
            assert_eq!(
                appended
                    .iter()
                    .filter(|event| event.kind == EventKind::Audit)
                    .count(),
                warning_rows
            );
            assert!(appended.iter().all(|event| event.target_id == Some(target)));
            assert_envelope_rows(&context, appended.len());
        }
    }
}

#[tokio::test]
async fn runtime_hard_delete_counts_warning_and_lifecycle_event_once() {
    let (runtime, _directory, token) = runtime();
    let target = seed_entity(&runtime, &token, "doomed").await;
    let source = seed_entity(&runtime, &token, "source").await;
    seed_edge(&runtime, &token, source, target, EdgeRelation::DerivedFrom).await;
    let before = events(&runtime, &token).await;
    let context = UsageContext::new();
    let deleted = scope(context.clone(), runtime.delete_entity(&token, target, true))
        .await
        .unwrap();
    assert!(deleted);
    let after = events(&runtime, &token).await;
    let appended: Vec<_> = after
        .iter()
        .filter(|event| !before.iter().any(|old| old.id == event.id))
        .collect();
    assert_eq!(appended.len(), 2);
    assert_eq!(
        appended
            .iter()
            .filter(|event| event.kind == EventKind::Audit)
            .count(),
        1
    );
    assert_eq!(
        appended
            .iter()
            .filter(|event| event.kind == EventKind::EntityDeleted)
            .count(),
        1
    );
    assert_envelope_rows(&context, appended.len());
}

#[tokio::test]
async fn atomic_rollback_discards_pending_event_rows_and_usage() {
    let (runtime, _directory, token) = runtime();
    let target = seed_entity(&runtime, &token, "before").await;
    let before = events(&runtime, &token).await;
    let plan = prepare_op(
        &runtime,
        &token,
        "update",
        &json!({"id": target, "name": "changed"}),
    )
    .await
    .unwrap();
    let context = UsageContext::new();
    let result = scope(
        context.clone(),
        run_atomic_unit(runtime.sql().as_ref(), vec![plan.clone(), plan]),
    )
    .await
    .unwrap();
    assert!(
        matches!(
            result,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                ..
            }
        ),
        "{result:?}"
    );
    assert_eq!(events(&runtime, &token).await.len(), before.len());
    assert_eq!(
        runtime
            .entities(&token)
            .unwrap()
            .get_entity(target)
            .await
            .unwrap()
            .unwrap()
            .name,
        "before"
    );
    assert_envelope_rows(&context, 0);
}

#[tokio::test]
async fn curation_merge_usage_matches_committed_rows_and_excludes_dry_runs() {
    for substrate in ["entity", "note"] {
        for dry_run in [false, true] {
            let (runtime, _directory, token) = runtime();
            let (into, from) = if substrate == "entity" {
                (
                    seed_entity(&runtime, &token, "into").await,
                    seed_entity(&runtime, &token, "from").await,
                )
            } else {
                (
                    seed_note(&runtime, &token, "into").await,
                    seed_note(&runtime, &token, "from").await,
                )
            };
            let before = events(&runtime, &token).await;
            let context = UsageContext::new();
            scope(context.clone(), async {
                if substrate == "entity" {
                    runtime
                        .merge_entity(
                            &token,
                            into,
                            from,
                            EntityDedupMergePolicy::PreferInto,
                            ContentMergeStrategy::Append,
                            dry_run,
                        )
                        .await
                        .unwrap();
                } else {
                    runtime
                        .merge_note(
                            &token,
                            into,
                            from,
                            EntityDedupMergePolicy::PreferInto,
                            ContentMergeStrategy::Append,
                            dry_run,
                        )
                        .await
                        .unwrap();
                }
            })
            .await;
            let after = events(&runtime, &token).await;
            let appended: Vec<_> = after
                .iter()
                .filter(|event| !before.iter().any(|old| old.id == event.id))
                .collect();
            assert_eq!(appended.len(), usize::from(!dry_run));
            if !dry_run {
                assert_eq!(
                    appended[0].kind,
                    if substrate == "entity" {
                        EventKind::EntityMerged
                    } else {
                        EventKind::NoteMerged
                    }
                );
            }
            assert_envelope_rows(&context, appended.len());
        }
    }
}

fn pool(file_backed: bool, queued: bool) -> (Arc<ConnectionPool>, Option<tempfile::TempDir>) {
    let directory = file_backed.then(|| tempfile::tempdir().unwrap());
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: directory
                .as_ref()
                .map(|directory| directory.path().join("writer-routes.db")),
            write_queue_enabled: Some(queued),
            write_routing_strict: false,
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(include_str!("../../khive-db/sql/events-ddl.sql"))
        .unwrap();
    (pool, directory)
}

fn event() -> Event {
    Event::new(
        "local",
        "usage-fixture",
        EventKind::Audit,
        SubstrateKind::Event,
        "agent:usage-fixture",
    )
}

#[tokio::test]
async fn atomic_writer_routes_count_committed_rows_and_preserve_return_values() {
    for (file_backed, queued) in [(false, false), (true, false), (true, true)] {
        let (pool, _directory) = pool(file_backed, queued);
        let bridge = khive_db::sql_bridge::SqlBridge::new(Arc::clone(&pool), file_backed);
        let store = khive_db::stores::event::SqlEventStore::new_scoped(
            Arc::clone(&pool),
            file_backed,
            "local",
        );
        let appended = event();
        let id = appended.id;
        let statements = khive_db::stores::event::event_insert_statements(&appended).unwrap();
        let operation: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                for statement in statements {
                    assert_eq!(writer.execute(statement).await?, 1);
                }
                Ok(Box::new(73_u64) as Box<dyn Any + Send>)
            })
        });
        let context = UsageContext::new();
        let value = scope(context.clone(), bridge.atomic_unit(operation))
            .await
            .unwrap();
        assert_eq!(
            *value
                .downcast::<u64>()
                .unwrap_or_else(|_| panic!("the committed return value must remain u64")),
            73
        );
        let rows = khive_storage::EventStore::query_events(
            &store,
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap()
        .items;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_envelope_rows(&context, rows.len());
    }
}

#[tokio::test]
async fn atomic_batch_counts_events_without_observation_or_unrelated_rows() {
    for (file_backed, queued) in [(false, false), (true, true)] {
        let (pool, _directory) = pool(file_backed, queued);
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch("CREATE TABLE unrelated (id INTEGER PRIMARY KEY)")
            .unwrap();
        let bridge = khive_db::sql_bridge::SqlBridge::new(Arc::clone(&pool), file_backed);
        let store = khive_db::stores::event::SqlEventStore::new_scoped(
            Arc::clone(&pool),
            file_backed,
            "local",
        );
        let mut appended = event();
        appended.kind = EventKind::SearchExecuted;
        appended.payload = json!({"result_kind": "note", "candidates": [Uuid::new_v4()], "selected": [Uuid::new_v4()]});
        let id = appended.id;
        let mut statements = khive_db::stores::event::event_insert_statements(&appended).unwrap();
        assert_eq!(statements.len(), 3);
        statements.push(SqlStatement {
            sql: "INSERT INTO unrelated VALUES (1)".into(),
            params: vec![],
            label: Some("unrelated-fixture".into()),
        });
        let operation: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let total = writer.execute_batch(statements).await?;
                Ok(Box::new(total) as Box<dyn Any + Send>)
            })
        });
        let context = UsageContext::new();
        let total = scope(context.clone(), bridge.atomic_unit(operation))
            .await
            .unwrap();
        assert_eq!(
            *total
                .downcast::<u64>()
                .unwrap_or_else(|_| panic!("the batch affected-row result must remain u64")),
            4
        );
        let rows = khive_storage::EventStore::query_events(
            &store,
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap()
        .items;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        let guard = pool.reader().unwrap();
        let observations: i64 = guard
            .query_row("SELECT COUNT(*) FROM event_observations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(observations, 2);
        drop(guard);
        assert_envelope_rows(&context, rows.len());
    }
}

#[tokio::test]
async fn event_store_append_usage_matches_actual_rows_on_all_writer_routes() {
    for (file_backed, queued) in [(false, false), (true, false), (true, true)] {
        let (pool, _directory) = pool(file_backed, queued);
        let store = khive_db::stores::event::SqlEventStore::new_scoped(
            Arc::clone(&pool),
            file_backed,
            "local",
        );
        let context = UsageContext::new();
        scope(context.clone(), async {
            khive_storage::EventStore::append_event(&store, event())
                .await
                .unwrap();
            let summary = khive_storage::EventStore::append_events(&store, vec![event(), event()])
                .await
                .unwrap();
            assert_eq!(summary.affected, 2);
        })
        .await;
        let rows = khive_storage::EventStore::query_events(
            &store,
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap()
        .items;
        assert_eq!(rows.len(), 3);
        assert_envelope_rows(&context, rows.len());
        let frozen = context.shipping_snapshot().unwrap();
        scope(
            context.clone(),
            khive_storage::EventStore::append_event(&store, event()),
        )
        .await
        .unwrap();
        assert_eq!(
            context.shipping_snapshot(),
            Some(frozen),
            "a post-snapshot enclosing audit cannot count itself"
        );
    }
}

#[tokio::test]
async fn unknown_atomic_write_outcome_omits_usage_from_the_real_envelope() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
    fn deny_rollback(context: AuthContext<'_>) -> Authorization {
        match context.action {
            AuthAction::Transaction {
                operation: TransactionOperation::Rollback,
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }
    }
    let (pool, _directory) = pool(false, false);
    pool.writer()
        .unwrap()
        .conn()
        .authorizer(Some(deny_rollback))
        .unwrap();
    let bridge = khive_db::sql_bridge::SqlBridge::new(Arc::clone(&pool), false);
    let statements = khive_db::stores::event::event_insert_statements(&event()).unwrap();
    let wrote = Arc::new(AtomicBool::new(false));
    let wrote_in_operation = Arc::clone(&wrote);
    let operation: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            for statement in statements {
                assert_eq!(writer.execute(statement).await?, 1);
            }
            wrote_in_operation.store(true, Ordering::SeqCst);
            Err(StorageError::Internal(
                "injected failure after an actual event insert".into(),
            ))
        })
    });
    let context = UsageContext::new();
    context.add(UsageUnit::DbRoundTrips, 1);
    context.freeze();
    let result = scope(context.clone(), bridge.atomic_unit(operation)).await;
    assert!(
        wrote.load(Ordering::SeqCst),
        "the refusal must follow an actual successful INSERT"
    );
    assert!(
        matches!(
            &result,
            Err(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
                ..
            }) | Err(StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown
            })
        ),
        "{:?}",
        result.as_ref().err()
    );
    assert!(
        context.snapshot().get("event_rows").is_none(),
        "an unknown commit outcome must not report a count"
    );
    let mut envelope = json!({"ok": false, "usage": {"stale": 1}});
    stamp_usage(&mut envelope, &context);
    assert_eq!(envelope["ok"], false);
    assert!(
        envelope.get("usage").is_none(),
        "the production stamper must omit all usage on an unknown write outcome"
    );
}
