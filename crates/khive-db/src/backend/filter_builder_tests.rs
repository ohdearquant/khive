use super::*;
use std::collections::BTreeSet;
use std::time::Duration;

use khive_storage::entity::{Entity, EntityFilter, EntityTombstones};
use khive_storage::event::{Event, EventFilter};
use khive_storage::note::Note;
use khive_storage::{
    DeleteMode, EntityStore, PageRequest, SqlValue, StorageCapability, StorageError,
};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::json;
use uuid::Uuid;

fn memory_backend() -> StorageBackend {
    crate::extension::ensure_extensions_loaded();
    let config = crate::pool::PoolConfig {
        path: None,
        code_map_vfs: None,
        #[cfg(any(unix, windows))]
        expected_file_identity: None,
        max_readers: 1,
        wal_mode: true,
        busy_timeout: Duration::from_secs(5),
        checkout_timeout: Duration::from_secs(5),
        journal_size_limit_bytes: 64 * 1024 * 1024,
        read_only: false,
        wal_ceiling: crate::pool::WalCeilingPolicy::default(),
        write_queue_enabled: Some(false),
        write_queue_capacity: 256,
        write_routing_strict: false,
        write_admission_deadline_ms: 2000,
        disk_guard_config: Some(crate::disk_guard_config::EffectiveDiskGuardConfig::default()),
        volume_lock_dir: None,
        read_tx_max_age: Duration::from_secs(120),
    };
    StorageBackend {
        pool: Arc::new(ConnectionPool::new(config).unwrap()),
        is_file_backed: false,
        path: None,
        vector_tables_ready: Default::default(),
        notes_seq_repair_runs: AtomicUsize::new(0),
        store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
    }
}

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn entity(n: u128, namespace: &str, properties: serde_json::Value) -> Entity {
    let mut row = Entity::new(namespace, "concept", format!("Name{n}"))
        .with_properties(properties)
        .with_entity_type(Some("topic"));
    row.id = id(n);
    row.created_at = n as i64;
    row.updated_at = n as i64;
    row
}

fn ids(rows: &[Entity]) -> BTreeSet<Uuid> {
    rows.iter().map(|row| row.id).collect()
}

