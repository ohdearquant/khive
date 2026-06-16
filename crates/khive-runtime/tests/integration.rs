//! Integration tests for khive-runtime.
//!
//! Tests cover entity CRUD, graph operations, note memory, GQL query,
//! and namespace isolation using an in-memory runtime.

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::types::{Direction, TraversalOptions, TraversalRequest};
use khive_storage::{EdgeRelation, Event};
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("in-memory runtime")
}

// =============================================================================
// Entity operations
// =============================================================================

#[tokio::test]
async fn entity_create_and_get_roundtrip() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "LoRA",
            Some("Low-Rank Adaptation"),
            None,
            vec![],
        )
        .await
        .unwrap();

    let fetched = rt.get_entity(&tok, entity.id).await.unwrap();
    assert_eq!(fetched.id, entity.id);
    assert_eq!(fetched.name, "LoRA");
    assert_eq!(fetched.kind, "concept");
    assert_eq!(fetched.description.as_deref(), Some("Low-Rank Adaptation"));
}

#[tokio::test]
async fn entity_create_with_properties_and_tags() {
    let rt = rt();
    let research_tok = rt.authorize(Namespace::parse("research").unwrap()).unwrap();

    let props = serde_json::json!({"domain": "fine-tuning", "type": "technique"});
    let entity = rt
        .create_entity(
            &research_tok,
            "concept",
            None,
            "QLoRA",
            Some("Quantized LoRA"),
            Some(props.clone()),
            vec!["fine-tuning".to_string(), "quantization".to_string()],
        )
        .await
        .unwrap();

    let fetched = rt.get_entity(&research_tok, entity.id).await.unwrap();
    assert_eq!(fetched.properties, Some(props));
    assert_eq!(fetched.tags, vec!["fine-tuning", "quantization"]);
}

#[tokio::test]
async fn entity_list_by_kind() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    rt.create_entity(&tok, "concept", None, "FlashAttention", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(&tok, "concept", None, "GQA", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(
        &tok,
        "document",
        None,
        "Attention Is All You Need",
        None,
        None,
        vec![],
    )
    .await
    .unwrap();

    let concepts = rt
        .list_entities(&tok, Some("concept"), None, 50, 0)
        .await
        .unwrap();
    assert_eq!(concepts.len(), 2);
    assert!(concepts.iter().any(|e| e.name == "FlashAttention"));
    assert!(concepts.iter().any(|e| e.name == "GQA"));

    let docs = rt
        .list_entities(&tok, Some("document"), None, 50, 0)
        .await
        .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "Attention Is All You Need");

    let all = rt.list_entities(&tok, None, None, 50, 0).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn entity_delete_soft() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let entity = rt
        .create_entity(&tok, "concept", None, "to-delete", None, None, vec![])
        .await
        .unwrap();

    let deleted = rt.delete_entity(&tok, entity.id, false).await.unwrap();
    assert!(deleted);

    // Soft-deleted entity is not found via get_entity
    let fetched = rt.get_entity(&tok, entity.id).await;
    assert!(fetched.is_err());
}

#[tokio::test]
async fn entity_count_by_kind() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    for _ in 0..3 {
        rt.create_entity(&tok, "concept", None, "concept-X", None, None, vec![])
            .await
            .unwrap();
    }
    for _ in 0..2 {
        rt.create_entity(&tok, "document", None, "doc-Y", None, None, vec![])
            .await
            .unwrap();
    }

    let concept_count = rt.count_entities(&tok, Some("concept")).await.unwrap();
    let doc_count = rt.count_entities(&tok, Some("document")).await.unwrap();
    let total = rt.count_entities(&tok, None).await.unwrap();

    assert_eq!(concept_count, 3);
    assert_eq!(doc_count, 2);
    assert_eq!(total, 5);
}

// =============================================================================
// Graph operations
// =============================================================================

#[tokio::test]
async fn link_and_neighbors() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let lora = rt
        .create_entity(&tok, "concept", None, "LoRA", None, None, vec![])
        .await
        .unwrap();
    let qlora = rt
        .create_entity(&tok, "concept", None, "QLoRA", None, None, vec![])
        .await
        .unwrap();

    rt.link(&tok, qlora.id, lora.id, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();

    let hits = rt
        .neighbors(&tok, qlora.id, Direction::Out, None, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, lora.id);
    assert_eq!(hits[0].relation, EdgeRelation::VariantOf);
}

#[tokio::test]
async fn traverse_multi_hop() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity(&tok, "concept", None, "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "B", None, None, vec![])
        .await
        .unwrap();
    let c = rt
        .create_entity(&tok, "concept", None, "C", None, None, vec![])
        .await
        .unwrap();

    rt.link(&tok, a.id, b.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok, b.id, c.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();

    let request = TraversalRequest {
        roots: vec![a.id],
        options: TraversalOptions {
            max_depth: 2,
            direction: Direction::Out,
            relations: Some(vec![EdgeRelation::Extends]),
            ..Default::default()
        },
        include_roots: false,
    };

    let paths = rt.traverse(&tok, request).await.unwrap();
    assert!(!paths.is_empty());

    // All traversed nodes should be reachable from a
    let reachable_ids: Vec<Uuid> = paths
        .iter()
        .flat_map(|p| p.nodes.iter().map(|n| n.node_id))
        .collect();
    assert!(reachable_ids.contains(&b.id));
    assert!(reachable_ids.contains(&c.id));
}

// =============================================================================
// Note (memory) operations
// =============================================================================

#[tokio::test]
async fn create_note_and_list_notes() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    rt.create_note(
        &tok,
        "observation",
        None,
        "LoRA is a fine-tuning technique",
        Some(0.9),
        None,
        vec![],
    )
    .await
    .unwrap();
    rt.create_note(
        &tok,
        "observation",
        None,
        "QLoRA uses quantization",
        Some(0.8),
        None,
        vec![],
    )
    .await
    .unwrap();
    rt.create_note(
        &tok,
        "question",
        None,
        "Review LoRA paper",
        Some(0.7),
        None,
        vec![],
    )
    .await
    .unwrap();

    let observations = rt
        .list_notes(&tok, Some("observation"), 50, 0)
        .await
        .unwrap();
    assert_eq!(observations.len(), 2);

    let questions = rt.list_notes(&tok, Some("question"), 50, 0).await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].content, "Review LoRA paper");

    let all = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn create_all_note_kinds() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    for kind in [
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ] {
        rt.create_note(&tok, kind, None, "content", Some(0.5), None, vec![])
            .await
            .unwrap();
    }
    let all = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert_eq!(all.len(), 5);
}

// =============================================================================
// GQL query
// =============================================================================

