use std::any::Any;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_storage::event::{EventAppendDisposition, EventGroupBy, EventPageQuery};
use khive_storage::{
    Entity, EntityStore, Event, EventFilter, EventStore, GraphStore, Note, NoteFilter, NoteStore,
    PageRequest, SqlAccess, SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult,
    WriterTaskRequestState,
};
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

use super::map_open_error;
use crate::backend::{StorageBackend, StoreSchemaKind};
use crate::error::SqliteError;
use crate::pool::PoolConfig;

const KINDS: [StoreSchemaKind; 4] = [
    StoreSchemaKind::Entities,
    StoreSchemaKind::Notes,
    StoreSchemaKind::Graph,
    StoreSchemaKind::Events,
];

fn backend(path: &Path, queue: bool) -> StorageBackend {
    StorageBackend::sqlite_with_pool_config(
        path,
        PoolConfig {
            write_queue_enabled: Some(queue),
            write_routing_strict: false,
            wal_mode: true,
            wal_ceiling: crate::pool::WalCeilingPolicy::default(),
            disk_guard_config: Some(crate::disk_guard_config::EffectiveDiskGuardConfig {
                reserve_bytes: 0,
                ..Default::default()
            }),
            write_admission_deadline_ms: 2_000,
            write_queue_capacity: 256,
            volume_lock_dir: Some(path.with_extension("locks")),
            ..PoolConfig::for_test()
        },
        None,
    )
    .unwrap()
}

fn attempts(backend: &StorageBackend, kind: StoreSchemaKind) -> usize {
    backend.store_schemas[kind as usize]
        .attempts
        .load(Ordering::Relaxed)
}

