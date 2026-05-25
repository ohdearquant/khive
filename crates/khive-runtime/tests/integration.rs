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
    let tok = rt.authorize(Namespace::local());

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
    let research_tok = rt.authorize(Namespace::parse("research").unwrap());

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
    let tok = rt.authorize(Namespace::local());

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
    let tok = rt.authorize(Namespace::local());

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
    let tok = rt.authorize(Namespace::local());

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
    let tok = rt.authorize(Namespace::local());

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
    let tok = rt.authorize(Namespace::local());

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
    let tok = rt.authorize(Namespace::local());

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
    let tok = rt.authorize(Namespace::local());
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
    let tok = rt.authorize(Namespace::local());

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
    let ns_a_tok = rt.authorize(Namespace::parse("ns-a").unwrap());
    let ns_b_tok = rt.authorize(Namespace::parse("ns-b").unwrap());

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
    let tok = rt.authorize(Namespace::local());
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
    let tok = rt.authorize(Namespace::local());
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
    let tok = rt.authorize(Namespace::local());
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
    let tok = rt.authorize(Namespace::local());
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
    let tok = rt.authorize(Namespace::local());
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
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let tok = rt.authorize(Namespace::local());
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
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let tok = rt.authorize(Namespace::local());
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
    let tok = rt.authorize(Namespace::local());
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