#[tokio::test]
async fn query_via_gql() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Set up entities and edges
    let lora = rt
        .create_entity(&tok, "concept", None, "LoRA", None, None, vec![])
        .await
        .unwrap();
    let qlora = rt
        .create_entity(&tok, "concept", None, "QLoRA", None, None, vec![])
        .await
        .unwrap();
    rt.link(&tok, qlora.id, lora.id, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();

    // Run a GQL traversal query
    let rows = rt
        .query(
            &tok,
            "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a, e, b LIMIT 10",
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    // Verify row contains the expected column names
    let first_row = &rows[0];
    assert!(first_row.get("a_name").is_some() || first_row.get("a_kind").is_some());
}

// =============================================================================
// Namespace isolation
// =============================================================================

#[tokio::test]
async fn namespace_isolation() {
    let rt = rt();
    let ns_a_tok = rt.authorize(Namespace::parse("ns-a").unwrap()).unwrap();
    let ns_b_tok = rt.authorize(Namespace::parse("ns-b").unwrap()).unwrap();

    rt.create_entity(&ns_a_tok, "concept", None, "EntityA", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(&ns_b_tok, "concept", None, "EntityB", None, None, vec![])
        .await
        .unwrap();

    let a_entities = rt
        .list_entities(&ns_a_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(a_entities.len(), 1);
    assert_eq!(a_entities[0].name, "EntityA");

    let b_entities = rt
        .list_entities(&ns_b_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(b_entities.len(), 1);
    assert_eq!(b_entities[0].name, "EntityB");
}

// =============================================================================
// Hybrid search indexing
// =============================================================================

#[tokio::test]
async fn create_entity_indexes_into_text_search() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "FlashAttention",
            Some("efficient attention mechanism"),
            None,
            vec![],
        )
        .await
        .unwrap();
    let hits = rt
        .hybrid_search(&tok, "FlashAttention", None, 10, None, None)
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.entity_id == entity.id),
        "newly created entity should be findable via hybrid_search (text path)"
    );
}

#[tokio::test]
async fn create_entity_no_embedding_model_does_not_propagate_vector_error() {
    // KhiveRuntime::memory() has embedding_model: None — vector indexing is silently skipped.
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let result = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "SilentVectorSkip",
            None,
            None,
            vec![],
        )
        .await;
    assert!(
        result.is_ok(),
        "create_entity must not propagate Unconfigured from vector store"
    );
}

// =============================================================================
// Soft-delete visibility
// =============================================================================

/// Soft-deleted entities must not appear in hybrid_search results.
#[tokio::test]
async fn hybrid_search_excludes_soft_deleted_entities() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "SoftDeleteMe",
            Some("entity that will be soft-deleted"),
            None,
            vec![],
        )
        .await
        .unwrap();

    // Confirm the entity is visible before deletion.
    let hits_before = rt
        .hybrid_search(&tok, "SoftDeleteMe", None, 10, None, None)
        .await
        .unwrap();
    assert!(
        hits_before.iter().any(|h| h.entity_id == entity.id),
        "entity should appear in hybrid_search before soft-delete"
    );

    rt.delete_entity(&tok, entity.id, false).await.unwrap(); // soft delete

    let hits_after = rt
        .hybrid_search(&tok, "SoftDeleteMe", None, 10, None, None)
        .await
        .unwrap();
    assert!(
        !hits_after.iter().any(|h| h.entity_id == entity.id),
        "soft-deleted entity must not appear in hybrid_search"
    );
}

/// Hard-deleted entities are gone from storage entirely and never appear in hybrid_search.
#[tokio::test]
async fn hybrid_search_excludes_hard_deleted_entities() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "HardDeleteMe",
            Some("entity that will be hard-deleted"),
            None,
            vec![],
        )
        .await
        .unwrap();

    let hits_before = rt
        .hybrid_search(&tok, "HardDeleteMe", None, 10, None, None)
        .await
        .unwrap();
    assert!(
        hits_before.iter().any(|h| h.entity_id == entity.id),
        "entity should appear in hybrid_search before hard-delete"
    );

    rt.delete_entity(&tok, entity.id, true).await.unwrap(); // hard delete

    // Hard-deleted rows are gone from the entity store; the FTS/vector indexes may still
    // have stale entries. The soft-delete filter sees no alive entity and drops the hit.
    let hits_after = rt
        .hybrid_search(&tok, "HardDeleteMe", None, 10, None, None)
        .await
        .unwrap();
    assert!(
        !hits_after.iter().any(|h| h.entity_id == entity.id),
        "hard-deleted entity must not appear in hybrid_search"
    );
}

/// Soft-deleted notes must not appear in list_notes results.
#[tokio::test]
async fn list_notes_excludes_soft_deleted() {
    use khive_storage::types::DeleteMode;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "soft-delete-test",
            Some(0.9),
            None,
            vec![],
        )
        .await
        .unwrap();

    let notes_before = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert!(
        notes_before.iter().any(|n| n.id == note.id),
        "note should appear before soft-delete"
    );

    rt.notes(&tok)
        .unwrap()
        .delete_note(note.id, DeleteMode::Soft)
        .await
        .unwrap();

    let notes_after = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert!(
        !notes_after.iter().any(|n| n.id == note.id),
        "soft-deleted note must not appear in list"
    );
}

// =============================================================================
// File-backed runtime
// =============================================================================

#[tokio::test]
async fn file_backed_runtime_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.db");

    {
        let config = RuntimeConfig {
            db_path: Some(path.clone()),
            default_namespace: Namespace::local(),
            embedding_model: None,
            gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            additional_embedding_models: vec![],
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let tok = rt.authorize(Namespace::local()).unwrap();
        rt.create_entity(&tok, "concept", None, "Persistent", None, None, vec![])
            .await
            .unwrap();
    }

    // Re-open the same file
    {
        let config = RuntimeConfig {
            db_path: Some(path.clone()),
            default_namespace: Namespace::local(),
            embedding_model: None,
            gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            additional_embedding_models: vec![],
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let tok = rt.authorize(Namespace::local()).unwrap();
        let entities = rt.list_entities(&tok, None, None, 50, 0).await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "Persistent");
    }
}

// =============================================================================
// F218 integration: synthetic observed_as_* edge end-to-end (CRIT-1 regression)
// =============================================================================

/// This test is the ONLY test that would have caught CRIT-1 (wrong JOIN target).
///
/// It seeds a real event + event_observations row and executes the canonical
/// ADR-041 §11 synthetic-edge GQL query end-to-end against an in-memory SQLite
/// database.  The old code joined `event_observations.event_id = entities.id`,
/// which can never match because the two ID spaces are disjoint.
#[tokio::test]
async fn synthetic_edge_observed_as_selected_returns_memory_note() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ns = "local";

    // Step 1: create a memory note (the observed entity).
    let memory_note = rt
        .create_note(
            &tok,
            "memory",
            None,
            "recalled memory content",
            Some(0.9),
            None,
            vec![],
        )
        .await
        .unwrap();
    let memory_id = memory_note.id;

    // Step 2: create an event of kind RerankExecuted with a payload that
    // includes `selected: [memory_id]`.  The storage layer's `append_event`
    // implementation calls `decode_rank_observations`, which reads
    // `payload["selected"]` and inserts a row into `event_observations` with
    // role="selected" and entity_id=memory_id.
    let event_store = rt.events(&tok).unwrap();
    let mut event = Event::new(
        ns,
        "rerank",
        EventKind::RerankExecuted,
        SubstrateKind::Note,
        "agent:test",
    );
    event.payload = serde_json::json!({
        "candidates": [],
        "selected": [memory_id.to_string()]
    });
    event_store.append_event(event).await.unwrap();

    // Step 3: execute the canonical ADR-041 §11 GQL query.
    // Before CRIT-1 fix: `FROM entities n0 JOIN event_observations e0 ON e0.event_id = n0.id`
    //   — IDs are disjoint, so zero rows returned.
    // After fix: `FROM events n0 JOIN event_observations e0 ON e0.event_id = n0.id`
    //   — correct join; the memory note is returned.
    let rows = rt
        .query(
            &tok,
            "MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN m",
        )
        .await
        .unwrap();

    assert!(
        !rows.is_empty(),
        "CRIT-1: synthetic edge query must return at least one row (memory note was seeded); \
         got 0 rows — event_observations join is broken"
    );

    // Verify the returned row contains our memory note's UUID.
    let memory_id_str = memory_id.to_string();
    let found = rows.iter().any(|row| {
        row.columns.iter().any(|col| {
            if let khive_storage::types::SqlValue::Text(s) = &col.value {
                s.contains(&memory_id_str)
            } else {
                false
            }
        })
    });
    assert!(
        found,
        "CRIT-1: returned rows must include the seeded memory note id {}; columns: {:?}",
        memory_id,
        rows.iter()
            .map(|r| r
                .columns
                .iter()
                .map(|c| (&c.name, &c.value))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>()
    );
}

