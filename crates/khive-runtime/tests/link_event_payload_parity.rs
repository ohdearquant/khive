//! Guarded composition and unchanged lifecycle payloads through real runtime dispatch.
use chrono::{TimeZone, Utc};
use khive_db::stores::graph::{
    GraphDocumentGuard, GraphEdgeSnapshotGuard, GraphMutationPreconditions,
};
use khive_runtime::{
    KhiveRuntime, LinkSpec, Namespace, NamespaceToken, RuntimeConfig, RuntimeError,
};
use khive_storage::{
    Edge, EdgeRelation, EdgeUpsertDisposition, Entity, Event, EventFilter, LinkId, PageRequest,
    SqlStatement, SqlValue, StorageCapability, StorageError, WriterTaskRequestState,
};
use khive_types::{EventKind, OperationAttribution, RefResolution};
use serde_json::{json, Value};
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
const BODY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
}
impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:payload-parity".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("complete runtime schema for payload parity fixture");
        let token = runtime.authorize(Namespace::local()).unwrap();
        runtime.graph(&token).unwrap();
        runtime.events(&token).unwrap();
        for n in 1..=3 {
            let mut entity = Entity::new("local", "document", format!("endpoint {n}"));
            entity.id = id(n);
            if n == 1 {
                entity.properties = Some(json!({"blob_ref":BODY}));
            }
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
        }
        Self { runtime, token }
    }
    async fn seed(&self) -> Edge {
        let time = Utc.timestamp_micros(1_000_000).single().unwrap();
        let edge = Edge {
            id: LinkId::from(id(50)),
            namespace: "local".into(),
            source_id: id(1),
            target_id: id(2),
            relation: EdgeRelation::LinksTo,
            weight: 0.25,
            created_at: time,
            updated_at: time,
            deleted_at: None,
            metadata: Some(json!({"web_extract":true,"nested":{"array":[1,"two",null]}})),
            target_backend: Some("legacy-source-stamp".into()),
        };
        self.runtime
            .graph(&self.token)
            .unwrap()
            .upsert_edge(edge.clone())
            .await
            .unwrap();
        let stored = self.edge(id(2)).await.unwrap();
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            serde_json::to_value(edge).unwrap()
        );
        stored
    }
    async fn edge(&self, target: Uuid) -> Option<Edge> {
        self.runtime
            .get_edge_by_natural_key_including_deleted(
                &self.token,
                "local",
                id(1),
                target,
                EdgeRelation::LinksTo,
            )
            .await
            .unwrap()
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
    async fn rows(&self, sql: &str, params: Vec<SqlValue>) -> Value {
        serde_json::to_value(
            self.runtime
                .sql()
                .reader()
                .await
                .unwrap()
                .query_all(SqlStatement {
                    sql: sql.into(),
                    params,
                    label: Some("guarded-link-fixture".into()),
                })
                .await
                .unwrap(),
        )
        .unwrap()
    }
    async fn snapshot(&self) -> Value {
        json!({"edges":self.rows("SELECT * FROM graph_edges ORDER BY id",vec![]).await,"ledger":self.rows("SELECT * FROM graph_edges_seq ORDER BY seq",vec![]).await,"sequence":self.rows("SELECT * FROM sqlite_sequence WHERE name='graph_edges_seq'",vec![]).await,"events":self.rows("SELECT * FROM events ORDER BY id",vec![]).await,"projection":self.rows("SELECT * FROM event_observations ORDER BY event_id,role,position",vec![]).await})
    }
    async fn script(&self, sql: &str) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(sql.to_owned())
            .await
            .unwrap();
    }
    async fn stored_payload(&self, event: Uuid) -> String {
        let row = self
            .runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_row(SqlStatement {
                sql: "SELECT payload FROM events WHERE id=?1".into(),
                params: vec![SqlValue::Text(event.to_string())],
                label: None,
            })
            .await
            .unwrap()
            .unwrap();
        match row.get("payload") {
            Some(SqlValue::Text(value)) => value.clone(),
            other => panic!("stored payload must be exact text: {other:?}"),
        }
    }
    async fn projection(&self, event: Uuid) -> Vec<(Uuid, String, String, i64)> {
        let rows = self
            .runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement {
                sql: "SELECT * FROM event_observations WHERE event_id=?1 ORDER BY role,position"
                    .into(),
                params: vec![SqlValue::Text(event.to_string())],
                label: None,
            })
            .await
            .unwrap();
        rows.into_iter().map(|row| {
            assert!(matches!(row.get("event_id"),Some(SqlValue::Text(value)) if value==&event.to_string()));
            let text=|name|match row.get(name){Some(SqlValue::Text(value))=>value.clone(),other=>panic!("invalid {name}: {other:?}")};
            let position=match row.get("position"){Some(SqlValue::Integer(value))=>*value,other=>panic!("invalid position {other:?}")};
            (text("entity_id").parse().unwrap(),text("referent_kind"),text("role"),position)
        }).collect()
    }
}
fn spec(target: Uuid) -> LinkSpec {
    LinkSpec {
        namespace: None,
        source_id: id(1),
        target_id: target,
        relation: EdgeRelation::LinksTo,
        weight: 0.75,
        metadata: Some(json!({"web_extract":true,"nested":{"replacement":[true,3]}})),
        resurrect: true,
    }
}
fn guards(target: Uuid, expected: Option<Edge>) -> GraphMutationPreconditions {
    GraphMutationPreconditions {
        document: Some(GraphDocumentGuard {
            namespace: "local".into(),
            id: id(1),
            expected_blob_ref: BODY.into(),
        }),
        edges: vec![GraphEdgeSnapshotGuard {
            namespace: "local".into(),
            source_id: id(1),
            target_id: target,
            relation: EdgeRelation::LinksTo,
            expected,
        }],
    }
}
fn assert_conflict(error: RuntimeError, message: &str) {
    let RuntimeError::Storage(mut error) = error else {
        panic!("typed Storage conflict required: {error:?}")
    };
    loop {
        match error {
            StorageError::WriterTaskRequestFailed {
                request_state,
                source,
            } => {
                assert_eq!(request_state, WriterTaskRequestState::TransactionRolledBack);
                error = *source;
            }
            StorageError::Conflict {
                capability,
                message: actual,
                ..
            } => {
                assert_eq!(capability, StorageCapability::Graph);
                assert_eq!(actual, message);
                break;
            }
            other => panic!("typed Graph conflict source required: {other:?}"),
        }
    }
}
fn comparable_envelope(event: &Event) -> Value {
    assert_eq!(event.id.get_version_num(), 4);
    assert!(event.created_at > 1_000_000);
    assert!(Utc.timestamp_micros(event.created_at).single().is_some());
    let mut value = serde_json::to_value(event).unwrap();
    let fields = value.as_object_mut().unwrap();
    // Each generated UUID/time is validated above; every other envelope field,
    // including the complete untouched payload, is compared below.
    fields.remove("id");
    fields.remove("created_at");
    value
}