fn schema(backend: &StorageBackend) -> Vec<(String, String)> {
    let reader = backend.pool.reader().unwrap();
    let mut statement = reader
        .conn()
        .prepare("SELECT type, name FROM sqlite_schema ORDER BY type, name")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

async fn cold_read(backend: &StorageBackend, kind: StoreSchemaKind) -> StorageResult<()> {
    match kind {
        StoreSchemaKind::Entities => {
            EntityStore::count_entities(backend, "local", Default::default())
                .await
                .map(|_| ())
        }
        StoreSchemaKind::Notes => NoteStore::count_notes(backend, "local", None)
            .await
            .map(|_| ()),
        StoreSchemaKind::Graph => GraphStore::edge_sequence(backend, Uuid::new_v4())
            .await
            .map(|_| ()),
        StoreSchemaKind::Events => EventStore::count_events(backend, Default::default())
            .await
            .map(|_| ()),
        StoreSchemaKind::Agents => unreachable!(),
    }
}

fn event() -> Event {
    Event::new(
        "local",
        "create",
        EventKind::EntityCreated,
        SubstrateKind::Entity,
        "fixture",
    )
}

#[tokio::test]
async fn coercion_and_pure_probes_leave_cold_queued_backend_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(backend(&dir.path().join("cold.db"), true));
    let before_schema = schema(&backend);
    let before_writes = backend.pool.writer_acquisition_snapshot();
    let entity: Arc<dyn EntityStore> = backend.clone();
    let note: Arc<dyn NoteStore> = backend.clone();
    let graph: Arc<dyn GraphStore> = backend.clone();
    let events: Arc<dyn EventStore> = backend.clone();
    let sql: Arc<dyn SqlAccess> = backend.clone();
    assert_eq!(backend.pool.writer_task_spawn_count(), 0);
    let valid = event();
    events.preflight_event(&valid).unwrap();
    let mut invalid = valid.clone();
    invalid.op_index = Some(1);
    invalid.ref_resolution = None;
    assert!(crate::stores::event::event_insert_statements(&invalid).is_err());
    let error = events.preflight_event(&invalid).unwrap_err();
    let StorageError::Driver {
        capability,
        operation,
        source,
    } = error
    else {
        panic!("{error}")
    };
    assert_eq!(capability, StorageCapability::Events);
    assert_eq!(operation, "preflight_event");
    assert!(source.downcast_ref::<rusqlite::Error>().is_some());
    assert!(events.supports_idempotent_audit_batch());
    assert_eq!(
        sql.database_path(),
        Some(std::fs::canonicalize(dir.path().join("cold.db")).unwrap())
    );
    assert_eq!(schema(&backend), before_schema);
    assert_eq!(backend.pool.writer_acquisition_snapshot(), before_writes);
    assert_eq!(backend.pool.writer_task_spawn_count(), 0);
    assert_eq!(backend.notes_seq_repair_run_count(), 0);
    for kind in KINDS {
        assert_eq!(attempts(&backend, kind), 0);
    }
    assert_eq!(
        entity
            .count_entities("local", Default::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(note.count_notes("local", None).await.unwrap(), 0);
    assert_eq!(graph.count_edges(Default::default()).await.unwrap(), 0);
    assert_eq!(events.count_events(Default::default()).await.unwrap(), 0);
    for kind in KINDS {
        cold_read(&backend, kind).await.unwrap();
        assert_eq!(attempts(&backend, kind), 1);
    }
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
}

#[tokio::test]
async fn warm_handles_do_not_reacquire_schema_writers_or_repeat_repairs() {
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(&dir.path().join("warm.db"), false);
    backend.prepare_core_schema().unwrap();
    backend.entities().unwrap();
    backend.notes().unwrap();
    backend.graph().unwrap();
    backend.events().unwrap();
    let before = backend.pool.writer_acquisition_snapshot();
    for _ in 0..2 {
        for kind in KINDS {
            cold_read(&backend, kind).await.unwrap();
        }
        EventStore::preflight_event(&backend, &event()).unwrap();
    }
    assert_eq!(backend.pool.writer_acquisition_snapshot(), before);
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
    for kind in KINDS {
        assert_eq!(attempts(&backend, kind), 1);
    }
}

#[tokio::test]
async fn current_schema_cold_handle_repairs_historical_sequence_hole_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopened.db");
    let note = Note::new("local", "observation", "historical");
    {
        let backend = backend(&path, false);
        backend.prepare_core_schema().unwrap();
        NoteStore::upsert_note(&backend, note.clone())
            .await
            .unwrap();
        let writer = backend.pool.try_writer().unwrap();
        assert_eq!(
            writer
                .conn()
                .execute(
                    "DELETE FROM notes_seq WHERE note_id=?1",
                    [note.id.to_string()]
                )
                .unwrap(),
            1
        );
    }
    let backend = Arc::new(backend(&path, false));
    let notes: Arc<dyn NoteStore> = backend.clone();
    assert_eq!(backend.notes_seq_repair_run_count(), 0);
    assert_eq!(attempts(&backend, StoreSchemaKind::Notes), 0);
    {
        let reader = backend.pool.reader().unwrap();
        assert_eq!(
            reader
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM notes_seq WHERE note_id=?1",
                    [note.id.to_string()],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
    assert_eq!(
        notes.get_note(note.id).await.unwrap().unwrap().content,
        "historical"
    );
    assert!(notes.note_sequence(note.id).await.unwrap().is_some());
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
    for kind in KINDS {
        cold_read(&backend, kind).await.unwrap();
        assert_eq!(attempts(&backend, kind), 1);
    }
}

#[tokio::test]
async fn each_core_trait_returns_typed_ddl_failure_and_retries_on_same_backend() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let backend = backend(&dir.path().join("retry.db"), false);
        backend
            .pool
            .try_writer()
            .unwrap()
            .conn()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        let error = cold_read(&backend, kind).await.unwrap_err();
        let StorageError::Driver { source, .. } = error else {
            panic!("{kind:?}: {error}")
        };
        let sqlite = source
            .downcast_ref::<SqliteError>()
            .expect("getter retains SqliteError cause");
        assert!(
            matches!(sqlite, SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, _)) if code.code == rusqlite::ErrorCode::ReadOnly)
        );
        assert_eq!(attempts(&backend, kind), 1);
        backend
            .pool
            .try_writer()
            .unwrap()
            .conn()
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
        cold_read(&backend, kind).await.unwrap();
        cold_read(&backend, kind).await.unwrap();
        assert_eq!(attempts(&backend, kind), 2);
    }
}