// =============================================================================
// update_edge conflict handling regression tests (codex round-3 H1)
// =============================================================================

/// Regression for Bug 1: when update_edge absorbs a conflict (the requested edge
/// is deleted and the existing canonical row is refreshed), the returned edge must
/// carry the SURVIVING canonical row's id — not the id of the deleted edge.
///
/// Setup: pre-create canonical A→B competes_with (E1), create A→B extends (E2).
/// Update E2's relation to competes_with. The returned id must be E1, not E2.
/// A subsequent get(returned_id) must succeed.
#[tokio::test]
async fn update_edge_returns_surviving_canonical_id_on_conflict() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity(&tok, "concept", None, "SurvA", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "SurvB", None, None, vec![])
        .await
        .unwrap();

    // E1: canonical competes_with between A and B (runtime canonicalises order).
    let e1 = rt
        .link(&tok, a.id, b.id, EdgeRelation::CompetesWith, 1.0, None)
        .await
        .unwrap();

    // E2: non-symmetric extends edge, using the higher-uuid as source so that
    // updating to competes_with will trigger a flip (endpoints_flipped=true path).
    let (src, tgt) = if a.id > b.id {
        (a.id, b.id)
    } else {
        (b.id, a.id)
    };
    let e2 = rt
        .link(&tok, src, tgt, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();

    // E1 and E2 must be different edges.
    assert_ne!(
        e1.id, e2.id,
        "pre-condition: E1 and E2 must be distinct edges"
    );

    // Update E2 to competes_with → conflict with E1 must be absorbed.
    let returned = rt
        .update_edge(
            &tok,
            e2.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::CompetesWith),
                weight: Some(0.9),
                ..Default::default()
            },
        )
        .await
        .expect("update_edge must succeed even when conflict is absorbed");

    // Bug 1 assertion: returned id must be E1 (surviving canonical row), not E2 (deleted).
    assert_eq!(
        returned.id, e1.id,
        "Bug 1: update_edge must return the SURVIVING canonical row id (E1={:?}), \
         got E2={:?}",
        e1.id, returned.id
    );

    // get(returned.id) must succeed — it must not 404.
    let fetched = rt
        .get_edge(&tok, returned.id.into())
        .await
        .expect("get_edge on returned id must not error")
        .expect("get_edge on returned id must find a row (not 404)");
    assert_eq!(
        fetched.id, e1.id,
        "fetched row id must match E1 (surviving canonical)"
    );

    // E2 must no longer exist.
    let e2_lookup = rt
        .get_edge(&tok, e2.id.into())
        .await
        .expect("get_edge on deleted id must not error");
    assert!(
        e2_lookup.is_none(),
        "Bug 1: deleted edge E2 must not be findable after conflict absorption"
    );
}

/// Regression for Bug 2: when an edge's relation is updated to a symmetric relation
/// and the endpoints are ALREADY in canonical order (endpoints_flipped=false),
/// a pre-existing canonical row with the same natural key must still be detected and
/// absorbed — no UNIQUE-constraint error, no duplicate row.
///
/// Setup: ensure A < B (canonical order). Pre-create canonical A→B competes_with (E1).
/// Create A→B extends (E2, already canonical since A < B and extends is non-symmetric).
/// Update E2's relation to competes_with (endpoints_flipped=false because A < B).
/// Assert: exactly one live competes_with edge remains between A and B.
#[tokio::test]
async fn update_edge_canonical_orientation_conflict() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let a = rt
        .create_entity(&tok, "concept", None, "CanOrA", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "CanOrB", None, None, vec![])
        .await
        .unwrap();

    // Determine canonical order: canon_lo < canon_hi.
    let (canon_lo, canon_hi) = if a.id < b.id {
        (a.id, b.id)
    } else {
        (b.id, a.id)
    };

    // E1: canonical competes_with (lower → higher, which is canonical).
    let e1 = rt
        .link(
            &tok,
            canon_lo,
            canon_hi,
            EdgeRelation::CompetesWith,
            1.0,
            None,
        )
        .await
        .unwrap();

    // E2: extends in the same canonical direction (lower → higher).
    // endpoints_flipped will be false when we update to competes_with.
    let e2 = rt
        .link(&tok, canon_lo, canon_hi, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();

    assert_ne!(
        e1.id, e2.id,
        "pre-condition: E1 and E2 must be distinct edges"
    );

    // Update E2's relation to competes_with — must not produce UNIQUE-constraint error.
    // Bug 2: the non-flipped path used to call upsert_edge which only checked ON CONFLICT(id),
    // missing the natural-key duplicate with a different id.
    rt.update_edge(
        &tok,
        e2.id.into(),
        EdgePatch {
            relation: Some(EdgeRelation::CompetesWith),
            ..Default::default()
        },
    )
    .await
    .expect("Bug 2: update_edge on canonical-orientation conflict must not error");

    // Verify exactly one live competes_with edge exists between canon_lo and canon_hi.
    let edges = rt
        .list_edges(
            &tok,
            khive_runtime::EdgeListFilter {
                source_id: Some(canon_lo),
                target_id: Some(canon_hi),
                relations: vec![EdgeRelation::CompetesWith],
                ..Default::default()
            },
            100,
        )
        .await
        .expect("list_edges must succeed");

    assert_eq!(
        edges.len(),
        1,
        "Bug 2: exactly one competes_with edge must exist after non-flipped conflict absorption; \
         found {} edges: {edges:?}",
        edges.len()
    );
}

// =============================================================================
// Secret gate: structured-field bypass regression (#83 fix round)
// =============================================================================