#[tokio::test]
async fn guarded_replacement_payload_matches_existing_link_and_complete_parent_shape() {
    let old = Fixture::new().await;
    let guarded = Fixture::new().await;
    let old_seed = old.seed().await;
    let guarded_seed = guarded.seed().await;
    assert_eq!(
        serde_json::to_value(&old_seed).unwrap(),
        serde_json::to_value(&guarded_seed).unwrap()
    );
    let operation = OperationAttribution {
        op_index: 7,
        ref_resolution: RefResolution::Resolved,
    };
    let old_result = khive_storage::operation_context::scope_operation_attribution(
        operation,
        old.runtime.link_observed(
            &old.token,
            id(1),
            id(2),
            EdgeRelation::LinksTo,
            0.75,
            spec(id(2)).metadata,
            false,
        ),
    )
    .await
    .unwrap();
    let (rows, retired) = khive_storage::operation_context::scope_operation_attribution(
        operation,
        guarded.runtime.link_many_guarded_observed(
            &guarded.token,
            vec![spec(id(2))],
            guards(id(2), Some(guarded_seed)),
            vec![],
        ),
    )
    .await
    .unwrap();
    assert!(retired.is_empty());
    assert_eq!(rows.len(), 1);
    let guarded_result = &rows[0];
    for result in [&old_result, guarded_result] {
        assert_eq!(result.disposition, EdgeUpsertDisposition::Updated);
        assert_eq!(result.edge.id, LinkId::from(id(50)));
        assert_eq!(result.edge.created_at, old_seed.created_at);
        assert_eq!(
            serde_json::to_vec(result.previous.as_ref().unwrap()).unwrap(),
            serde_json::to_vec(&old_seed).unwrap()
        );
    }
    let old_events = old.events().await;
    let guarded_events = guarded.events().await;
    assert_eq!(old_events.len(), 1);
    assert_eq!(guarded_events.len(), 1);
    let a = &old_events[0];
    let b = &guarded_events[0];
    let previous = json!({"id":id(50),"namespace":"local","source_id":id(1),"target_id":id(2),"relation":"links_to","weight":0.25,"created_at":"1970-01-01T00:00:01Z","updated_at":"1970-01-01T00:00:01Z","deleted_at":null,"metadata":{"web_extract":true,"nested":{"array":[1,"two",null]}},"target_backend":"legacy-source-stamp"});
    let expected = json!({"id":id(50),"namespace":"local","mutation":"updated","source_id":id(1),"target_id":id(2),"relation":"links_to","weight":0.75,"metadata":{"web_extract":true,"nested":{"replacement":[true,3]}},"previous":previous});
    for event in [a, b] {
        assert_eq!(event.kind, EventKind::EdgeUpdated);
        assert_eq!(
            event.payload, expected,
            "complete parent payload and previous shape must remain exact"
        );
        assert_eq!(
            event.actor,
            format!("{}:{}", old.token.actor().kind, old.token.actor().id)
        );
        assert_eq!(event.op_index, Some(7));
        assert_eq!(event.ref_resolution, Some(RefResolution::Resolved));
    }
    assert_eq!(
        serde_json::to_vec(&a.payload).unwrap(),
        serde_json::to_vec(&b.payload).unwrap()
    );
    assert_eq!(
        old.stored_payload(a.id).await.as_bytes(),
        guarded.stored_payload(b.id).await.as_bytes()
    );
    assert_eq!(comparable_envelope(a), comparable_envelope(b));
    let projections = old.projection(a.id).await;
    assert_eq!(
        projections,
        vec![(id(50), "edge".into(), "target".into(), 0)]
    );
    assert_eq!(projections, guarded.projection(b.id).await);
}