#[tokio::test]
async fn notes_schema_success_does_not_cache_failed_sequence_repair() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    let dir = tempfile::tempdir().unwrap();
    let backend = backend(&dir.path().join("repair.db"), false);
    let id = Uuid::new_v4();
    let denied = Arc::new(AtomicBool::new(false));
    {
        let writer = backend.pool.try_writer().unwrap();
        crate::stores::note::ensure_notes_schema(writer.conn()).unwrap();
        writer.conn().execute("INSERT INTO notes(id,namespace,kind,content,created_at,updated_at) VALUES (?1,'local','observation','legacy',1,1)", [id.to_string()]).unwrap();
        writer
            .conn()
            .execute("DELETE FROM notes_seq WHERE note_id=?1", [id.to_string()])
            .unwrap();
        let fired = denied.clone();
        writer.conn().authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Insert { table_name } if table_name == "notes_seq") {
                fired.store(true, Ordering::Relaxed); Authorization::Deny
            } else { Authorization::Allow }
        })).unwrap();
    }
    let notes: &dyn NoteStore = &backend;
    let error = notes.count_notes("local", None).await.unwrap_err();
    assert!(denied.load(Ordering::Relaxed));
    let StorageError::Driver {
        capability,
        operation,
        source,
    } = error
    else {
        panic!("{error}")
    };
    assert_eq!(capability, StorageCapability::Notes);
    assert_eq!(operation, "notes");
    assert!(matches!(
        source.downcast_ref::<SqliteError>(),
        Some(SqliteError::Rusqlite(_))
    ));
    assert_eq!(attempts(&backend, StoreSchemaKind::Notes), 1);
    assert_eq!(backend.notes_seq_repair_run_count(), 0);
    backend
        .pool
        .try_writer()
        .unwrap()
        .conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(notes.count_notes("local", None).await.unwrap(), 1);
    assert!(notes.note_sequence(id).await.unwrap().is_some());
    assert_eq!(backend.notes_seq_repair_run_count(), 1);
    assert_eq!(attempts(&backend, StoreSchemaKind::Notes), 1);
}

#[tokio::test]
async fn cancelled_cold_trait_call_preserves_admission_timeout_without_ddl() {
    for kind in KINDS {
        let dir = tempfile::tempdir().unwrap();
        let backend = backend(&dir.path().join("cancel.db"), false);
        let before = backend.pool.writer_acquisition_snapshot();
        let (_sender, receiver) = tokio::sync::watch::channel(true);
        let result = khive_storage::scope_request_read_cancellation(receiver, async {
            khive_storage::capture_request_read_context()
                .scope_store_acquisition("cold_handle", || {
                    futures::executor::block_on(cold_read(&backend, kind))
                })
        })
        .await;
        assert!(
            matches!(result, Err(StorageError::Timeout { operation }) if operation == "cold_handle")
        );
        assert_eq!(attempts(&backend, kind), 0);
        assert_eq!(backend.pool.writer_acquisition_snapshot(), before);
    }
}