#[tokio::test]
async fn entity_create_blocks_secret_in_properties() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    // A fake AWS key embedded in entity properties — must be blocked.
    let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
    let result = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "TestEntity",
            None,
            Some(props),
            vec![],
        )
        .await;
    assert!(
        result.is_err(),
        "entity create with secret in properties must be blocked"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

#[tokio::test]
async fn entity_create_blocks_secret_in_tags() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let tags = vec![
        "type:concept".to_string(),
        "AKIAFAKEKEY1234567890".to_string(),
    ];
    let result = rt
        .create_entity(&tok, "concept", None, "TestEntity", None, None, tags)
        .await;
    assert!(
        result.is_err(),
        "entity create with secret in tags must be blocked"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

#[tokio::test]
async fn note_create_blocks_secret_in_properties() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let props = serde_json::json!({ "api_key": "AKIAFAKEKEY1234567890" });
    let result = rt
        .create_note(
            &tok,
            "observation",
            None,
            "Safe content",
            None,
            Some(props),
            vec![],
        )
        .await;
    assert!(
        result.is_err(),
        "note create with secret in properties must be blocked"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

// Regression: pure-hex credential in trigger context must be blocked.
// Pure hex cannot reach entropy threshold (hex max 4.0 < 4.5), so the
// secret gate must detect it via the hex-credential-token path.  This
// test exercises the MCP-reachable write path (create_note → create_note_inner
// → secret_gate::check) to confirm persistence is blocked end-to-end.
#[tokio::test]
async fn note_create_blocks_hex_credential_in_content() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    // 32-char pure hex near the phrase "api key" in the note body.
    let content = "api key 4f9c2e8a1d3b5c7e9f0a2b4d6e8c0a2b"; // gitleaks:allow
    let result = rt
        .create_note(&tok, "observation", None, content, None, None, vec![])
        .await;
    assert!(
        result.is_err(),
        "note create with hex credential in content must be blocked; got Ok"
    );
    assert!(
        matches!(
            result.unwrap_err(),
            khive_runtime::RuntimeError::SecretDetected(_)
        ),
        "error must be SecretDetected"
    );
}

// =============================================================================
// EmbedderRegistry integration tests (#397)
// =============================================================================

mod embedder_registry_tests {
    use async_trait::async_trait;
    use khive_gate::AllowAllGate;
    use khive_runtime::{EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeError};
    use khive_types::Namespace;
    use lattice_embed::{EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    // ── MockEmbedderProvider ─────────────────────────────────────────────────

    /// A synthetic embedding provider that returns a fixed vector of `42.0` values.
    ///
    /// Used to verify that custom providers are reachable via
    /// `KhiveRuntime::embedder` after registration.
    struct MockEmbedderProvider {
        name: String,
        dims: usize,
    }

    impl MockEmbedderProvider {
        fn new(name: &str, dims: usize) -> Self {
            Self {
                name: name.to_owned(),
                dims,
            }
        }
    }

    struct MockEmbeddingService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for MockEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
            Ok(texts.iter().map(|_| vec![42.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "mock-embedding-service"
        }
    }

    #[async_trait]
    impl EmbedderProvider for MockEmbedderProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(MockEmbeddingService { dims: self.dims }))
        }
    }

    fn memory_rt_no_model() -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
        })
        .expect("in-memory runtime")
    }

    // ── Test: register + embedder round-trip ─────────────────────────────────

    #[tokio::test]
    async fn register_embedder_and_retrieve_via_embedder_method() {
        let rt = memory_rt_no_model();
        rt.register_embedder(MockEmbedderProvider::new("mock", 384));

        let service = rt
            .embedder("mock")
            .await
            .expect("embedder lookup must succeed after registration");

        let texts = vec!["hello world".to_string()];
        let vecs = service
            .embed(&texts, EmbeddingModel::AllMiniLmL6V2)
            .await
            .expect("mock service must embed successfully");

        assert_eq!(vecs.len(), 1);
        assert_eq!(vecs[0].len(), 384);
        assert!(
            vecs[0].iter().all(|&v| (v - 42.0_f32).abs() < 1e-6),
            "mock service must return constant 42.0 vector"
        );
    }

    // ── Test: registered names include custom provider ────────────────────────

    #[tokio::test]
    async fn registered_names_includes_custom_provider() {
        let rt = memory_rt_no_model();
        rt.register_embedder(MockEmbedderProvider::new("my-encoder", 128));

        let names = rt.registered_embedding_model_names();
        assert!(
            names.contains(&"my-encoder".to_string()),
            "registered_embedding_model_names must include custom provider 'my-encoder'; got {names:?}"
        );
    }

    // ── Test: dual-embedding regression — both MiniLM and paraphrase reachable ─

    #[tokio::test]
    async fn dual_embedding_regression_both_models_registered() {
        use khive_runtime::RuntimeConfig;
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: khive_runtime::BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
        })
        .expect("runtime with two models");

        let names = rt.registered_embedding_model_names();

        assert!(
            names.contains(&"all-minilm-l6-v2".to_string()),
            "MiniLM must be registered; names: {names:?}"
        );
        assert!(
            names.contains(&"paraphrase-multilingual-minilm-l12-v2".to_string()),
            "paraphrase must be registered; names: {names:?}"
        );

        // Verify resolve_embedding_model works for both.
        rt.resolve_embedding_model(Some("all-minilm-l6-v2"))
            .expect("MiniLM must resolve");
        rt.resolve_embedding_model(Some("paraphrase"))
            .expect("paraphrase alias must resolve");
    }

    // ── Test: unknown embedder returns UnknownModel ───────────────────────────

    #[tokio::test]
    async fn embedder_unknown_name_returns_error() {
        let rt = memory_rt_no_model();
        let err = rt
            .embedder("no-such-model")
            .await
            .err()
            .expect("expected Err for unknown embedder name, got Ok");
        assert!(
            matches!(err, RuntimeError::UnknownModel(ref n) if n == "no-such-model"),
            "expected UnknownModel for unregistered name; got {err:?}"
        );
    }

    // ── Test: custom provider registered via pack hook is reachable end-to-end ─
    //
    // This is the integration counterpart to the unit tests in
    // `embedder_registry.rs`. It verifies the full stack: a pack overrides
    // `register_embedders`, the transport calls `VerbRegistry::call_register_embedders`,
    // and the custom provider can be resolved and used via `rt.embedder(name)`.

    #[tokio::test]
    async fn pack_register_embedders_hook_makes_provider_reachable() {
        use async_trait::async_trait;
        use khive_runtime::pack::HandlerDef;
        use khive_runtime::NamespaceToken;
        use khive_runtime::{PackRuntime, VerbRegistry, VerbRegistryBuilder};
        use khive_types::Pack;
        use serde_json::Value;

        struct EmbedderPack;

        impl Pack for EmbedderPack {
            const NAME: &'static str = "embedder-test-pack";
            const NOTE_KINDS: &'static [&'static str] = &[];
            const ENTITY_KINDS: &'static [&'static str] = &[];
            const HANDLERS: &'static [HandlerDef] = &[];
        }

        #[async_trait]
        impl PackRuntime for EmbedderPack {
            fn name(&self) -> &str {
                Self::NAME
            }
            fn note_kinds(&self) -> &'static [&'static str] {
                Self::NOTE_KINDS
            }
            fn entity_kinds(&self) -> &'static [&'static str] {
                Self::ENTITY_KINDS
            }
            fn handlers(&self) -> &'static [HandlerDef] {
                Self::HANDLERS
            }
            fn register_embedders(&self, runtime: &KhiveRuntime) {
                runtime.register_embedder(MockEmbedderProvider::new("pack-custom-encoder", 256));
            }
            async fn dispatch(
                &self,
                _verb: &str,
                _params: Value,
                _registry: &VerbRegistry,
                _token: &NamespaceToken,
            ) -> Result<Value, khive_runtime::RuntimeError> {
                Ok(Value::Null)
            }
        }

        let rt = memory_rt_no_model();
        // Simulate what the transport does: build the registry, then call the hook.
        let mut builder = VerbRegistryBuilder::new();
        builder.register(EmbedderPack);
        let registry = builder.build().expect("registry builds");
        registry.call_register_embedders(&rt);

        // After the hook fires, the custom provider must be reachable.
        let service = rt
            .embedder("pack-custom-encoder")
            .await
            .expect("pack-contributed provider must be reachable after call_register_embedders");

        let texts = vec!["test sentence".to_string()];
        let vecs = service
            .embed(&texts, EmbeddingModel::AllMiniLmL6V2)
            .await
            .expect("custom service must embed without error");
        assert_eq!(vecs.len(), 1);
        assert_eq!(
            vecs[0].len(),
            256,
            "dims must match provider declaration (256)"
        );
    }

    // ── Test: failing provider build() returns Err instead of panicking ───────

    #[tokio::test]
    async fn failing_provider_build_returns_err_not_panic() {
        struct FailingProvider;

        #[async_trait]
        impl EmbedderProvider for FailingProvider {
            fn name(&self) -> &str {
                "failing-provider"
            }
            fn dimensions(&self) -> usize {
                128
            }
            async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
                Err(RuntimeError::Internal(
                    "simulated provider construction failure".into(),
                ))
            }
        }

        let rt = memory_rt_no_model();
        rt.register_embedder(FailingProvider);

        let result = rt.embedder("failing-provider").await;
        assert!(
            result.is_err(),
            "embedder() must return Err when build() fails, not panic; got Ok"
        );
        let err = result.err().expect("checked above");
        let msg = err.to_string();
        assert!(
            msg.contains("simulated provider construction failure")
                || msg.contains("build() failed")
                || msg.contains("Internal"),
            "error must carry build failure context; got: {msg}"
        );
    }
}