#[tokio::test]
async fn guarded_link_refuses_replaced_body_before_any_edge_unit_write() {
    let f = Fixture::new().await;
    let seed = f.seed().await;
    let (rows, _) = f
        .runtime
        .link_many_guarded_observed(
            &f.token,
            vec![spec(id(2))],
            guards(id(2), Some(seed)),
            vec![],
        )
        .await
        .expect("valid guarded replacement positive control");
    assert_eq!(rows[0].disposition, EdgeUpsertDisposition::Updated);
    let current = f.edge(id(2)).await.unwrap();
    let mut document = f
        .runtime
        .entities(&f.token)
        .unwrap()
        .get_entity(id(1))
        .await
        .unwrap()
        .unwrap();
    document.properties = Some(json!({"blob_ref":"22".repeat(32)}));
    f.runtime
        .entities(&f.token)
        .unwrap()
        .upsert_entity(document)
        .await
        .unwrap();
    let before = f.snapshot().await;
    let error = f
        .runtime
        .link_many_guarded_observed(
            &f.token,
            vec![spec(id(2))],
            guards(id(2), Some(current)),
            vec![],
        )
        .await
        .unwrap_err();
    assert_conflict(error, "source document body reference changed");
    assert_eq!(
        f.snapshot().await,
        before,
        "a stale body must write no edge, ledger, event or projection unit"
    );
}

