use super::*;
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistryBuilder};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_types::{Id128, NoteDraft, ProposalChangeset, ProposalCreatedPayload};
use uuid::Uuid;

fn setup() -> (KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    (rt, tok)
}

fn build_registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    builder.build().expect("registry build")
}

async fn ensure_schema(rt: &KhiveRuntime) {
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "\
            CREATE TABLE IF NOT EXISTS proposals_open (\
                proposal_id TEXT PRIMARY KEY, \
                namespace TEXT NOT NULL, \
                proposer TEXT NOT NULL, \
                title TEXT NOT NULL, \
                status TEXT NOT NULL, \
                created_at INTEGER NOT NULL, \
                updated_at INTEGER NOT NULL, \
                expiry INTEGER, \
                last_decision TEXT, \
                review_count INTEGER NOT NULL DEFAULT 0, \
                approve_count INTEGER NOT NULL DEFAULT 0, \
                reject_count INTEGER NOT NULL DEFAULT 0\
            )"
            .to_string(),
            params: vec![],
            label: Some("test.ensure_schema".into()),
        })
        .await
        .expect("create table");
}

async fn insert_projection_row(
    rt: &KhiveRuntime,
    tok: &NamespaceToken,
    proposal_id: Uuid,
    status: &str,
) {
    let now = chrono::Utc::now().timestamp_micros();
    let ns = tok.namespace().as_str().to_owned();
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT OR REPLACE INTO proposals_open \
              (proposal_id, namespace, proposer, title, status, created_at, updated_at, \
               approve_count, reject_count, review_count) \
              VALUES (?1, ?2, 'alice', 'Test', ?3, ?4, ?4, 1, 0, 1)"
                .to_string(),
            params: vec![
                SqlValue::Text(proposal_id.to_string()),
                SqlValue::Text(ns),
                SqlValue::Text(status.to_string()),
                SqlValue::Integer(now),
            ],
            label: Some("test.insert_projection_row".into()),
        })
        .await
        .expect("insert row");
}

async fn seed_proposal_created_event(
    rt: &KhiveRuntime,
    tok: &NamespaceToken,
    proposal_id: Uuid,
    changeset: ProposalChangeset,
) {
    let payload = ProposalCreatedPayload {
        proposal_id: Id128::from_u128(proposal_id.as_u128()),
        proposer: "alice".to_string(),
        title: "Test".to_string(),
        description: "desc".to_string(),
        changeset,
        reviewers: vec![],
        expiry: None,
        parent_id: None,
    };
    let payload_json = serde_json::to_value(&payload).expect("serialize");
    let mut event = khive_storage::event::Event::new(
        tok.namespace().as_str(),
        "propose",
        EventKind::ProposalCreated,
        khive_storage::SubstrateKind::Entity,
        "alice",
    );
    event.payload = payload_json;
    event.aggregate_kind = Some("proposal".to_string());
    event.aggregate_id = Some(proposal_id);
    rt.events(tok)
        .expect("event store")
        .append_event(event)
        .await
        .expect("append event");
}

#[tokio::test]
async fn apply_worker_applies_add_edge_changeset() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    // Create two entities to link.
    let e1 = rt
        .create_entity(&tok, "concept", None, "EntityA", None, None, vec![])
        .await
        .expect("create e1");
    let e2 = rt
        .create_entity(&tok, "concept", None, "EntityB", None, None, vec![])
        .await
        .expect("create e2");

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::AddEdge {
        source: Id128::from_u128(e1.id.as_u128()),
        target: Id128::from_u128(e2.id.as_u128()),
        relation: khive_types::EdgeRelation::Extends,
        weight: Some(1.0),
    };

    // Seed the ProposalCreated event in the event store.
    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;

    // Seed the projection row in 'approved' state (1 approve, 0 rejects).
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed");

    // Verify: edge exists in graph store (source = e1).
    let edges = rt
        .list_edges(
            &tok,
            khive_runtime::EdgeListFilter {
                source_id: Some(e1.id),
                ..Default::default()
            },
            100,
        )
        .await
        .expect("list_edges");
    assert!(
        !edges.is_empty(),
        "apply_worker must have created an edge between EntityA and EntityB"
    );

    // Verify: ProposalApplied event was emitted.
    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    assert_eq!(
        applied_events.items.len(),
        1,
        "exactly one ProposalApplied event must be emitted"
    );

    // Verify: projection status updated to 'applied'.
    let projection = ProposalsProjectionWorker::new(rt.clone());
    let row = projection
        .get_proposal_row(&tok, proposal_id)
        .await
        .expect("get row")
        .expect("row must exist");
    assert_eq!(row.status, "applied");
}