// =============================================================================
// Epistemic endpoint tests (ADR-055 Phase 2+3)
// =============================================================================

// --- Entity→Entity ACCEPT cases ---

/// Concept→Concept supports: base allowlist row.
#[tokio::test]
async fn link_concept_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let a = rt
        .create_entity(&tok, "concept", None, "Finding A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "Claim B", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, a.id, b.id, EdgeRelation::Supports, 0.8, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
    assert_eq!(edge.source_id, a.id);
    assert_eq!(edge.target_id, b.id);
}

/// Document→Concept supports: base allowlist row.
#[tokio::test]
async fn link_document_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let doc = rt
        .create_entity(&tok, "document", None, "Paper X", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Hypothesis Y", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, doc.id, claim.id, EdgeRelation::Supports, 0.9, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
}

/// Concept→Concept refutes: base allowlist row.
#[tokio::test]
async fn link_concept_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let a = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Counter-evidence",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let b = rt
        .create_entity(&tok, "concept", None, "Claim B", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, a.id, b.id, EdgeRelation::Refutes, 0.7, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

/// Document→Concept refutes: base allowlist row.
#[tokio::test]
async fn link_document_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let doc = rt
        .create_entity(&tok, "document", None, "Negative study", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Claim C", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, doc.id, claim.id, EdgeRelation::Refutes, 0.85, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

// --- Note→Note ACCEPT cases ---

/// Note→Note supports: same substrate, any note kind allowed.
#[tokio::test]
async fn link_note_note_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let finding = rt
        .create_note(
            &tok,
            "observation",
            Some("Finding note"),
            "experiment shows positive result",
            Some(0.8),
            None,
            vec![],
        )
        .await
        .unwrap();
    let claim = rt
        .create_note(
            &tok,
            "question",
            Some("Claim note"),
            "does intervention work?",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    let edge = rt
        .link(
            &tok,
            finding.id,
            claim.id,
            EdgeRelation::Supports,
            0.9,
            None,
        )
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
    assert_eq!(edge.source_id, finding.id);
    assert_eq!(edge.target_id, claim.id);
}

/// Note→Note refutes: same substrate allowed.
#[tokio::test]
async fn link_note_note_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let counter = rt
        .create_note(
            &tok,
            "observation",
            Some("Counter finding"),
            "null result from replication",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .unwrap();
    let hypothesis = rt
        .create_note(
            &tok,
            "insight",
            Some("Hypothesis"),
            "the intervention increases outcome",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    let edge = rt
        .link(
            &tok,
            counter.id,
            hypothesis.id,
            EdgeRelation::Refutes,
            0.75,
            None,
        )
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

// --- Cross-substrate REJECT cases ---

/// Note→Entity supports: cross-substrate, must error.
#[tokio::test]
async fn link_note_entity_supports_rejected() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "finding note",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let entity = rt
        .create_entity(&tok, "concept", None, "Some concept", None, None, vec![])
        .await
        .unwrap();
    let result = rt
        .link(&tok, note.id, entity.id, EdgeRelation::Supports, 0.8, None)
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "note→entity supports must be rejected (cross-substrate); got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}

/// Entity→Note refutes: cross-substrate, must error.
#[tokio::test]
async fn link_entity_note_refutes_rejected() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity(&tok, "concept", None, "A concept", None, None, vec![])
        .await
        .unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "some note",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let result = rt
        .link(&tok, entity.id, note.id, EdgeRelation::Refutes, 0.5, None)
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "entity→note refutes must be rejected (cross-substrate); got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("refutes"),
        "error message must name the relation 'refutes'; got: {msg}"
    );
}

// --- Disallowed entity pair REJECT case ---

/// Person→Concept supports: not in base allowlist, must error naming the relation.
#[tokio::test]
async fn link_person_concept_supports_rejected_with_relation_name() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let person = rt
        .create_entity(&tok, "person", None, "Researcher A", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Hypothesis Z", None, None, vec![])
        .await
        .unwrap();
    let result = rt
        .link(&tok, person.id, claim.id, EdgeRelation::Supports, 0.5, None)
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "person→concept supports is not in base allowlist; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}

// --- Remaining allowlist source kinds ---

/// Dataset→Concept supports: base allowlist row (previously untested source kind).
#[tokio::test]
async fn link_dataset_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ds = rt
        .create_entity(&tok, "dataset", None, "Bench-X", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Hypothesis Q", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, ds.id, claim.id, EdgeRelation::Supports, 0.8, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
}

/// Artifact→Concept refutes: base allowlist row (previously untested source kind).
#[tokio::test]
async fn link_artifact_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let art = rt
        .create_entity(&tok, "artifact", None, "Checkpoint-v1", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Claim R", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, art.id, claim.id, EdgeRelation::Refutes, 0.7, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
}

/// Artifact→Concept supports: base allowlist row (previously untested combination).
#[tokio::test]
async fn link_artifact_concept_supports_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let art = rt
        .create_entity(&tok, "artifact", None, "Checkpoint-v2", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Claim T", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, art.id, claim.id, EdgeRelation::Supports, 0.8, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Supports);
    assert_eq!(edge.source_id, art.id);
    assert_eq!(edge.target_id, claim.id);
}

/// Dataset→Concept refutes: base allowlist row (previously untested combination).
#[tokio::test]
async fn link_dataset_concept_refutes_accepted() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let ds = rt
        .create_entity(&tok, "dataset", None, "Bench-Y", None, None, vec![])
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Hypothesis W", None, None, vec![])
        .await
        .unwrap();
    let edge = rt
        .link(&tok, ds.id, claim.id, EdgeRelation::Refutes, 0.75, None)
        .await
        .unwrap();
    assert_eq!(edge.relation, EdgeRelation::Refutes);
    assert_eq!(edge.source_id, ds.id);
    assert_eq!(edge.target_id, claim.id);
}

// --- update_edge parity tests ---

/// (a) update_edge legal entity edge → Supports on allowlist pair: accepted.
/// Uses concept→concept: start with Extends, update to Supports.
#[tokio::test]
async fn update_edge_to_supports_on_legal_entity_pair_accepted() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let evidence = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "Evidence concept",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    let claim = rt
        .create_entity(&tok, "concept", None, "Hypothesis H", None, None, vec![])
        .await
        .unwrap();
    // Start with Extends (legal for concept→concept).
    let edge = rt
        .link(
            &tok,
            evidence.id,
            claim.id,
            EdgeRelation::Extends,
            0.9,
            None,
        )
        .await
        .unwrap();
    // Update the relation to Supports — concept→concept is in the Supports allowlist.
    let updated = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Supports),
                ..Default::default()
            },
        )
        .await
        .expect("update_edge to supports on concept→concept must be accepted");
    assert_eq!(updated.relation, EdgeRelation::Supports);
}