async fn assert_entity_routes(store: &dyn EntityStore, filter: EntityFilter, expected: &[u128]) {
    let expected: BTreeSet<_> = expected.iter().copied().map(id).collect();
    let page = store
        .query_entities(
            "alpha",
            filter.clone(),
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(ids(&page.items), expected);
    assert_eq!(
        store.count_entities("alpha", filter.clone()).await.unwrap(),
        expected.len() as u64
    );
    let page = store
        .query_entities_count_free(
            "alpha",
            filter.clone(),
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(ids(&page.items), expected);
    assert_eq!(page.total, None);
    if filter.names_ci.is_empty() {
        let mut after = None;
        let mut seen = BTreeSet::new();
        loop {
            let page = store
                .query_entities_after("alpha", filter.clone(), after, 1)
                .await
                .unwrap();
            for row in page.items {
                assert!(seen.insert(row.id), "duplicate seek row");
            }
            match page.next_after {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
        assert_eq!(seen, expected);
    }
}

#[tokio::test]
async fn entity_liveness_reaches_every_query_route_without_live_only_indexes() {
    let backend = memory_backend();
    backend.prepare_core_schema().unwrap();
    let store = backend.entities().unwrap();
    for n in 1..=4 {
        store
            .upsert_entity(entity(
                n,
                if n == 4 { "beta" } else { "alpha" },
                json!({"group": if n == 3 { "other" } else { "keep" }}),
            ))
            .await
            .unwrap();
    }
    assert!(store.delete_entity(id(2), DeleteMode::Soft).await.unwrap());
    assert_entity_routes(store.as_ref(), EntityFilter::default(), &[1, 3]).await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().include_tombstones(),
        &[1, 2, 3],
    )
    .await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().tombstoned_only(),
        &[2],
    )
    .await;
    for filter in [
        EntityFilter {
            kinds: vec!["concept".into()],
            ..Default::default()
        },
        EntityFilter {
            entity_types: vec!["topic".into()],
            ..Default::default()
        },
        EntityFilter {
            ids: vec![id(1), id(2), id(3)],
            ..Default::default()
        },
        EntityFilter {
            names_ci: vec!["NAME1".into(), "name2".into(), "Name3".into()],
            ..Default::default()
        },
    ] {
        assert_entity_routes(
            store.as_ref(),
            filter.clone().include_tombstones(),
            &[1, 2, 3],
        )
        .await;
        assert_entity_routes(store.as_ref(), filter.clone().tombstoned_only(), &[2]).await;
        assert_entity_routes(
            store.as_ref(),
            filter
                .include_tombstones()
                .property_eq("$.group", SqlValue::Text("keep".into())),
            &[1, 2],
        )
        .await;
    }
    let all = EntityFilter::default()
        .property_eq("$.group", SqlValue::Text("keep".into()))
        .include_tombstones();
    assert_entity_routes(store.as_ref(), all.clone(), &[1, 2]).await;
    let mut namespaces = (0..501).map(|n| format!("unused{n}")).collect::<Vec<_>>();
    namespaces.extend(["beta".into(), "alpha".into(), "alpha".into()]);
    assert_entity_routes(
        store.as_ref(),
        EntityFilter { namespaces, ..all },
        &[1, 2, 4],
    )
    .await;
    assert_eq!(
        EntityFilter::default()
            .tombstoned_only()
            .include_tombstones()
            .tombstones,
        EntityTombstones::All
    );
    assert_eq!(
        EntityFilter::default()
            .include_tombstones()
            .tombstoned_only()
            .tombstones,
        EntityTombstones::Only
    );
    assert!(store.get_entity(id(2)).await.unwrap().is_none());
    assert!(store
        .get_entity_including_deleted(id(2))
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
}

#[tokio::test]
async fn entity_property_equalities_bind_paths_values_and_preserve_sql_null_and_boolean_policy() {
    let backend = memory_backend();
    backend.prepare_core_schema().unwrap();
    let store = backend.entities().unwrap();
    for (n, props) in [
        (
            1,
            json!({"nested":{"label":"O'Reilly"},"flag":true,"nil":null,"object":{"a":1}}),
        ),
        (
            2,
            json!({"nested":{"label":"other"},"flag":1,"nil":null,"object":{"a":2}}),
        ),
        (3, json!({"flag":false})),
        (4, json!({"flag":"1"})),
    ] {
        store
            .upsert_entity(entity(n, "alpha", props))
            .await
            .unwrap();
    }
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.flag", SqlValue::Bool(true)),
        &[1, 2],
    )
    .await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.flag", SqlValue::Float(1.0)),
        &[1, 2],
    )
    .await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.flag", SqlValue::Text("1".into())),
        &[4],
    )
    .await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.nil", SqlValue::Null),
        &[],
    )
    .await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.missing", SqlValue::Null),
        &[],
    )
    .await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.object", SqlValue::Json(json!({"a":1}))),
        &[1],
    )
    .await;
    let filtered = EntityFilter::default()
        .property_eq("$.nested.label", SqlValue::Text("O'Reilly".into()))
        .property_eq("$.flag", SqlValue::Integer(1));
    assert_entity_routes(store.as_ref(), filtered.clone(), &[1]).await;
    assert_entity_routes(
        store.as_ref(),
        EntityFilter::default().property_eq("$.nested.label", SqlValue::Text("' OR 1=1 --".into())),
        &[],
    )
    .await;
    let page = store
        .query_entities(
            "alpha",
            EntityFilter {
                ids: vec![id(1), id(2)],
                ..filtered
            },
            PageRequest {
                limit: 2,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(ids(&page.items), BTreeSet::from([id(1)]));
    assert_eq!(
        page.total,
        Some(1),
        "property-filtered IDs are not a complete ID lookup"
    );
    for path in [
        "$",
        "$.",
        "$.nested..label",
        "$[0]",
        "$.flag[0]",
        "$.flag' OR 1=1 --",
        "$.é",
    ] {
        let filter = EntityFilter::default().property_eq(path, SqlValue::Integer(1));
        let errors = [
            store
                .query_entities(
                    "alpha",
                    filter.clone(),
                    PageRequest {
                        limit: 1,
                        offset: 0,
                    },
                )
                .await
                .unwrap_err(),
            store
                .query_entities_count_free(
                    "alpha",
                    filter.clone(),
                    PageRequest {
                        limit: 1,
                        offset: 0,
                    },
                )
                .await
                .unwrap_err(),
            store
                .query_entities_after("alpha", filter.clone(), None, 1)
                .await
                .unwrap_err(),
            store.count_entities("alpha", filter).await.unwrap_err(),
        ];
        for error in errors {
            assert!(
                matches!(
                    error,
                    StorageError::InvalidInput {
                        capability: StorageCapability::Entities,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
}

fn event(n: u128, namespace: &str, outcome: EventOutcome, payload: serde_json::Value) -> Event {
    let mut row = Event::new(
        namespace,
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "actor:test",
    )
    .with_outcome(outcome)
    .with_payload(payload);
    row.id = id(n);
    row.created_at = n as i64;
    row
}

async fn assert_event_routes(
    store: &dyn khive_storage::EventStore,
    filter: EventFilter,
    expected: &[u128],
) {
    let expected: BTreeSet<_> = expected.iter().copied().map(id).collect();
    let page = store
        .query_events(
            filter.clone(),
            PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.iter().map(|row| row.id).collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(page.total, None);
    assert_eq!(
        store.count_events(filter).await.unwrap(),
        expected.len() as u64
    );
}

#[tokio::test]
async fn event_outcomes_and_payload_equalities_compose_with_existing_filters() {
    let backend = memory_backend();
    backend.prepare_core_schema().unwrap();
    let store = backend.events_for_namespace("alpha").unwrap();
    let proposal = id(999);
    for (n, outcome, flag, label) in [
        (1, EventOutcome::Success, json!(true), "O'Reilly"),
        (2, EventOutcome::Denied, json!(1), "other"),
        (3, EventOutcome::Error, json!(false), "other"),
        (4, EventOutcome::Success, json!("1"), "other"),
    ] {
        store.append_event(event(n, "alpha", outcome, json!({"result_kind":"note","flag":flag,"nested":{"label":label},"nil":null,"proposal_id":proposal.to_string()}))).await.unwrap();
    }
    backend
        .events_for_namespace("beta")
        .unwrap()
        .append_event(event(
            5,
            "beta",
            EventOutcome::Success,
            json!({"result_kind":"note","flag":true}),
        ))
        .await
        .unwrap();
    for (outcome, expected) in [
        (EventOutcome::Success, vec![1, 4]),
        (EventOutcome::Denied, vec![2]),
        (EventOutcome::Error, vec![3]),
    ] {
        assert_event_routes(
            store.as_ref(),
            EventFilter::default().outcome(outcome),
            &expected,
        )
        .await;
    }
    assert_event_routes(
        store.as_ref(),
        EventFilter::default().payload_eq("$.flag", SqlValue::Bool(true)),
        &[1, 2],
    )
    .await;
    assert_event_routes(
        store.as_ref(),
        EventFilter::default().payload_eq("$.flag", SqlValue::Float(1.0)),
        &[1, 2],
    )
    .await;
    assert_event_routes(
        store.as_ref(),
        EventFilter::default().payload_eq("$.flag", SqlValue::Text("1".into())),
        &[4],
    )
    .await;
    for path in ["$.nil", "$.missing"] {
        assert_event_routes(
            store.as_ref(),
            EventFilter::default().payload_eq(path, SqlValue::Null),
            &[],
        )
        .await;
    }
    let filter = EventFilter {
        kinds: vec![EventKind::SearchExecuted],
        verbs: vec!["search".into()],
        actors: vec!["actor:test".into()],
        after: Some(0),
        before: Some(10),
        payload_proposal_id: Some(proposal),
        ..Default::default()
    }
    .outcome(EventOutcome::Error)
    .outcome(EventOutcome::Success)
    .payload_eq("$.nested.label", SqlValue::Text("O'Reilly".into()))
    .payload_eq("$.flag", SqlValue::Integer(1));
    assert_event_routes(store.as_ref(), filter, &[1]).await;
    assert_event_routes(
        store.as_ref(),
        EventFilter::default().payload_eq("$.nested.label", SqlValue::Text("' OR 1=1 --".into())),
        &[],
    )
    .await;
    for path in [
        "$",
        "$.",
        "$.nested..label",
        "$[0]",
        "$.flag[0]",
        "$.flag' OR 1=1 --",
        "$.é",
    ] {
        let filter = EventFilter::default().payload_eq(path, SqlValue::Integer(1));
        for error in [
            store
                .query_events(
                    filter.clone(),
                    PageRequest {
                        limit: 1,
                        offset: 0,
                    },
                )
                .await
                .unwrap_err(),
            store.count_events(filter).await.unwrap_err(),
        ] {
            assert!(
                matches!(
                    error,
                    StorageError::InvalidInput {
                        capability: StorageCapability::Events,
                        ..
                    }
                ),
                "{error:?}"
            );
        }
    }
}

#[test]
fn old_serialized_filters_receive_new_field_defaults_and_builders_round_trip() {
    let mut entity = serde_json::to_value(EntityFilter::default()).unwrap();
    entity
        .as_object_mut()
        .unwrap()
        .remove("property_equalities");
    entity.as_object_mut().unwrap().remove("tombstones");
    let restored: EntityFilter = serde_json::from_value(entity).unwrap();
    assert!(restored.property_equalities.is_empty());
    assert_eq!(restored.tombstones, EntityTombstones::Live);
    let mut event = serde_json::to_value(EventFilter::default()).unwrap();
    event.as_object_mut().unwrap().remove("outcome");
    event.as_object_mut().unwrap().remove("payload_equalities");
    let restored: EventFilter = serde_json::from_value(event).unwrap();
    assert!(restored.outcome.is_none());
    assert!(restored.payload_equalities.is_empty());
    for value in [
        serde_json::to_value(
            EntityFilter::default()
                .tombstoned_only()
                .property_eq("$.flag", SqlValue::Bool(true)),
        )
        .unwrap(),
        serde_json::to_value(EntityFilter::default().include_tombstones()).unwrap(),
    ] {
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<EntityFilter>(value.clone()).unwrap())
                .unwrap(),
            value
        );
    }
    let value = serde_json::to_value(
        EventFilter::default()
            .outcome(EventOutcome::Denied)
            .payload_eq("$.flag", SqlValue::Integer(1)),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<EventFilter>(value.clone()).unwrap())
            .unwrap(),
        value
    );
}

fn schema_snapshot(backend: &StorageBackend) -> Vec<(String, String, Option<String>)> {
    let reader = backend.pool.reader().unwrap();
    let mut statement = reader
        .conn()
        .prepare("SELECT type, name, sql FROM sqlite_master ORDER BY type, name")
        .unwrap();
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}

#[tokio::test]
async fn namespace_enumeration_is_read_only_when_service_tables_are_absent() {
    let backend = memory_backend();
    let before = schema_snapshot(&backend);
    for mode in [NamespaceLiveness::default(), NamespaceLiveness::All] {
        assert!(backend
            .list_namespaces("scheduled_event", mode)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(schema_snapshot(&backend), before);
    }
    {
        let writer = backend.pool.writer().unwrap();
        writer
            .conn()
            .execute_batch("CREATE TABLE notes(namespace TEXT NOT NULL)")
            .unwrap();
    }
    let before = schema_snapshot(&backend);
    let error = backend
        .list_namespaces("scheduled_event", NamespaceLiveness::Live)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            StorageError::Driver {
                capability: StorageCapability::Sql,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(schema_snapshot(&backend), before);
}

#[tokio::test]
async fn namespaces_use_exact_stored_kind_and_explicit_liveness_across_existing_tables() {
    let backend = memory_backend();
    backend.prepare_core_schema().unwrap();
    let entities = backend.entities().unwrap();
    let notes = backend.notes().unwrap();
    for (n, namespace, kind, deleted) in [
        (1, "shared", "scheduled_event", false),
        (2, "z-deleted", "scheduled_event", true),
        (3, "not-a-match", "Scheduled_event", false),
        (4, "event-entity", "search_executed", false),
        (5, "quoted", "x' OR 1=1 --", false),
    ] {
        let mut row = entity(n, namespace, json!({}));
        row.kind = kind.into();
        entities.upsert_entity(row).await.unwrap();
        if deleted {
            assert!(entities
                .delete_entity(id(n), DeleteMode::Soft)
                .await
                .unwrap());
        }
    }
    for (n, namespace, kind, deleted, due) in [
        (11, "shared", "scheduled_event", false, 0),
        (12, "a-future", "scheduled_event", false, i64::MAX),
        (13, "b-deleted", "scheduled_event", true, 0),
        (14, "not-a-match", "task", false, 0),
        (15, "event-note", "search_executed", false, 0),
    ] {
        let mut row = Note::new(namespace, kind, "fixture").with_properties(json!({"due":due}));
        row.id = id(n);
        notes.upsert_note(row).await.unwrap();
        if deleted {
            assert!(notes.delete_note(id(n), DeleteMode::Soft).await.unwrap());
        }
    }
    backend
        .events_for_namespace("event-only")
        .unwrap()
        .append_event(event(
            21,
            "event-only",
            EventOutcome::Denied,
            json!({"result_kind":"note"}),
        ))
        .await
        .unwrap();
    backend
        .events_for_namespace("shared")
        .unwrap()
        .append_event(event(
            22,
            "shared",
            EventOutcome::Error,
            json!({"result_kind":"note"}),
        ))
        .await
        .unwrap();
    let before = schema_snapshot(&backend);
    assert_eq!(
        backend
            .list_namespaces("scheduled_event", NamespaceLiveness::default())
            .await
            .unwrap(),
        ["a-future", "shared"]
    );
    assert_eq!(
        backend
            .list_namespaces("scheduled_event", NamespaceLiveness::All)
            .await
            .unwrap(),
        ["a-future", "b-deleted", "shared", "z-deleted"]
    );
    for mode in [NamespaceLiveness::Live, NamespaceLiveness::All] {
        assert_eq!(
            backend
                .list_namespaces("search_executed", mode)
                .await
                .unwrap(),
            ["event-entity", "event-note", "event-only", "shared"]
        );
        assert!(backend
            .list_namespaces("entity", mode)
            .await
            .unwrap()
            .is_empty());
        assert!(backend.list_namespaces("", mode).await.unwrap().is_empty());
        assert_eq!(
            backend.list_namespaces("x' OR 1=1 --", mode).await.unwrap(),
            ["quoted"]
        );
    }
    assert_eq!(schema_snapshot(&backend), before);
}