#[tokio::test]
async fn apply_worker_skips_non_approved_proposals() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    // Projection row is 'open' — should not apply.
    insert_projection_row(&rt, &tok, proposal_id, "open").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed without error");

    // Verify no ProposalApplied event was emitted.
    let event_store = rt.events(&tok).expect("event store");
    let applied = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query");
    assert_eq!(
        applied.items.len(),
        0,
        "no ProposalApplied event should be emitted for a non-approved proposal"
    );
}

/// C2 regression: apply worker must reject invalid entity kinds the same way `create` does.
#[tokio::test]
async fn apply_worker_rejects_invalid_entity_kind() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    // Changeset references an invalid entity kind (not in the closed taxonomy).
    let changeset = ProposalChangeset::AddEntity {
        entity: EntityDraft {
            kind: "invalidkind".to_string(),
            name: "BadEntity".to_string(),
            description: Some("should fail".to_string()),
            properties: None,
            tags: vec![],
        },
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply itself must succeed (errors emitted as ProposalApplied{Failed})");

    // The apply must have emitted a ProposalApplied{Failed} event, not success.
    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");

    assert_eq!(
        applied_events.items.len(),
        1,
        "ProposalApplied event must be emitted"
    );

    // Verify no entity with that name was created.
    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert!(
        !entities.iter().any(|e| e.name == "BadEntity"),
        "entity with invalid kind must not be created in the KG"
    );
}

/// H2 regression: apply worker must not mutate the KG when proposal withdrawn before worker runs.
#[tokio::test]
async fn apply_worker_skips_kg_mutation_when_withdrawn_after_approve() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::AddEntity {
        entity: EntityDraft {
            kind: "concept".to_string(),
            name: "ShouldNotExist".to_string(),
            description: Some("withdrawn before apply".to_string()),
            properties: None,
            tags: vec![],
        },
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;

    // Start in 'withdrawn' status — simulates: approve → withdraw both landed
    // before the apply worker runs.
    insert_projection_row(&rt, &tok, proposal_id, "withdrawn").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed without error");

    // Assert: no ProposalApplied event was emitted (worker bailed out early).
    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query applied events");
    assert_eq!(
        applied_events.items.len(),
        0,
        "H2: no ProposalApplied event must be emitted when proposal is withdrawn"
    );

    // Assert: no entity was created in the KG.
    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert!(
        !entities.iter().any(|e| e.name == "ShouldNotExist"),
        "H2: KG must not be mutated when proposal was withdrawn before apply"
    );
}