/// (b) update_edge entity edge → Supports on off-allowlist pair: rejected.
#[tokio::test]
async fn update_edge_to_supports_on_disallowed_entity_pair_rejected() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let person = rt
        .create_entity(&tok, "person", None, "Researcher B", None, None, vec![])
        .await
        .unwrap();
    let concept = rt
        .create_entity(&tok, "concept", None, "Claim S", None, None, vec![])
        .await
        .unwrap();
    // Person→Concept with a relation that IS legal to start (introduced_by is
    // illegal for person→concept too — use enables which IS legal for person
    // is also illegal, use instance_of which allows *→concept).
    // Simplest: use instance_of (valid for *→concept) to create the edge first.
    let edge = rt
        .link(
            &tok,
            person.id,
            concept.id,
            EdgeRelation::InstanceOf,
            1.0,
            None,
        )
        .await
        .unwrap();
    // Now update to Supports — person is not in the allowlist for supports.
    let result = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Supports),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "update_edge to supports on person→concept must be rejected; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}

/// (c) update_edge note→entity annotates edge → Supports: rejected (cross-substrate).
#[tokio::test]
async fn update_edge_annotates_to_supports_rejected_cross_substrate() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let entity = rt
        .create_entity(&tok, "concept", None, "Target concept", None, None, vec![])
        .await
        .unwrap();
    let note = rt
        .create_note(
            &tok,
            "observation",
            None,
            "some observation",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    // Create note→entity annotates edge (the only legal cross-substrate relation).
    let edge = rt
        .link(&tok, note.id, entity.id, EdgeRelation::Annotates, 1.0, None)
        .await
        .unwrap();
    // Update to Supports → must fail (note→entity is cross-substrate for supports).
    let result = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Supports),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(result, Err(khive_runtime::RuntimeError::InvalidInput(_))),
        "update_edge note→entity annotates → supports must be rejected; got {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("supports"),
        "error message must name the relation 'supports'; got: {msg}"
    );
}

/// (d) update_edge note→note edge → Refutes: accepted (same substrate).
#[tokio::test]
async fn update_edge_note_note_to_refutes_accepted() {
    use khive_runtime::EdgePatch;

    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note_a = rt
        .create_note(
            &tok,
            "observation",
            None,
            "prior finding",
            Some(0.6),
            None,
            vec![],
        )
        .await
        .unwrap();
    let note_b = rt
        .create_note(
            &tok,
            "insight",
            None,
            "derived claim",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    // Create a note→note edge with Supports first.
    let edge = rt
        .link(
            &tok,
            note_a.id,
            note_b.id,
            EdgeRelation::Supports,
            0.8,
            None,
        )
        .await
        .unwrap();
    // Update to Refutes — note→note same-substrate, must be accepted.
    let updated = rt
        .update_edge(
            &tok,
            edge.id.into(),
            EdgePatch {
                relation: Some(EdgeRelation::Refutes),
                ..Default::default()
            },
        )
        .await
        .expect("update_edge note→note supports → refutes must be accepted");
    assert_eq!(updated.relation, EdgeRelation::Refutes);
}

// =============================================================================
// Multi-namespace read visibility (visible-set tokens)
// =============================================================================

/// ADR-007 PR-A1: visible-set enforcement on by-ID ops is removed.
/// list_entities / list_notes still filter by visible_namespaces (PR-B collapses that).
/// get_entity and get_note_including_deleted now return any record by UUID regardless
/// of the token's visible set.  Writes still land in the primary namespace only.
#[tokio::test]
async fn visible_set_reads_primary_and_extra_not_third() {
    let rt = rt();

    // Mint single-namespace write tokens for three isolated namespaces.
    let tok_a = rt.authorize(Namespace::parse("vis-a").unwrap()).unwrap();
    let tok_b = rt.authorize(Namespace::parse("vis-b").unwrap()).unwrap();
    let tok_c = rt.authorize(Namespace::parse("vis-c").unwrap()).unwrap();

    // Write one entity and one note in each namespace.
    let entity_a = rt
        .create_entity(&tok_a, "concept", None, "EntityA", None, None, vec![])
        .await
        .unwrap();
    let entity_b = rt
        .create_entity(&tok_b, "concept", None, "EntityB", None, None, vec![])
        .await
        .unwrap();
    let entity_c = rt
        .create_entity(&tok_c, "concept", None, "EntityC", None, None, vec![])
        .await
        .unwrap();

    let note_a = rt
        .create_note(&tok_a, "observation", None, "NoteA", None, None, vec![])
        .await
        .unwrap();
    let note_b = rt
        .create_note(&tok_b, "observation", None, "NoteB", None, None, vec![])
        .await
        .unwrap();
    let note_c = rt
        .create_note(&tok_c, "observation", None, "NoteC", None, None, vec![])
        .await
        .unwrap();

    // Mint a visible-set token: primary=vis-a, visible=[vis-a, vis-b].
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("vis-a").unwrap(),
            vec![Namespace::parse("vis-b").unwrap()],
        )
        .unwrap();

    // --- list_entities sees a+b, not c ---
    let visible_entities = rt.list_entities(&vis_tok, None, None, 50, 0).await.unwrap();
    let entity_names: Vec<&str> = visible_entities.iter().map(|e| e.name.as_str()).collect();
    assert!(entity_names.contains(&"EntityA"), "EntityA must be visible");
    assert!(entity_names.contains(&"EntityB"), "EntityB must be visible");
    assert!(
        !entity_names.contains(&"EntityC"),
        "EntityC must NOT be visible"
    );

    // --- list_notes sees a+b, not c ---
    let visible_notes = rt.list_notes(&vis_tok, None, 50, 0).await.unwrap();
    let note_contents: Vec<&str> = visible_notes.iter().map(|n| n.content.as_str()).collect();
    assert!(note_contents.contains(&"NoteA"), "NoteA must be visible");
    assert!(note_contents.contains(&"NoteB"), "NoteB must be visible");
    assert!(
        !note_contents.contains(&"NoteC"),
        "NoteC must NOT be visible"
    );

    // --- get_entity: all three succeed by UUID (PR-A1: visible-set gate removed) ---
    rt.get_entity(&vis_tok, entity_a.id)
        .await
        .expect("get entity_a must succeed");
    rt.get_entity(&vis_tok, entity_b.id)
        .await
        .expect("get entity_b (visible non-primary) must succeed");
    rt.get_entity(&vis_tok, entity_c.id)
        .await
        .expect("get entity_c by UUID succeeds — visible-set gate removed in PR-A1");

    // --- get_note: all three returned by UUID (PR-A1: visible-set gate removed) ---
    let fetched_note_a = rt
        .get_note_including_deleted(&vis_tok, note_a.id)
        .await
        .expect("call must not error");
    assert!(
        fetched_note_a.is_some(),
        "note_a (primary namespace) must be returned"
    );

    let fetched_note_b = rt
        .get_note_including_deleted(&vis_tok, note_b.id)
        .await
        .expect("call must not error");
    assert!(
        fetched_note_b.is_some(),
        "note_b (visible non-primary) must be returned"
    );

    let fetched_note_c = rt
        .get_note_including_deleted(&vis_tok, note_c.id)
        .await
        .expect("call must not error");
    // PR-A1: by-ID get returns the note regardless of visible set (list_notes still filters — PR-B).
    assert!(
        fetched_note_c.is_some(),
        "note_c (outside visible set) must be returned by UUID via PR-A1 by-ID contract"
    );
    assert_eq!(
        fetched_note_c.as_ref().unwrap().namespace.as_str(),
        "vis-c",
        "fetched note_c must preserve its stored namespace"
    );

    // --- WRITE via vis_tok lands in primary (vis-a) only, not in vis-b ---
    let written = rt
        .create_entity(
            &vis_tok,
            "concept",
            None,
            "WrittenViaVisToken",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        written.namespace.as_str(),
        "vis-a",
        "write must stamp primary namespace, not any extra-visible one"
    );

    // Verify vis-b does not contain the newly written entity.
    let b_entities = rt.list_entities(&tok_b, None, None, 50, 0).await.unwrap();
    let b_names: Vec<&str> = b_entities.iter().map(|e| e.name.as_str()).collect();
    assert!(
        !b_names.contains(&"WrittenViaVisToken"),
        "write must NOT appear in vis-b"
    );

    // Suppress unused-variable warnings for IDs we intentionally only inserted.
    let _ = note_a;
    let _ = note_c;
    let _ = entity_c;
}