#[tokio::test]
async fn guarded_link_refuses_unmarked_winner_and_changed_full_ownership_snapshot() {
    for expected_absence in [true, false] {
        let f = Fixture::new().await;
        let seed = f.seed().await;
        let (rows, _) = f
            .runtime
            .link_many_guarded_observed(
                &f.token,
                vec![spec(id(2))],
                guards(id(2), Some(seed)),
                vec![],
            )
            .await
            .expect("valid ownership guard positive control");
        assert_eq!(rows.len(), 1);
        let time = Utc.timestamp_micros(2_000_000).single().unwrap();
        let mut caller = Edge {
            id: LinkId::from(id(60)),
            namespace: "local".into(),
            source_id: id(1),
            target_id: id(3),
            relation: EdgeRelation::LinksTo,
            weight: 0.5,
            created_at: time,
            updated_at: time,
            deleted_at: None,
            metadata: Some(json!({"caller_owned":true,"nested":{"value":1}})),
            target_backend: Some("caller-placement".into()),
        };
        let expected = if expected_absence {
            assert!(f.edge(id(3)).await.is_none());
            None
        } else {
            f.runtime
                .graph(&f.token)
                .unwrap()
                .upsert_edge(caller.clone())
                .await
                .unwrap();
            Some(f.edge(id(3)).await.unwrap())
        };
        caller.metadata = Some(json!({"caller_owned":true,"nested":{"value":2}}));
        f.runtime
            .graph(&f.token)
            .unwrap()
            .upsert_edge(caller)
            .await
            .unwrap();
        let winner = f.edge(id(3)).await.unwrap();
        let before = f.snapshot().await;
        let error = f
            .runtime
            .link_many_guarded_observed(
                &f.token,
                vec![spec(id(3))],
                guards(id(3), expected),
                vec![],
            )
            .await
            .unwrap_err();
        assert_conflict(error, "edge ownership snapshot changed");
        assert_eq!(f.snapshot().await, before);
        assert_eq!(
            serde_json::to_value(f.edge(id(3)).await.unwrap()).unwrap(),
            serde_json::to_value(winner).unwrap(),
            "unmarked caller row must not be overwritten or claimed"
        );
    }
}

#[tokio::test]
async fn guarded_reconciliation_rolls_back_upsert_and_retirement_then_retries_once() {
    let f = Fixture::new().await;
    let retire = f.seed().await;
    let before = f.snapshot().await;
    f.script("CREATE TRIGGER c9_retirement_fault BEFORE INSERT ON events WHEN NEW.kind='edge_deleted' AND EXISTS(SELECT 1 FROM graph_edges WHERE target_id='00000000-0000-0000-0000-000000000002' AND deleted_at IS NOT NULL) AND EXISTS(SELECT 1 FROM graph_edges WHERE target_id='00000000-0000-0000-0000-000000000003' AND deleted_at IS NULL) BEGIN SELECT RAISE(ABORT,'c9-retirement-event'); END;").await;
    let error = f
        .runtime
        .link_many_guarded_observed(
            &f.token,
            vec![spec(id(3))],
            guards(id(3), None),
            vec![retire.clone()],
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("c9-retirement-event"),
        "must fail after both DML operations: {error:?}"
    );
    assert_eq!(
        f.snapshot().await,
        before,
        "upsert and retraction are one event transaction"
    );
    f.script("DROP TRIGGER c9_retirement_fault").await;
    let (rows, retired) = f
        .runtime
        .link_many_guarded_observed(
            &f.token,
            vec![spec(id(3))],
            guards(id(3), None),
            vec![retire],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].disposition, EdgeUpsertDisposition::Created);
    assert_eq!(retired, vec![LinkId::from(id(50))]);
    assert!(f.edge(id(2)).await.unwrap().deleted_at.is_some());
    let events = f.events().await;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::LinkCreated)
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::EdgeDeleted)
            .count(),
        1
    );
    let before = f.snapshot().await;
    let mut stale = f.edge(id(3)).await.unwrap();
    stale.metadata = Some(json!({"foreign_owner":true}));
    let error = f
        .runtime
        .link_many_guarded_observed(
            &f.token,
            vec![],
            GraphMutationPreconditions::default(),
            vec![stale],
        )
        .await
        .unwrap_err();
    assert_conflict(error, "edge retirement snapshot changed");
    assert_eq!(
        f.snapshot().await,
        before,
        "retirement-only batches must still check the complete snapshot"
    );
}