/// C3 regression: apply worker must reject invalid note kinds the same way `create` does.
#[tokio::test]
async fn apply_worker_rejects_invalid_note_kind() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::AddNote {
        note: NoteDraft {
            kind: "invalidnotekind".to_string(),
            name: Some("BadNote".to_string()),
            content: "should fail".to_string(),
            properties: None,
        },
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply itself must succeed (errors emitted as ProposalApplied{Failed})");

    // The apply must have emitted a ProposalApplied{Failed} event, not success.
    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");

    assert_eq!(
        applied_events.items.len(),
        1,
        "C3: ProposalApplied event must be emitted"
    );

    // Verify no note with that name was created.
    let notes = rt
        .notes(&tok)
        .expect("notes store")
        .query_notes(
            tok.namespace().as_str(),
            None,
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .expect("query_notes");
    assert!(
        !notes
            .items
            .iter()
            .any(|n| n.name.as_deref() == Some("BadNote")),
        "C3: note with invalid kind must not be created in the KG"
    );
}

/// #423 regression: a legacy queued multi-step Compound (AddEntity then an
/// AddEdge with a phantom target) must not leak the entity write — the whole
/// compound is rejected before any step is applied.
#[tokio::test]
async fn apply_worker_rejects_legacy_multi_step_compound_without_partial_entity_write() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::Compound {
        steps: vec![
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("AtomicLeak"),
            },
            ProposalChangeset::AddEdge {
                source: Id128::from_u128(Uuid::new_v4().as_u128()),
                target: Id128::from_u128(Uuid::new_v4().as_u128()),
                relation: khive_types::EdgeRelation::Extends,
                weight: Some(1.0),
            },
        ],
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed (rejection emitted as ProposalApplied{Failed})");

    // No entity leaked from the first (successful-looking) step.
    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert!(
        !entities.iter().any(|e| e.name == "AtomicLeak"),
        "#423: multi-step Compound must not leak the AddEntity write"
    );

    // Exactly one ProposalApplied{Failed} event, applied_step_count == 0.
    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    assert_eq!(
        applied_events.items.len(),
        1,
        "#423: exactly one ProposalApplied event must be emitted"
    );
    let payload_str = applied_events.items[0].payload.to_string();
    assert!(
        payload_str.contains("multi-step Compound"),
        "#423: failure payload must mention multi-step Compound; got: {payload_str}"
    );
    assert!(
        payload_str.contains("\"applied_step_count\":0"),
        "#423: failure payload must report zero applied steps; got: {payload_str}"
    );
}

/// #423 regression: `propose` itself must reject multi-step Compound changesets
/// up front, before a proposal is even created.
#[tokio::test]
async fn propose_rejects_multi_step_compound_until_atomic_apply() {
    let (rt, _tok) = setup();
    ensure_schema(&rt).await;

    let registry = build_registry(&rt);
    let params = serde_json::json!({
        "title": "Multi-step compound",
        "description": "should be rejected",
        "changeset": {
            "kind": "compound",
            "steps": [
                {"kind": "add_entity", "entity": {"kind": "concept", "name": "A"}},
                {"kind": "add_entity", "entity": {"kind": "concept", "name": "B"}}
            ]
        }
    });
    let err = registry
        .dispatch("propose", params)
        .await
        .expect_err("multi-step Compound propose must fail");
    match err {
        RuntimeError::InvalidInput(msg) => {
            assert!(
                msg.contains("multi-step Compound"),
                "unexpected message: {msg}"
            );
        }
        other => panic!("unexpected error variant: {other}"),
    }
}

/// #424 regression: `AddEntity` proposals must accept pack-local entity kinds
/// (e.g. `resource`) even though `khive_types::EntityKind` does not know them.
#[tokio::test]
async fn apply_add_entity_accepts_pack_local_resource_kind() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::AddEntity {
        entity: EntityDraft {
            kind: "resource".to_string(),
            name: "ProposedResource".to_string(),
            description: None,
            properties: None,
            tags: vec![],
        },
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed");

    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    let created = entities
        .iter()
        .find(|e| e.name == "ProposedResource")
        .expect("#424: ProposedResource must be created");
    assert_eq!(
        created.kind, "resource",
        "#424: kind must resolve to resource"
    );
}