/// Backward compatibility: a token minted via `authorize()` (no visibility)
/// behaves exactly as before — single namespace, strict equality on reads/writes.
/// This is the original namespace_isolation test reproduced verbatim to confirm
/// nothing regressed.
#[tokio::test]
async fn namespace_isolation_backward_compat() {
    let rt = rt();
    let ns_a_tok = rt.authorize(Namespace::parse("bc-a").unwrap()).unwrap();
    let ns_b_tok = rt.authorize(Namespace::parse("bc-b").unwrap()).unwrap();

    rt.create_entity(&ns_a_tok, "concept", None, "EntityA", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(&ns_b_tok, "concept", None, "EntityB", None, None, vec![])
        .await
        .unwrap();

    let a_entities = rt
        .list_entities(&ns_a_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(a_entities.len(), 1);
    assert_eq!(a_entities[0].name, "EntityA");

    let b_entities = rt
        .list_entities(&ns_b_tok, None, None, 50, 0)
        .await
        .unwrap();
    assert_eq!(b_entities.len(), 1);
    assert_eq!(b_entities[0].name, "EntityB");
}

// =============================================================================
// Fix 4: visible-set token invariants (primary always included, no duplicates)
// =============================================================================

/// No extra-visible namespaces → visible set contains only the primary.
#[test]
fn mint_with_visibility_empty_extra_yields_primary_only() {
    let rt = rt();
    let tok = rt
        .authorize_with_visibility(Namespace::parse("ns-primary-only").unwrap(), vec![])
        .unwrap();

    let vis = tok.visible_namespaces();
    assert_eq!(vis.len(), 1, "primary only when no extras given");
    assert_eq!(vis[0].as_str(), "ns-primary-only");
    assert_eq!(tok.namespace().as_str(), "ns-primary-only");
}

/// When the primary is repeated in the extra list it must not appear twice.
#[test]
fn mint_with_visibility_deduplicates_primary_in_extras() {
    let rt = rt();
    let tok = rt
        .authorize_with_visibility(
            Namespace::parse("ns-dedup").unwrap(),
            vec![
                Namespace::parse("ns-dedup").unwrap(),
                Namespace::parse("ns-extra").unwrap(),
            ],
        )
        .unwrap();

    let vis = tok.visible_namespaces();
    assert_eq!(vis.len(), 2, "primary counted once, one distinct extra");
    assert_eq!(vis[0].as_str(), "ns-dedup");
    assert_eq!(vis[1].as_str(), "ns-extra");
}

// =============================================================================
// Fix 1: mutations confined to primary namespace; reads use visible set
// =============================================================================

/// A note written into an extra-visible namespace can be read back through
/// the visible-set token (resolve uses the visible set for notes).
#[tokio::test]
async fn resolve_uses_visible_set_for_note_in_extra_namespace() {
    let rt = rt();
    let _tok_a = rt.authorize(Namespace::parse("res-a").unwrap()).unwrap();
    let tok_b = rt.authorize(Namespace::parse("res-b").unwrap()).unwrap();

    let note_b = rt
        .create_note(&tok_b, "observation", None, "NoteInB", None, None, vec![])
        .await
        .unwrap();

    // visible-set token: primary=res-a, sees res-b too.
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("res-a").unwrap(),
            vec![Namespace::parse("res-b").unwrap()],
        )
        .unwrap();

    // get_note_including_deleted uses resolve() which should honour visible set.
    let fetched = rt
        .get_note_including_deleted(&vis_tok, note_b.id)
        .await
        .expect("call must not error");
    assert!(
        fetched.is_some(),
        "note in extra-visible namespace must be readable via visible-set token"
    );
    assert_eq!(fetched.unwrap().content, "NoteInB");
}

/// A link whose target lives in the extra-visible (but not primary) namespace
/// must be rejected — mutation endpoints must both be in the primary namespace.
#[tokio::test]
async fn link_refuses_target_in_visible_but_not_primary_namespace() {
    let rt = rt();
    let tok_a = rt
        .authorize(Namespace::parse("link-mut-a").unwrap())
        .unwrap();
    let tok_b = rt
        .authorize(Namespace::parse("link-mut-b").unwrap())
        .unwrap();

    let entity_a = rt
        .create_entity(&tok_a, "concept", None, "SrcEntity", None, None, vec![])
        .await
        .unwrap();
    let entity_b = rt
        .create_entity(&tok_b, "concept", None, "TgtEntity", None, None, vec![])
        .await
        .unwrap();

    // primary=link-mut-a, visible=[link-mut-a, link-mut-b].
    // entity_b lives in link-mut-b (visible, not primary).
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("link-mut-a").unwrap(),
            vec![Namespace::parse("link-mut-b").unwrap()],
        )
        .unwrap();

    let result = rt
        .link(
            &vis_tok,
            entity_a.id,
            entity_b.id,
            EdgeRelation::Extends,
            1.0,
            None,
        )
        .await;
    assert!(
        result.is_err(),
        "link with target in visible-only namespace must be rejected by mutation endpoint validation"
    );
}