#[tokio::test]
async fn memory_core_handles_keep_conditional_writes_sequences_and_event_batch_results() {
    let backend = Arc::new(StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let entities: Arc<dyn EntityStore> = backend.clone();
    let notes: Arc<dyn NoteStore> = backend.clone();
    let events: Arc<dyn EventStore> = backend.clone();
    let entity = Entity::new("other", "concept", "kept");
    assert!(entities
        .insert_entity_if_absent(entity.clone())
        .await
        .unwrap());
    assert!(!entities
        .insert_entity_if_absent(entity.clone())
        .await
        .unwrap());
    assert_eq!(
        entities
            .get_entity(entity.id)
            .await
            .unwrap()
            .unwrap()
            .namespace,
        "other"
    );
    let mut note = Note::new("local", "observation", "before");
    assert!(notes.insert_note_if_absent(note.clone()).await.unwrap());
    assert!(!notes.insert_note_if_absent(note.clone()).await.unwrap());
    let original = note.updated_at;
    note.content = "after".into();
    note.updated_at += 1;
    assert!(notes
        .replace_note_if_unchanged(note.clone(), original, None)
        .await
        .unwrap());
    assert!(!notes
        .replace_note_if_unchanged(note.clone(), original, None)
        .await
        .unwrap());
    assert!(notes.note_sequence(note.id).await.unwrap().is_some());
    let page = notes
        .query_notes_filtered_count_free(
            "local",
            &NoteFilter::default(),
            PageRequest {
                offset: 0,
                limit: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, None);
    assert_eq!(page.items[0].content, "after");
    let event = event();
    assert_eq!(
        events
            .append_events_idempotent(vec![event.clone()])
            .await
            .unwrap()
            .rows,
        vec![EventAppendDisposition::Inserted]
    );
    assert_eq!(
        events
            .append_events_idempotent(vec![event.clone()])
            .await
            .unwrap()
            .rows,
        vec![EventAppendDisposition::AlreadyPresentIdentical]
    );
    let mut changed = event.clone();
    changed.actor = "changed".into();
    assert_eq!(
        events
            .append_events_idempotent(vec![changed])
            .await
            .unwrap()
            .rows,
        vec![EventAppendDisposition::IdentityConflict]
    );
    assert_eq!(
        events
            .count_events_grouped(EventFilter::default(), EventGroupBy::Actor)
            .await
            .unwrap(),
        BTreeMap::from([("fixture".to_string(), 1)])
    );
    let window = events
        .query_event_page(EventPageQuery {
            since_us: event.created_at,
            until_us: event.created_at + 1,
            kinds: vec![],
            actors: vec![],
            exclude_namespaces: vec![],
            after: None,
            max_rows: 1,
        })
        .await
        .unwrap();
    assert_eq!(window.rows.len(), 1);
    assert_eq!(window.rows[0].event.id, event.id);
}

#[tokio::test]
async fn graph_handle_keeps_guarded_write_outcomes_and_compare_and_set() {
    use khive_storage::types::{Edge, GuardedWriteOutcome};
    let backend = StorageBackend::memory().unwrap();
    backend.prepare_core_schema().unwrap();
    let source = Entity::new("local", "concept", "source");
    let target = Entity::new("local", "concept", "target");
    EntityStore::upsert_entity(&backend, source.clone())
        .await
        .unwrap();
    let mut edge = Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id: source.id,
        target_id: target.id,
        relation: khive_types::EdgeRelation::DependsOn,
        weight: 0.7,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let graph: &dyn GraphStore = &backend;
    assert!(matches!(
        graph.upsert_edge_guarded(edge.clone()).await.unwrap(),
        GuardedWriteOutcome::Refused(_)
    ));
    assert!(graph.get_edge(edge.id).await.unwrap().is_none());
    EntityStore::upsert_entity(&backend, target).await.unwrap();
    assert_eq!(
        graph.upsert_edge_guarded(edge.clone()).await.unwrap(),
        GuardedWriteOutcome::Written
    );
    let original = graph.get_edge(edge.id).await.unwrap().unwrap().updated_at;
    edge.updated_at = original + chrono::Duration::microseconds(1);
    edge.weight = 0.9;
    assert!(graph
        .replace_edge_if_unchanged(edge.clone(), original, None)
        .await
        .unwrap());
    assert!(!graph
        .replace_edge_if_unchanged(edge.clone(), original, None)
        .await
        .unwrap());
    assert_eq!(graph.get_edge(edge.id).await.unwrap().unwrap().weight, 0.9);
}

#[tokio::test]
async fn readonly_missing_core_schema_returns_leaf_errors_without_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing-schema.db");
    {
        let _identity_only = backend(&path, false);
    }
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let before_schema = schema(&readonly);
    let before = readonly.pool.writer_acquisition_snapshot();
    for kind in KINDS {
        let error = cold_read(&readonly, kind).await.unwrap_err();
        let StorageError::Driver { source, .. } = error else {
            panic!("{kind:?}: {error}")
        };
        let cause = source
            .downcast_ref::<rusqlite::Error>()
            .expect("leaf SQLite source");
        assert_eq!(
            cause.sqlite_error().unwrap().extended_code,
            rusqlite::ffi::SQLITE_ERROR
        );
        assert!(cause.to_string().contains("no such table"), "{cause}");
        assert_eq!(attempts(&readonly, kind), 0);
    }
    assert_eq!(schema(&readonly), before_schema);
    assert_eq!(readonly.notes_seq_repair_run_count(), 0);
    assert_eq!(readonly.pool.writer_acquisition_snapshot(), before);
}

#[tokio::test]
async fn readonly_core_operations_use_existing_schema_without_repairs_or_writers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshot.db");
    let note = Note::new("local", "observation", "frozen");
    {
        let writable = backend(&path, false);
        writable.prepare_core_schema().unwrap();
        NoteStore::upsert_note(&writable, note.clone())
            .await
            .unwrap();
    }
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(&path);
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let before = readonly.pool.writer_acquisition_snapshot();
    assert_eq!(
        NoteStore::get_note(&readonly, note.id)
            .await
            .unwrap()
            .unwrap()
            .content,
        "frozen"
    );
    for kind in KINDS {
        cold_read(&readonly, kind).await.unwrap();
        assert_eq!(attempts(&readonly, kind), 0);
    }
    assert_eq!(readonly.notes_seq_repair_run_count(), 0);
    assert_eq!(readonly.pool.writer_acquisition_snapshot(), before);
}

