//! Create-only graph composition through the public runtime seam.

use khive_db::stores::graph::GraphMutationPreconditions;
use khive_runtime::{
    KhiveRuntime, LinkSpec, Namespace, NamespaceToken, RuntimeConfig, RuntimeError,
};
use khive_storage::{
    DeleteMode, Edge, EdgeInsertIfAbsentOutcome, EdgeRelation, Entity, Event, EventFilter,
    GraphStatementGuard, GraphStatementGuardFailure, GraphStatementGuardFailureReason, Note,
    PageRequest, SqlStatement, SqlValue, StorageCapability, StorageError,
};
use khive_types::EventKind;
use serde_json::json;
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn spec() -> LinkSpec {
    LinkSpec {
        namespace: None,
        source_id: id(2),
        target_id: id(1),
        relation: EdgeRelation::CompetesWith,
        weight: 0.75,
        metadata: Some(json!({"evidence": "preserved"})),
        resurrect: false,
    }
}

fn guard(label: &str, value: i64) -> GraphStatementGuard {
    GraphStatementGuard {
        label: label.into(),
        statement: SqlStatement {
            sql: "SELECT ?1".into(),
            params: vec![SqlValue::Integer(value)],
            label: Some("separate-sql-diagnostic".into()),
        },
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:guarded-insert".into()),
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        runtime.graph(&token).unwrap();
        runtime.events(&token).unwrap();
        for value in [1, 2] {
            let mut entity = Entity::new("local", "project", format!("project {value}"));
            entity.id = id(value);
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
        }
        Self { runtime, token }
    }

    async fn events(&self) -> Vec<Event> {
        self.runtime
            .events(&self.token)
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

    async fn edge(&self) -> Option<Edge> {
        self.runtime
            .get_edge_by_natural_key_including_deleted(
                &self.token,
                "local",
                id(1),
                id(2),
                EdgeRelation::CompetesWith,
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn create_only_link_emits_one_canonical_dynamic_event_and_preserves_winners() {
    let f = Fixture::new().await;
    let EdgeInsertIfAbsentOutcome::Created(edge) = f
        .runtime
        .link_if_absent_with_guards(&f.token, spec(), vec![guard("ready", 1)])
        .await
        .unwrap()
    else {
        panic!("absent edge must be created")
    };
    assert_eq!((edge.source_id, edge.target_id), (id(1), id(2)));
    let stored = f.edge().await.unwrap();
    assert_eq!(stored.id, edge.id);
    assert_eq!((stored.source_id, stored.target_id), (id(1), id(2)));
    assert_eq!(
        stored.created_at.timestamp_micros(),
        edge.created_at.timestamp_micros()
    );
    assert_eq!(
        stored.updated_at.timestamp_micros(),
        edge.updated_at.timestamp_micros()
    );
    assert_eq!(stored.weight, edge.weight);
    assert_eq!(stored.metadata, edge.metadata);
    let events = f.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::LinkCreated);
    assert_eq!(events[0].actor, "actor:test:guarded-insert");
    assert_eq!(events[0].namespace, "local");
    assert_eq!(events[0].target_id, Some(Uuid::from(edge.id)));
    assert_eq!(
        events[0].payload,
        json!({
            "id": Uuid::from(edge.id), "namespace": "local", "mutation": "created",
            "source_id": id(1), "target_id": id(2), "relation": "competes_with",
            "weight": 0.75, "metadata": {"evidence": "preserved"}, "previous": null,
            "source_kind": "entity", "target_kind": "entity"
        })
    );
    let mut replacement = spec();
    replacement.weight = 0.1;
    replacement.metadata = None;
    assert!(matches!(
        f.runtime
            .link_if_absent_with_guards(&f.token, replacement, vec![])
            .await
            .unwrap(),
        EdgeInsertIfAbsentOutcome::ExistingLive
    ));
    assert_eq!(
        serde_json::to_value(f.edge().await.unwrap()).unwrap(),
        serde_json::to_value(stored).unwrap()
    );
    assert!(f
        .runtime
        .graph(&f.token)
        .unwrap()
        .delete_edge(edge.id, DeleteMode::Soft)
        .await
        .unwrap());
    let tombstone = f.edge().await.unwrap();
    assert!(tombstone.deleted_at.is_some());
    assert!(matches!(
        f.runtime
            .link_if_absent_with_guards(&f.token, spec(), vec![])
            .await
            .unwrap(),
        EdgeInsertIfAbsentOutcome::Tombstoned
    ));
    assert_eq!(
        serde_json::to_value(f.edge().await.unwrap()).unwrap(),
        serde_json::to_value(tombstone).unwrap()
    );
    assert_eq!(f.events().await, events);
}

#[tokio::test]
async fn create_only_annotation_uses_actual_note_target_kind() {
    let f = Fixture::new().await;
    for value in [3, 4] {
        let mut note = Note::new("local", "observation", "annotation endpoint");
        note.id = id(value);
        f.runtime
            .notes(&f.token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
    }
    let mut annotation = spec();
    annotation.source_id = id(3);
    annotation.target_id = id(4);
    annotation.relation = EdgeRelation::Annotates;
    assert!(matches!(
        f.runtime
            .link_if_absent_with_guards(&f.token, annotation, vec![])
            .await
            .unwrap(),
        EdgeInsertIfAbsentOutcome::Created(_)
    ));
    let events = f.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].payload["source_kind"], "note");
    assert_eq!(events[0].payload["target_kind"], "note");
}

#[tokio::test]
async fn refused_guard_is_typed_and_guard_only_batch_does_not_short_circuit() {
    let f = Fixture::new().await;
    let failure = GraphStatementGuardFailure {
        label: "caller-predicate".into(),
        reason: GraphStatementGuardFailureReason::PredicateFalse,
    };
    let EdgeInsertIfAbsentOutcome::GuardFailed(actual) = f
        .runtime
        .link_if_absent_with_guards(&f.token, spec(), vec![guard("caller-predicate", 0)])
        .await
        .unwrap()
    else {
        panic!("false guard must refuse insertion")
    };
    assert_eq!(actual, failure);
    assert!(f.edge().await.is_none());
    assert!(f.events().await.is_empty());
    let error = f
        .runtime
        .link_many_guarded_observed(
            &f.token,
            vec![],
            GraphMutationPreconditions {
                statement_guards: vec![guard("caller-predicate", 0)],
                ..Default::default()
            },
            vec![],
        )
        .await
        .expect_err("guards must run even without edges or retirements");
    let RuntimeError::Storage(StorageError::Conflict {
        capability,
        operation,
        message,
    }) = error
    else {
        panic!("typed conflict required: {error:?}")
    };
    assert_eq!(capability, StorageCapability::Graph);
    assert_eq!(operation, "link_many_guarded_observed");
    assert_eq!(message, failure.to_string());
    assert!(f.events().await.is_empty());
}

#[tokio::test]
async fn create_only_link_rejects_resurrection_weight_namespace_and_endpoint_errors() {
    let f = Fixture::new().await;
    let mut invalid = spec();
    invalid.resurrect = true;
    assert!(matches!(
        f.runtime
            .link_if_absent_with_guards(&f.token, invalid, vec![])
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
    for weight in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let mut invalid = spec();
        invalid.weight = weight;
        assert!(matches!(
            f.runtime
                .link_if_absent_with_guards(&f.token, invalid, vec![])
                .await,
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    let mut invalid = spec();
    invalid.namespace = Some("other".into());
    assert!(matches!(
        f.runtime
            .link_if_absent_with_guards(&f.token, invalid, vec![])
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
    let mut invalid = spec();
    invalid.relation = EdgeRelation::Annotates;
    assert!(matches!(
        f.runtime
            .link_if_absent_with_guards(&f.token, invalid, vec![])
            .await,
        Err(RuntimeError::InvalidInput(_))
    ));
    assert!(f.edge().await.is_none());
    assert!(f.events().await.is_empty());
}

#[tokio::test]
async fn create_only_event_failure_rolls_back_the_edge() {
    let f = Fixture::new().await;
    f.runtime.sql().writer().await.unwrap().execute_script(
        "CREATE TRIGGER guarded_insert_event_failure BEFORE INSERT ON events WHEN NEW.kind='link_created' BEGIN SELECT RAISE(ABORT,'guarded-insert-event-failure'); END;".into(),
    ).await.unwrap();
    let error = f
        .runtime
        .link_if_absent_with_guards(&f.token, spec(), vec![guard("ready", 1)])
        .await
        .expect_err("event append must abort edge creation");
    assert!(matches!(error, RuntimeError::Storage(_)), "{error}");
    assert!(error.to_string().contains("guarded-insert-event-failure"));
    assert!(f.edge().await.is_none());
    assert!(f.events().await.is_empty());
}