/// An annotates note whose annotated target lives in the extra-visible (but not
/// primary) namespace must be rejected by create_note's mutation gate.
#[tokio::test]
async fn create_note_annotates_refuses_target_in_visible_only_namespace() {
    let rt = rt();
    let _tok_a = rt
        .authorize(Namespace::parse("ann-mut-a").unwrap())
        .unwrap();
    let tok_b = rt
        .authorize(Namespace::parse("ann-mut-b").unwrap())
        .unwrap();

    let entity_b = rt
        .create_entity(&tok_b, "concept", None, "AnnotTarget", None, None, vec![])
        .await
        .unwrap();

    // primary=ann-mut-a, visible=[ann-mut-a, ann-mut-b].
    // entity_b lives in ann-mut-b (visible, not primary).
    let vis_tok = rt
        .authorize_with_visibility(
            Namespace::parse("ann-mut-a").unwrap(),
            vec![Namespace::parse("ann-mut-b").unwrap()],
        )
        .unwrap();

    let result = rt
        .create_note(
            &vis_tok,
            "observation",
            None,
            "AnnotNote",
            None,
            None,
            vec![entity_b.id],
        )
        .await;
    assert!(
        result.is_err(),
        "annotates with target in visible-only namespace must be rejected"
    );
}

// =============================================================================
// Finding 5: hybrid_search cross-namespace Option B limitation documented + tested
// =============================================================================

/// Documents the Phase 1.5 limitation: `hybrid_search` is primary-namespace-only
/// for both the FTS and vector legs (each namespace owns its own FTS table and
/// ANN index; cross-namespace fanout is deferred to Phase 1.5).
///
/// This test verifies the actual current behavior:
/// - The primary-namespace entity appears in results.
/// - The extra-namespace entity does NOT appear in results (its FTS data lives
///   in `fts_entities_{extra-ns}`, a separate table not queried here).
///
/// A caller with a visible set can READ the extra-namespace entity directly via
/// `get_entity`, but `hybrid_search` does not surface it today.
#[tokio::test]
async fn hybrid_search_is_primary_namespace_only_phase1_5_limitation() {
    let rt = rt();

    let ns_primary = Namespace::parse("hs-primary-ns").unwrap();
    let ns_extra = Namespace::parse("hs-extra-ns").unwrap();

    let tok_primary = rt.authorize(ns_primary.clone()).unwrap();
    let tok_extra = rt.authorize(ns_extra.clone()).unwrap();

    // Create an entity in primary namespace with a distinctive term.
    let entity_in_primary = rt
        .create_entity(
            &tok_primary,
            "concept",
            None,
            "StellarPrimary",
            Some("unique stellar primary concept"),
            None,
            vec![],
        )
        .await
        .unwrap();

    // Create an entity in the extra namespace with the same distinctive term.
    let entity_in_extra = rt
        .create_entity(
            &tok_extra,
            "concept",
            None,
            "StellarExtra",
            Some("unique stellar extra concept"),
            None,
            vec![],
        )
        .await
        .unwrap();

    // Visible-set token: primary = hs-primary-ns, also sees hs-extra-ns.
    let vis_tok = rt
        .authorize_with_visibility(ns_primary.clone(), vec![ns_extra.clone()])
        .unwrap();

    // Search: FTS-only (no embedding model in test runtime).
    // Current behavior: only primary-namespace results surface.
    let hits = rt
        .hybrid_search(&vis_tok, "stellar", None, 20, None, None)
        .await
        .unwrap();

    let hit_ids: Vec<Uuid> = hits.iter().map(|h| h.entity_id).collect();

    // Primary entity must surface.
    assert!(
        hit_ids.contains(&entity_in_primary.id),
        "hybrid_search must return entity from primary namespace; \
         expected entity_id={}, got: {hit_ids:?}",
        entity_in_primary.id,
    );

    // Extra-namespace entity does NOT surface — Phase 1.5 limitation.
    // Each namespace has its own FTS table; cross-namespace FTS fanout is deferred.
    assert!(
        !hit_ids.contains(&entity_in_extra.id),
        "hybrid_search must NOT return entity from extra (visible-only) namespace \
         until Phase 1.5 cross-namespace fanout ships; \
         entity_id={} unexpectedly appeared in: {hit_ids:?}",
        entity_in_extra.id,
    );

    // Direct read of the extra-namespace entity via get_entity must still work
    // (this proves the visible set wiring is correct — only search is primary-scoped).
    let fetched = rt
        .get_entity(&vis_tok, entity_in_extra.id)
        .await
        .expect("get_entity via visible-set token must return extra-namespace entity");
    assert_eq!(
        fetched.id, entity_in_extra.id,
        "visible-set read of extra-namespace entity must succeed"
    );
}

// =============================================================================
// PR-A1: cross-namespace note by-ID operations (update_note / delete_note)
// =============================================================================

/// update_note via a foreign-namespace token must succeed (PR-A1).
/// Non-vacuity: this test FAILS if the old visible-set guard is restored.
#[tokio::test]
async fn update_note_cross_namespace_succeeds() {
    use khive_runtime::NotePatch;

    let rt = rt();
    let tok_a = rt
        .authorize(Namespace::parse("note-ns-a").unwrap())
        .unwrap();
    let tok_b = rt
        .authorize(Namespace::parse("note-ns-b").unwrap())
        .unwrap();

    let note = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "original content",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(note.namespace.as_str(), "note-ns-a");

    // Update from a different token — must succeed.
    let patch = NotePatch::new(None, Some("updated content".to_string()), None, None, None);
    let updated = rt.update_note(&tok_b, note.id, patch).await;
    assert!(
        updated.is_ok(),
        "update_note from foreign token must succeed; got {:?}",
        updated
    );
    let updated = updated.unwrap();
    assert_eq!(updated.content, "updated content");
    // Namespace on the record must NOT change to tok_b's namespace.
    assert_eq!(
        updated.namespace.as_str(),
        "note-ns-a",
        "namespace must remain the record's stored namespace after cross-ns update"
    );
}

/// delete_note (soft and hard) via a foreign-namespace token must succeed (PR-A1).
/// Non-vacuity: this test FAILS if the old ensure_namespace guard is restored.
#[tokio::test]
async fn delete_note_cross_namespace_succeeds() {
    let rt = rt();
    let tok_a = rt.authorize(Namespace::parse("del-ns-a").unwrap()).unwrap();
    let tok_b = rt.authorize(Namespace::parse("del-ns-b").unwrap()).unwrap();

    // --- soft delete from foreign token ---
    let note_soft = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "soft target",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let soft_result = rt.delete_note(&tok_b, note_soft.id, false).await;
    assert!(
        soft_result.unwrap(),
        "cross-namespace soft delete_note must return true"
    );
    // Confirm gone via live query.
    let after_soft = rt
        .get_note_including_deleted(&tok_a, note_soft.id)
        .await
        .unwrap();
    assert!(
        after_soft.is_some(),
        "soft-deleted note must still appear in including_deleted"
    );

    // --- hard delete from foreign token ---
    let note_hard = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "hard target",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .unwrap();
    let hard_result = rt.delete_note(&tok_b, note_hard.id, true).await;
    assert!(
        hard_result.unwrap(),
        "cross-namespace hard delete_note must return true"
    );
    let after_hard = rt
        .get_note_including_deleted(&tok_a, note_hard.id)
        .await
        .unwrap();
    assert!(
        after_hard.is_none(),
        "hard-deleted note must not appear even via including_deleted"
    );
}
