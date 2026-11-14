//! Entity-only supersession is checked before immutable proposal history.

use khive_runtime::{
    KhiveRuntime, Namespace, RequestIdentity, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{Entity, Event, EventFilter, LinkId, Note, PageRequest, SqlStatement};
use khive_types::{
    ApplyResult, EdgeRelation, EventKind, ProposalAppliedPayload, ProposalChangeset,
};
use serde_json::{json, Value};
use uuid::Uuid;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".into()],
            brain_profile: None,
            actor_id: Some("entity-proposer".into()),
            events_split: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("entity-proposer".into()));
        builder.register(crate::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry.dispatch(verb, params).await.unwrap()
    }

    async fn entity(&self, name: &str, namespace: &str) -> Entity {
        let response = self
            .call(
                "create",
                json!({
                    "kind": "concept", "name": name, "namespace": namespace,
                    "description": "Persisted entity precondition fixture",
                    "properties": {"retained": name}, "skip_dedup_check": true
                }),
            )
            .await;
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let entity = self
            .runtime
            .get_entity(&token, full_id(&response))
            .await
            .unwrap();
        assert_eq!(entity.namespace, namespace);
        assert!(entity.deleted_at.is_none());
        entity
    }

    async fn snapshot(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut state = serde_json::Map::new();
        for (name, sql) in [
            ("entities", "SELECT * FROM entities ORDER BY id"),
            ("notes", "SELECT * FROM notes ORDER BY id"),
            ("edges", "SELECT * FROM graph_edges ORDER BY id"),
            (
                "entity_fts",
                "SELECT rowid, * FROM fts_entities ORDER BY rowid",
            ),
            ("note_fts", "SELECT rowid, * FROM fts_notes ORDER BY rowid"),
            (
                "observations",
                "SELECT * FROM event_observations ORDER BY event_id, role, position",
            ),
            (
                "proposals",
                "SELECT * FROM proposals_open ORDER BY proposal_id",
            ),
            // Generic gate audit is permitted; proposal/domain history cannot change.
            (
                "events",
                "SELECT * FROM events WHERE kind != 'audit' ORDER BY id",
            ),
        ] {
            let rows = reader
                .query_all(SqlStatement::new(sql, vec![]))
                .await
                .unwrap();
            state.insert(name.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(state)
    }

    async fn events(&self, filter: EventFilter) -> Vec<Event> {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let store = self.runtime.events(&token).unwrap();
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
            store.count_events(filter).await.unwrap(),
            page.items.len() as u64
        );
        page.items
    }

    async fn populated(&self) -> (Uuid, Uuid, Vec<(&'static str, Uuid)>) {
        let old = self.entity("Existing old", "local").await;
        let new = self.entity("Existing new", "local").await;
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let ordinary = self.call("create", json!({
            "kind": "observation", "content": "Persisted ordinary note", "skip_dedup_check": true
        })).await;
        let ordinary_id = full_id(&ordinary);
        let ordinary = self
            .runtime
            .notes(&token)
            .unwrap()
            .get_note(ordinary_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ordinary.kind, "observation");
        assert_eq!(ordinary.content, "Persisted ordinary note");

        // A typed storage fixture supplies the foreign pack's note substrate only.
        let memory = Note::new("local", "memory", "Persisted memory note");
        self.runtime
            .notes(&token)
            .unwrap()
            .upsert_note(memory.clone())
            .await
            .unwrap();
        let stored = self
            .runtime
            .notes(&token)
            .unwrap()
            .get_note(memory.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(stored).unwrap(),
            serde_json::to_value(&memory).unwrap()
        );

        let linked = self
            .call(
                "link",
                json!({
                    "source_id": old.id, "target_id": new.id, "relation": "contains"
                }),
            )
            .await;
        let edge_id = full_id(&linked);
        let edge = self
            .runtime
            .graph(&token)
            .unwrap()
            .get_edge(LinkId(edge_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edge.source_id, old.id);
        assert_eq!(edge.target_id, new.id);
        assert_eq!(edge.relation, EdgeRelation::Contains);
        assert!(edge.deleted_at.is_none());

        let created = self
            .events(EventFilter {
                target_id: Some(old.id),
                kinds: vec![EventKind::EntityCreated],
                ..Default::default()
            })
            .await;
        assert_eq!(created.len(), 1);
        let event_id = created[0].id;
        assert_eq!(
            self.runtime
                .events(&token)
                .unwrap()
                .get_event(event_id)
                .await
                .unwrap()
                .unwrap(),
            created[0]
        );

        let deleted = self.entity("Deleted entity", "local").await;
        self.call("delete", json!({"id": deleted.id})).await;
        assert!(matches!(
            self.runtime.get_entity(&token, deleted.id).await,
            Err(RuntimeError::NotFound(_))
        ));
        assert!(self
            .runtime
            .get_entity_including_deleted(&token, deleted.id)
            .await
            .unwrap()
            .unwrap()
            .deleted_at
            .is_some());
        let missing = Uuid::new_v4();
        assert!(self
            .runtime
            .get_entity_including_deleted(&token, missing)
            .await
            .unwrap()
            .is_none());

        self.call("propose", proposal(json!({
            "kind": "add_entity", "entity": {"kind": "concept", "name": "Unapplied history seed"}
        }))).await;
        let state = self.snapshot().await;
        for name in [
            "entities",
            "notes",
            "edges",
            "entity_fts",
            "note_fts",
            "proposals",
            "events",
        ] {
            assert!(
                !state[name].as_array().unwrap().is_empty(),
                "nonempty {name} witness"
            );
        }
        let bad = vec![
            ("missing", missing),
            ("ordinary note", ordinary_id),
            ("memory note", memory.id),
            ("edge", edge_id),
            ("event", event_id),
            ("deleted entity", deleted.id),
        ];
        for (label, id) in &bad {
            assert!(
                matches!(
                    self.runtime.get_entity(&token, *id).await,
                    Err(RuntimeError::NotFound(_))
                ),
                "{label} must not be a live entity"
            );
        }
        (old.id, new.id, bad)
    }
}

fn full_id(response: &Value) -> Uuid {
    let raw = response["id"].as_str().unwrap();
    let id = Uuid::parse_str(raw).expect("actual response has a full UUID");
    assert_eq!(raw, id.to_string());
    id
}

fn proposal(changeset: Value) -> Value {
    json!({"title": "Entity supersession", "description": "Validate both live entity operands", "changeset": changeset})
}

fn wrap(mut step: Value, depth: usize) -> Value {
    for _ in 0..depth {
        step = json!({"kind": "compound", "steps": [step]});
    }
    step
}

fn supersede(old: Uuid, new: Uuid) -> Value {
    json!({"kind": "supersede_entity", "old": old, "new": new})
}

fn assert_not_live(error: RuntimeError, id: Uuid) {
    let RuntimeError::NotFound(message) = error else {
        panic!("expected typed NotFound for {id}, got {error:?}");
    };
    assert_eq!(
        message,
        format!("{id} does not name a live entity: entity {id}")
    );
}

#[tokio::test]
async fn propose_supersede_entity_requires_two_live_entities() {
    let f = Fixture::new();
    let (old, new, bad) = f.populated().await;
    let before = f.snapshot().await;
    let ordinary_note = bad
        .iter()
        .find(|(label, _)| *label == "ordinary note")
        .unwrap()
        .1;
    let memory_note = bad
        .iter()
        .find(|(label, _)| *label == "memory note")
        .unwrap()
        .1;
    for (label, invalid) in bad {
        for old_operand in [true, false] {
            for depth in 0..=2 {
                let step = if old_operand {
                    supersede(invalid, new)
                } else {
                    supersede(old, invalid)
                };
                let error = f
                    .registry
                    .dispatch("propose", proposal(wrap(step, depth)))
                    .await
                    .expect_err(
                        "wrong-substrate or absent entity must refuse before proposal persistence",
                    );
                assert_not_live(error, invalid);
                assert_eq!(
                    f.snapshot().await,
                    before,
                    "{label}, old={old_operand}, depth={depth}"
                );
            }
        }
    }
    let error = f
        .registry
        .dispatch(
            "propose",
            proposal(wrap(supersede(ordinary_note, memory_note), 2)),
        )
        .await
        .expect_err("old is checked before new when both are invalid");
    assert_not_live(error, ordinary_note);
    assert_eq!(f.snapshot().await, before);
    for (first, second) in [(ordinary_note, memory_note), (memory_note, ordinary_note)] {
        let compound = json!({"kind": "compound", "steps": [
            wrap(supersede(first, new), 1), wrap(supersede(old, second), 1)
        ]});
        let error = f
            .registry
            .dispatch("propose", proposal(compound))
            .await
            .expect_err("invalid siblings are visited in submitted order");
        assert_not_live(error, first);
        assert_eq!(f.snapshot().await, before);
    }
}

#[tokio::test]
async fn propose_supersede_entity_accepts_live_entities_and_preserves_direction() {
    let f = Fixture::new();
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    for depth in 0..=2 {
        // The proposal stays local; both IDs may belong to other namespaces.
        let (old_namespace, new_namespace) = if depth == 0 {
            ("local", "local")
        } else {
            ("old-space", "new-space")
        };
        let old = f.entity(&format!("Old {depth}"), old_namespace).await;
        let new = f.entity(&format!("New {depth}"), new_namespace).await;
        let changeset = wrap(supersede(old.id, new.id), depth);
        let proposed = f.call("propose", proposal(changeset.clone())).await;
        assert_eq!(proposed["status"], "open");
        let proposal_id = full_id(&proposed);
        assert!(f
            .runtime
            .graph(&token)
            .unwrap()
            .get_edge_by_natural_key_including_deleted(
                "local",
                new.id,
                old.id,
                EdgeRelation::Supersedes
            )
            .await
            .unwrap()
            .is_none());
        let reviewed = f
            .registry
            .dispatch_with_identity(
                "review",
                json!({"id": proposal_id, "decision": "approve"}),
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: Some("entity-reviewer".into()),
                    visible_namespaces: vec![],
                    owned_namespaces: vec![],
                    process_ref: None,
                    request_id: None,
                }),
            )
            .await
            .expect("a different actor approves and applies the proposal");
        assert_eq!(reviewed["decision"], "approve");
        let projection =
            crate::projection_worker::ProposalsProjectionWorker::new(f.runtime.clone())
                .get_proposal_row(&token, proposal_id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(projection.status, "applied");
        let graph = f.runtime.graph(&token).unwrap();
        let edge = graph
            .get_edge_by_natural_key_including_deleted(
                "local",
                new.id,
                old.id,
                EdgeRelation::Supersedes,
            )
            .await
            .unwrap()
            .expect("actual new-to-old edge");
        assert_eq!(edge.source_id, new.id);
        assert_eq!(edge.target_id, old.id);
        assert_eq!(edge.relation, EdgeRelation::Supersedes);
        assert!(edge.deleted_at.is_none());
        assert!(graph
            .get_edge_by_natural_key_including_deleted(
                "local",
                old.id,
                new.id,
                EdgeRelation::Supersedes
            )
            .await
            .unwrap()
            .is_none());
        for original in [&old, &new] {
            assert_eq!(
                serde_json::to_value(f.runtime.get_entity(&token, original.id).await.unwrap())
                    .unwrap(),
                serde_json::to_value(original).unwrap()
            );
        }
        let events = f
            .events(EventFilter {
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            })
            .await;
        for (kind, verb) in [
            (EventKind::ProposalCreated, "propose"),
            (EventKind::ProposalReviewed, "review"),
            (EventKind::ProposalApplied, "propose-apply"),
        ] {
            let matching: Vec<_> = events.iter().filter(|event| event.kind == kind).collect();
            assert_eq!(matching.len(), 1, "one {kind:?} record");
            let event = matching[0];
            assert_eq!(event.verb, verb);
            assert_eq!(event.namespace, "local");
            assert_eq!(event.aggregate_kind.as_deref(), Some("proposal"));
            assert_eq!(event.aggregate_id, Some(proposal_id));
            assert_eq!(event.payload["proposal_id"], proposal_id.to_string());
            if kind == EventKind::ProposalCreated {
                let typed: ProposalChangeset = serde_json::from_value(changeset.clone()).unwrap();
                assert_eq!(
                    event.payload["changeset"],
                    serde_json::to_value(typed).unwrap()
                );
            } else if kind == EventKind::ProposalReviewed {
                assert_eq!(event.payload["reviewer"], "entity-reviewer");
                assert_eq!(event.payload["decision"], "approve");
            } else {
                let applied: ProposalAppliedPayload =
                    serde_json::from_value(event.payload.clone()).unwrap();
                assert!(matches!(applied.result, ApplyResult::Success { .. }));
                assert_eq!(applied.proposal_id.to_u128(), proposal_id.as_u128());
                assert_eq!(applied.applied_by, "system:propose-apply");
            }
        }
    }
}

#[tokio::test]
async fn propose_supersede_entity_valid_multi_step_keeps_existing_containment() {
    let f = Fixture::new();
    let (old, new, bad) = f.populated().await;
    let before = f.snapshot().await;
    let valid = supersede(old, new);
    for depth in 0..=1 {
        let compound = wrap(
            json!({"kind": "compound", "steps": [valid.clone(), valid.clone()]}),
            depth,
        );
        let error = f
            .registry
            .dispatch("propose", proposal(compound))
            .await
            .expect_err("valid multi-step remains unsupported");
        let RuntimeError::InvalidInput(message) = error else {
            panic!("expected existing containment error, got {error:?}");
        };
        assert_eq!(message, "multi-step Compound proposals are not supported until atomic proposal apply is available");
        assert_eq!(f.snapshot().await, before);
    }
    let missing = bad.iter().find(|(label, _)| *label == "missing").unwrap().1;
    for invalid in [supersede(missing, new), supersede(old, missing)] {
        let compound = json!({"kind": "compound", "steps": [valid.clone(), wrap(invalid, 2)]});
        let error = f
            .registry
            .dispatch("propose", proposal(compound))
            .await
            .expect_err("nested invalid operand precedes containment refusal");
        assert_not_live(error, missing);
        assert_eq!(f.snapshot().await, before);
    }
}

#[tokio::test]
async fn propose_supersede_entity_preserves_merged_operand_redirects() {
    let f = Fixture::new();
    f.populated().await;
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    for old_operand in [true, false] {
        for depth in [0, 2] {
            let old = f.entity("Old before merge", "local").await;
            let new = f.entity("New before merge", "local").await;
            let kept = f.entity("Kept merge target", "local").await;
            let consumed = if old_operand { old.id } else { new.id };
            f.call(
                "merge",
                json!({"kind": "entity", "into_id": kept.id, "from_id": consumed}),
            )
            .await;
            let tombstone = f
                .runtime
                .get_entity_including_deleted(&token, consumed)
                .await
                .unwrap()
                .unwrap();
            assert!(tombstone.deleted_at.is_some());
            assert_eq!(tombstone.merged_into, Some(kept.id));
            let kept_after_merge = f.runtime.get_entity(&token, kept.id).await.unwrap();
            assert!(kept_after_merge.deleted_at.is_none());
            let redirect = format!("{consumed} was merged into {}; query the kept id", kept.id);
            let runtime_error = f.runtime.get_entity(&token, consumed).await.unwrap_err();
            assert!(
                matches!(runtime_error, RuntimeError::NotFound(ref detail) if detail == &redirect)
            );

            let before = f.snapshot().await;
            let error = f
                .registry
                .dispatch("propose", proposal(wrap(supersede(old.id, new.id), depth)))
                .await
                .expect_err("a merged operand must refuse without following its redirect");
            let RuntimeError::NotFound(message) = error else {
                panic!("expected typed NotFound for {consumed}, got {error:?}");
            };
            assert_eq!(
                message,
                format!("{consumed} does not name a live entity: {redirect}")
            );
            assert_eq!(f.snapshot().await, before);
            assert_eq!(
                serde_json::to_value(f.runtime.get_entity(&token, kept.id).await.unwrap()).unwrap(),
                serde_json::to_value(kept_after_merge).unwrap()
            );
            let redirected_pair = if old_operand {
                (new.id, kept.id)
            } else {
                (kept.id, old.id)
            };
            assert!(f
                .runtime
                .graph(&token)
                .unwrap()
                .get_edge_by_natural_key_including_deleted(
                    "local",
                    redirected_pair.0,
                    redirected_pair.1,
                    EdgeRelation::Supersedes,
                )
                .await
                .unwrap()
                .is_none());
        }
    }
}