/// #424 regression: `AddEntity` proposals must reject whitespace-only names the
/// same way the normal `create` handler does, with zero net writes.
#[tokio::test]
async fn apply_add_entity_rejects_whitespace_name_without_write() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::AddEntity {
        entity: EntityDraft {
            kind: "concept".to_string(),
            name: "   ".to_string(),
            description: None,
            properties: None,
            tags: vec![],
        },
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let entities_before = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed (errors emitted as ProposalApplied{Failed})");

    let entities_after = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert_eq!(
        entities_before.len(),
        entities_after.len(),
        "#424: whitespace-only name must result in zero net writes"
    );

    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    assert_eq!(applied_events.items.len(), 1);
    let payload_str = applied_events.items[0].payload.to_string();
    assert!(
        payload_str.contains("name must not be empty"),
        "#424: failure payload must mention the empty-name guard; got: {payload_str}"
    );
}

// ---- Write-budget tests ------------------------------------------------

fn make_entity_draft(name: &str) -> EntityDraft {
    EntityDraft {
        kind: "concept".to_string(),
        name: name.to_string(),
        description: None,
        properties: None,
        tags: vec![],
    }
}

/// Over-budget flat Compound: 3 AddEntity steps, budget=Some(2).
/// Expects: ProposalApplied{Failed} with WriteBudgetExceeded, zero new entities.
#[tokio::test]
async fn budget_exceeded_flat_compound_creates_zero_rows() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::Compound {
        steps: vec![
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("BudgetA"),
            },
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("BudgetB"),
            },
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("BudgetC"),
            },
        ],
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let entities_before = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, Some(2))
        .await
        .expect("maybe_apply must succeed (budget error emitted as ProposalApplied{Failed})");

    // Verify: ProposalApplied{Failed} was emitted.
    let event_store = rt.events(&tok).expect("event store");
    let applied_events = event_store
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(proposal_id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .expect("query events");
    assert_eq!(
        applied_events.items.len(),
        1,
        "budget: ProposalApplied event must be emitted on over-budget"
    );
    // #423: multi-step Compound is now rejected for containment before the
    // budget check ever runs, so the failure reason is the containment
    // message rather than WriteBudgetExceeded. The all-or-nothing outcome
    // (zero new entity rows) is what this test actually guards.
    let payload_str = applied_events.items[0].payload.to_string();
    assert!(
        payload_str.contains("WriteBudgetExceeded")
            || payload_str.contains("write budget exceeded")
            || payload_str.contains("multi-step Compound"),
        "budget: failure payload must mention WriteBudgetExceeded or multi-step Compound; got: {payload_str}"
    );

    // Verify: zero new entity rows (all-or-nothing guarantee).
    let entities_after = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert_eq!(
        entities_before.len(),
        entities_after.len(),
        "budget: over-budget apply must create zero new entity rows"
    );
}

/// #423 superseded this scenario: a 2-step Compound (previously "in-budget,
/// applies fully") is now rejected outright as a multi-step Compound before
/// the budget check runs, regardless of budget. Renamed from
/// `budget_in_budget_flat_compound_applies_fully` to reflect the new contract.
#[tokio::test]
async fn budget_in_budget_flat_compound_is_rejected_as_multi_step() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::Compound {
        steps: vec![
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("InBudgetA"),
            },
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("InBudgetB"),
            },
        ],
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, Some(2))
        .await
        .expect("maybe_apply must succeed (rejection emitted as ProposalApplied{Failed})");

    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert!(
        !entities.iter().any(|e| e.name == "InBudgetA"),
        "#423: multi-step Compound must not apply any step, including InBudgetA"
    );
    assert!(
        !entities.iter().any(|e| e.name == "InBudgetB"),
        "#423: multi-step Compound must not apply any step, including InBudgetB"
    );
}

/// Nested Compound: outer has 1 AddEntity + inner Compound with 2 AddEntity.
/// Now rejected as multi-step Compound (#423) before the budget check runs;
/// zero-write invariant still holds either way.
#[tokio::test]
async fn budget_nested_compound_counts_recursively() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::Compound {
        steps: vec![
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("NestedOuter"),
            },
            ProposalChangeset::Compound {
                steps: vec![
                    ProposalChangeset::AddEntity {
                        entity: make_entity_draft("NestedInnerA"),
                    },
                    ProposalChangeset::AddEntity {
                        entity: make_entity_draft("NestedInnerB"),
                    },
                ],
            },
        ],
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let entities_before = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, Some(2))
        .await
        .expect("maybe_apply must succeed (error as ProposalApplied{Failed})");

    let entities_after = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert_eq!(
        entities_before.len(),
        entities_after.len(),
        "budget: nested over-budget must create zero rows"
    );
}

