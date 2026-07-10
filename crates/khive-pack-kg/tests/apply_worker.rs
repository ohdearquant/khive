//! Integration tests for ProposalApplyWorker.
//!
//! Moved from `src/apply_worker.rs` inline tests (KG-AUD-004: inline test
//! sections must be under 300 lines).

use khive_pack_kg::apply_worker::ProposalApplyWorker;
use khive_pack_kg::projection_worker::ProposalsProjectionWorker;
use khive_pack_kg::KgPack;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, VerbRegistryBuilder};
use khive_storage::types::{PageRequest, SqlStatement, SqlValue};
use khive_storage::EventFilter;
use khive_types::EventKind;
use khive_types::{Id128, NoteDraft, ProposalChangeset, ProposalCreatedPayload};
use uuid::Uuid;

fn setup() -> (KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    (rt, tok)
}

fn build_registry(rt: &KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
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

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed");

    let edges = rt
        .list_edges(
            &tok,
            khive_runtime::EdgeListFilter {
                source_id: Some(e1.id),
                ..Default::default()
            },
            100,
            0,
        )
        .await
        .expect("list_edges");
    assert!(
        !edges.is_empty(),
        "apply_worker must have created an edge between EntityA and EntityB"
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
    assert_eq!(
        applied_events.items.len(),
        1,
        "exactly one ProposalApplied event must be emitted"
    );

    let projection = ProposalsProjectionWorker::new(rt.clone());
    let row = projection
        .get_proposal_row(&tok, proposal_id)
        .await
        .expect("get row")
        .expect("row must exist");
    assert_eq!(row.status, "applied");
}

/// ADR-046 apply-worker path test for the codex PR #814 Medium finding: the
/// five new content_strategy tests only exercised the runtime directly, not
/// a proposal-driven merge. `apply_merge_entities` hard-codes
/// `EntityDedupMergePolicy::PreferInto` + `ContentMergeStrategy::Append`
/// (the safe, lossless default for proposal-driven merges) — verify that
/// path actually rewires edges and tombstones the source.
#[tokio::test]
async fn apply_worker_applies_merge_entities_changeset() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let into = rt
        .create_entity(&tok, "concept", None, "Into", Some("desc A"), None, vec![])
        .await
        .expect("create into");
    let from = rt
        .create_entity(&tok, "concept", None, "From", Some("desc B"), None, vec![])
        .await
        .expect("create from");
    let other = rt
        .create_entity(&tok, "concept", None, "Other", None, None, vec![])
        .await
        .expect("create other");
    rt.link(
        &tok,
        other.id,
        from.id,
        khive_types::EdgeRelation::Extends,
        1.0,
        None,
    )
    .await
    .expect("link other -> from");

    let proposal_id = Uuid::new_v4();
    let changeset = ProposalChangeset::MergeEntities {
        into: Id128::from_u128(into.id.as_u128()),
        from: Id128::from_u128(from.id.as_u128()),
    };

    seed_proposal_created_event(&rt, &tok, proposal_id, changeset).await;
    insert_projection_row(&rt, &tok, proposal_id, "approved").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed");

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

    let merged = rt
        .get_entity(&tok, into.id)
        .await
        .expect("into entity must still exist");
    assert_eq!(
        merged.description.as_deref(),
        Some("desc A\n\n---\n\ndesc B"),
        "apply_merge_entities must merge content with the Append default"
    );

    let source = rt
        .get_entity(&tok, from.id)
        .await
        .expect_err("from entity must be tombstoned (soft-deleted) after merge");
    assert!(
        matches!(source, khive_runtime::RuntimeError::NotFound(_)),
        "expected NotFound for the tombstoned source entity, got: {source:?}"
    );

    let other_neighbors = rt
        .neighbors(
            &tok,
            other.id,
            khive_storage::types::Direction::Out,
            None,
            None,
        )
        .await
        .expect("neighbors must succeed");
    assert_eq!(other_neighbors.len(), 1);
    assert_eq!(
        other_neighbors[0].node_id, into.id,
        "the edge from `other` must have been rewired onto the surviving `into` entity"
    );
}

#[tokio::test]
async fn apply_worker_skips_non_approved_proposals() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
    insert_projection_row(&rt, &tok, proposal_id, "open").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed without error");

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

#[tokio::test]
async fn apply_worker_rejects_invalid_entity_kind() {
    use khive_types::EntityDraft;

    let (rt, tok) = setup();
    ensure_schema(&rt).await;

    let proposal_id = Uuid::new_v4();
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

    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert!(
        !entities.iter().any(|e| e.name == "BadEntity"),
        "entity with invalid kind must not be created in the KG"
    );
}

#[tokio::test]
async fn apply_worker_skips_kg_mutation_when_withdrawn_after_approve() {
    use khive_types::EntityDraft;

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
    insert_projection_row(&rt, &tok, proposal_id, "withdrawn").await;

    let registry = build_registry(&rt);
    let worker = ProposalApplyWorker::new(rt.clone());
    worker
        .maybe_apply(&tok, proposal_id, &registry, None)
        .await
        .expect("maybe_apply must succeed without error");

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

    let entities = rt
        .list_entities(&tok, None, None, 100, 0)
        .await
        .expect("list_entities");
    assert!(
        !entities.iter().any(|e| e.name == "ShouldNotExist"),
        "H2: KG must not be mutated when proposal was withdrawn before apply"
    );
}

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