fn statement(sql: &str) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params: vec![],
        label: None,
    }
}

#[tokio::test]
async fn sql_trait_shares_direct_and_queued_atomic_commit_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    for mode in 0..3 {
        let backend = Arc::new(if mode == 0 {
            StorageBackend::memory().unwrap()
        } else {
            backend(&dir.path().join(format!("sql-{mode}.db")), mode == 2)
        });
        let sql: Arc<dyn SqlAccess> = backend.clone();
        assert_eq!(sql.database_path(), backend.sql().database_path());
        sql.writer()
            .await
            .unwrap()
            .execute_script("CREATE TABLE marker(value INTEGER)".into())
            .await
            .unwrap();
        let committed = sql
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    assert_eq!(
                        writer
                            .execute(statement("INSERT INTO marker VALUES (1)"))
                            .await?,
                        1
                    );
                    Ok(Box::new(7_u64) as Box<dyn Any + Send>)
                })
            }))
            .await
            .unwrap();
        assert_eq!(*committed.downcast::<u64>().unwrap(), 7);
        let error = sql
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute(statement("INSERT INTO marker VALUES (2)"))
                        .await?;
                    Err(StorageError::Internal("rollback witness".into()))
                })
            }))
            .await
            .unwrap_err();
        match error {
            StorageError::Internal(message) => assert_eq!(message, "rollback witness"),
            StorageError::WriterTaskRequestFailed {
                request_state,
                source,
            } => {
                assert_eq!(request_state, WriterTaskRequestState::TransactionRolledBack);
                assert!(
                    matches!(*source, StorageError::Internal(ref message) if message == "rollback witness")
                );
            }
            other => panic!("unexpected rollback classification: {other}"),
        }
        let row = sql
            .reader()
            .await
            .unwrap()
            .query_row(statement(
                "SELECT COUNT(*) AS n, SUM(value) AS s FROM marker",
            ))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(row.columns[0].value, SqlValue::Integer(1)));
        assert!(matches!(row.columns[1].value, SqlValue::Integer(1)));
        assert_eq!(
            backend.pool.writer_task_spawn_count(),
            usize::from(mode == 2)
        );
    }
}