/// Some(0) budget: AddEdge-only changeset still applies; no new entity rows needed.
#[tokio::test]
async fn budget_some_zero_allows_edge_only_changeset() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    // Pre-create two entities outside the proposal.
    let e1 = rt
        .create_entity(&tok, "concept", None, "EdgeSrc", None, None, vec![])
        .await
        .expect("create e1");
    let e2 = rt
        .create_entity(&tok, "concept", None, "EdgeDst", None, None, vec![])
        .await
        .expect("create e2");

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::AddEdge {
        source: Id128::from_u128(e1.id.as_u128()),
        target: Id128::from_u128(e2.id.as_u128()),
        relation: khive_types::EdgeRelation::Extends,
        weight: Some(1.0),
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, Some(0))
        .await
        .expect("maybe_apply must succeed");

    // Verify edge was created despite budget=Some(0).
    let edges = rt
        .list_edges(
            &tok,
            khive_runtime::EdgeListFilter {
                source_id: Some(e1.id),
                ..Default::default()
            },
            100,
        )
        .await
        .expect("list_edges");
    assert!(
        !edges.is_empty(),
        "budget: Some(0) must not block AddEdge-only changeset"
    );
}

/// WriteBudget unit test: consume_new_entry() honours the limit.
#[test]
fn write_budget_consume_enforces_limit() {
    let mut budget = WriteBudget::new(Some(2));
    assert!(budget.consume_new_entry().is_ok(), "first consume ok");
    assert!(budget.consume_new_entry().is_ok(), "second consume ok");
    let err = budget.consume_new_entry().expect_err("third must fail");
    match err {
        RuntimeError::WriteBudgetExceeded {
            max_new_entries,
            attempted_new_entries,
        } => {
            assert_eq!(max_new_entries, 2);
            assert_eq!(attempted_new_entries, 3);
        }
        other => panic!("unexpected error: {other}"),
    }
}

/// WriteBudget unit test: None budget never fails.
#[test]
fn write_budget_none_is_unlimited() {
    let mut budget = WriteBudget::new(None);
    for _ in 0..1000 {
        assert!(budget.consume_new_entry().is_ok());
    }
}

/// count_new_entries unit test: flat and nested Compound.
#[test]
fn count_new_entries_recursive() {
    let flat = ProposalChangeset::Compound {
        steps: vec![
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("X"),
            },
            ProposalChangeset::AddNote {
                note: khive_types::NoteDraft {
                    kind: "observation".to_string(),
                    name: None,
                    content: "c".to_string(),
                    properties: None,
                },
            },
            ProposalChangeset::AddEdge {
                source: Id128::from_u128(0),
                target: Id128::from_u128(1),
                relation: khive_types::EdgeRelation::Extends,
                weight: None,
            },
        ],
    };
    assert_eq!(
        count_new_entries(&flat),
        2,
        "AddEntity + AddNote = 2; AddEdge = 0"
    );

    let nested = ProposalChangeset::Compound {
        steps: vec![
            ProposalChangeset::AddEntity {
                entity: make_entity_draft("Y"),
            },
            ProposalChangeset::Compound {
                steps: vec![
                    ProposalChangeset::AddEntity {
                        entity: make_entity_draft("Z"),
                    },
                    ProposalChangeset::AddNote {
                        note: khive_types::NoteDraft {
                            kind: "observation".to_string(),
                            name: None,
                            content: "c".to_string(),
                            properties: None,
                        },
                    },
                ],
            },
        ],
    };
    assert_eq!(count_new_entries(&nested), 3, "1 + 2 nested = 3");
}