#[test]
fn acquisition_mapping_preserves_capacity_settlement_and_original_stop_error() {
    let stopped = map_open_error(
        SqliteError::RequestReadStopped(StorageError::Timeout {
            operation: "original".into(),
        }),
        StorageCapability::Notes,
        "notes",
    );
    assert!(matches!(stopped, StorageError::Timeout { operation } if operation == "original"));
    let floor = map_open_error(
        SqliteError::CapacityFloor {
            volume: "volume".into(),
            available_bytes: 9,
            floor_bytes: 10,
            required_headroom_bytes: 2,
        },
        StorageCapability::Notes,
        "notes",
    );
    assert!(matches!(
        floor,
        StorageError::CapacityFloor {
            capability: StorageCapability::Sql,
            available_bytes: 9,
            floor_bytes: 10,
            required_headroom_bytes: 2,
            ..
        }
    ));
    for (error, state) in [
        (
            SqliteError::WriterPoisoned,
            WriterTaskRequestState::NotStarted,
        ),
        (
            SqliteError::WriterSettlementUnknown,
            WriterTaskRequestState::SideEffectsUnknown,
        ),
    ] {
        assert!(
            matches!(map_open_error(error, StorageCapability::Entities, "entities"), StorageError::WriterTaskTerminated { request_state, sqlite_full_codes: None } if request_state == state)
        );
    }
}

#[test]
fn every_live_core_trait_method_has_an_explicit_matching_backend_override() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut total = 0;
    for (name, file) in [
        ("EntityStore", "entity"),
        ("NoteStore", "note"),
        ("GraphStore", "graph"),
        ("EventStore", "event"),
        ("SqlAccess", "sql"),
    ] {
        let declared = syn::parse_file(
            &std::fs::read_to_string(root.join(format!("../khive-storage/src/{file}.rs"))).unwrap(),
        )
        .unwrap();
        let trait_ = declared
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Trait(item) if item.ident == name => Some(item),
                _ => None,
            })
            .unwrap();
        let expected: BTreeMap<_, _> = trait_
            .items
            .iter()
            .filter_map(|item| match item {
                syn::TraitItem::Fn(method) => Some((
                    method.sig.ident.to_string(),
                    (method.sig.asyncness.is_some(), method.sig.inputs.len()),
                )),
                _ => None,
            })
            .collect();
        let actual = syn::parse_file(
            &std::fs::read_to_string(root.join(format!("src/backend/core_stores/{file}.rs")))
                .unwrap(),
        )
        .unwrap();
        let implementations: Vec<_> = actual
            .items
            .iter()
            .filter_map(|item| match item {
                syn::Item::Impl(item)
                    if item.trait_.as_ref().is_some_and(|(_, path, _)| {
                        path.segments.last().unwrap().ident == name
                    }) && matches!(item.self_ty.as_ref(), syn::Type::Path(path)
                        if path.qself.is_none() && path.path.is_ident("StorageBackend")) =>
                {
                    Some(item)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            implementations.len(),
            1,
            "exactly one {name} impl for StorageBackend"
        );
        let impl_ = implementations[0];
        let actual: BTreeMap<_, _> = impl_
            .items
            .iter()
            .filter_map(|item| match item {
                syn::ImplItem::Fn(method) => Some((
                    method.sig.ident.to_string(),
                    (method.sig.asyncness.is_some(), method.sig.inputs.len()),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            actual, expected,
            "{name}: forward newly added defaults explicitly"
        );
        total += expected.len();
    }
    assert_eq!(
        total, 93,
        "review every new core method and update the bound inventory"
    );
}
