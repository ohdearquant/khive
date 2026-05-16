//! Integration tests for khive-runtime.
//!
//! Tests cover entity CRUD, graph operations, note memory, GQL query,
//! and namespace isolation using an in-memory runtime.

use khive_runtime::{KhiveRuntime, NoteKind, RuntimeConfig};
use khive_storage::types::{Direction, TraversalOptions, TraversalRequest};
use khive_storage::EdgeRelation;
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

    let entity = rt
        .create_entity(
            None,
            "concept",
            "LoRA",
            Some("Low-Rank Adaptation"),
            None,
            vec![],
        )
        .await
        .unwrap();

    let fetched = rt.get_entity(None, entity.id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, entity.id);
    assert_eq!(fetched.name, "LoRA");
    assert_eq!(fetched.kind, khive_types::EntityKind::Concept);
    assert_eq!(fetched.description.as_deref(), Some("Low-Rank Adaptation"));
}

#[tokio::test]
async fn entity_create_with_properties_and_tags() {
    let rt = rt();

    let props = serde_json::json!({"domain": "fine-tuning", "type": "technique"});
    let entity = rt
        .create_entity(
            Some("research"),
            "concept",
            "QLoRA",
            Some("Quantized LoRA"),
            Some(props.clone()),
            vec!["fine-tuning".to_string(), "quantization".to_string()],
        )
        .await
        .unwrap();

    let fetched = rt
        .get_entity(Some("research"), entity.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched.properties, Some(props));
    assert_eq!(fetched.tags, vec!["fine-tuning", "quantization"]);
}

#[tokio::test]
async fn entity_list_by_kind() {
    let rt = rt();

    rt.create_entity(None, "concept", "FlashAttention", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(None, "concept", "GQA", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(
        None,
        "document",
        "Attention Is All You Need",
        None,
        None,
        vec![],
    )
    .await
    .unwrap();

    let concepts = rt.list_entities(None, Some("concept"), 50).await.unwrap();
    assert_eq!(concepts.len(), 2);
    assert!(concepts.iter().any(|e| e.name == "FlashAttention"));
    assert!(concepts.iter().any(|e| e.name == "GQA"));

    let docs = rt.list_entities(None, Some("document"), 50).await.unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "Attention Is All You Need");

    let all = rt.list_entities(None, None, 50).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn entity_delete_soft() {
    let rt = rt();

    let entity = rt
        .create_entity(None, "concept", "to-delete", None, None, vec![])
        .await
        .unwrap();

    let deleted = rt.delete_entity(None, entity.id, false).await.unwrap();
    assert!(deleted);

    let fetched = rt.get_entity(None, entity.id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn entity_count_by_kind() {
    let rt = rt();

    for _ in 0..3 {
        rt.create_entity(None, "concept", "concept-X", None, None, vec![])
            .await
            .unwrap();
    }
    for _ in 0..2 {
        rt.create_entity(None, "document", "doc-Y", None, None, vec![])
            .await
            .unwrap();
    }

    let concept_count = rt.count_entities(None, Some("concept")).await.unwrap();
    let doc_count = rt.count_entities(None, Some("document")).await.unwrap();
    let total = rt.count_entities(None, None).await.unwrap();

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

    let lora = rt
        .create_entity(None, "concept", "LoRA", None, None, vec![])
        .await
        .unwrap();
    let qlora = rt
        .create_entity(None, "concept", "QLoRA", None, None, vec![])
        .await
        .unwrap();

    rt.link(None, qlora.id, lora.id, EdgeRelation::VariantOf, 1.0)
        .await
        .unwrap();

    let hits = rt
        .neighbors(None, qlora.id, Direction::Out, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node_id, lora.id);
    assert_eq!(hits[0].relation, EdgeRelation::VariantOf);
}

#[tokio::test]
async fn traverse_multi_hop() {
    let rt = rt();

    let a = rt
        .create_entity(None, "concept", "A", None, None, vec![])
        .await
        .unwrap();
    let b = rt
        .create_entity(None, "concept", "B", None, None, vec![])
        .await
        .unwrap();
    let c = rt
        .create_entity(None, "concept", "C", None, None, vec![])
        .await
        .unwrap();

    rt.link(None, a.id, b.id, EdgeRelation::Extends, 1.0)
        .await
        .unwrap();
    rt.link(None, b.id, c.id, EdgeRelation::Extends, 1.0)
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

    let paths = rt.traverse(None, request).await.unwrap();
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

    rt.create_note(
        None,
        NoteKind::Observation,
        "LoRA is a fine-tuning technique",
        0.9,
    )
    .await
    .unwrap();
    rt.create_note(None, NoteKind::Observation, "QLoRA uses quantization", 0.8)
        .await
        .unwrap();
    rt.create_note(None, NoteKind::Question, "Review LoRA paper", 0.7)
        .await
        .unwrap();

    let observations = rt.list_notes(None, Some("observation"), 50).await.unwrap();
    assert_eq!(observations.len(), 2);

    let questions = rt.list_notes(None, Some("question"), 50).await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].content, "Review LoRA paper");

    let all = rt.list_notes(None, None, 50).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn create_all_note_kinds() {
    let rt = rt();
    for kind in [
        NoteKind::Observation,
        NoteKind::Insight,
        NoteKind::Question,
        NoteKind::Decision,
        NoteKind::Reference,
    ] {
        rt.create_note(None, kind, "content", 0.5).await.unwrap();
    }
    let all = rt.list_notes(None, None, 50).await.unwrap();
    assert_eq!(all.len(), 5);
}

// =============================================================================
// GQL query
// =============================================================================

#[tokio::test]
async fn query_via_gql() {
    let rt = rt();

    // Set up entities and edges
    let lora = rt
        .create_entity(None, "concept", "LoRA", None, None, vec![])
        .await
        .unwrap();
    let qlora = rt
        .create_entity(None, "concept", "QLoRA", None, None, vec![])
        .await
        .unwrap();
    rt.link(None, qlora.id, lora.id, EdgeRelation::VariantOf, 1.0)
        .await
        .unwrap();

    // Run a GQL traversal query
    let rows = rt
        .query(
            None,
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

    rt.create_entity(Some("ns_a"), "concept", "EntityA", None, None, vec![])
        .await
        .unwrap();
    rt.create_entity(Some("ns_b"), "concept", "EntityB", None, None, vec![])
        .await
        .unwrap();

    let a_entities = rt.list_entities(Some("ns_a"), None, 50).await.unwrap();
    assert_eq!(a_entities.len(), 1);
    assert_eq!(a_entities[0].name, "EntityA");

    let b_entities = rt.list_entities(Some("ns_b"), None, 50).await.unwrap();
    assert_eq!(b_entities.len(), 1);
    assert_eq!(b_entities[0].name, "EntityB");
}

// =============================================================================
// Hybrid search indexing
// =============================================================================

#[tokio::test]
async fn create_entity_indexes_into_text_search() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let entity = rt
        .create_entity(
            None,
            "concept",
            "FlashAttention",
            Some("efficient attention mechanism"),
            None,
            vec![],
        )
        .await
        .unwrap();
    let hits = rt
        .hybrid_search(None, "FlashAttention", None, 10)
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
    let result = rt
        .create_entity(None, "concept", "SilentVectorSkip", None, None, vec![])
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
    let entity = rt
        .create_entity(
            None,
            "concept",
            "SoftDeleteMe",
            Some("entity that will be soft-deleted"),
            None,
            vec![],
        )
        .await
        .unwrap();

    // Confirm the entity is visible before deletion.
    let hits_before = rt
        .hybrid_search(None, "SoftDeleteMe", None, 10)
        .await
        .unwrap();
    assert!(
        hits_before.iter().any(|h| h.entity_id == entity.id),
        "entity should appear in hybrid_search before soft-delete"
    );

    rt.delete_entity(None, entity.id, false).await.unwrap(); // soft delete

    let hits_after = rt
        .hybrid_search(None, "SoftDeleteMe", None, 10)
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
    let entity = rt
        .create_entity(
            None,
            "concept",
            "HardDeleteMe",
            Some("entity that will be hard-deleted"),
            None,
            vec![],
        )
        .await
        .unwrap();

    let hits_before = rt
        .hybrid_search(None, "HardDeleteMe", None, 10)
        .await
        .unwrap();
    assert!(
        hits_before.iter().any(|h| h.entity_id == entity.id),
        "entity should appear in hybrid_search before hard-delete"
    );

    rt.delete_entity(None, entity.id, true).await.unwrap(); // hard delete

    // Hard-deleted rows are gone from the entity store; the FTS/vector indexes may still
    // have stale entries. The soft-delete filter sees no alive entity and drops the hit.
    let hits_after = rt
        .hybrid_search(None, "HardDeleteMe", None, 10)
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
    let note = rt
        .create_note(None, NoteKind::Observation, "soft-delete-test", 0.9)
        .await
        .unwrap();

    let notes_before = rt.list_notes(None, None, 50).await.unwrap();
    assert!(
        notes_before.iter().any(|n| n.id == note.id),
        "note should appear before soft-delete"
    );

    rt.notes(None)
        .unwrap()
        .delete_note(note.id, DeleteMode::Soft)
        .await
        .unwrap();

    let notes_after = rt.list_notes(None, None, 50).await.unwrap();
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
            default_namespace: "local".to_string(),
            embedding_model: None,
        };
        let rt = KhiveRuntime::new(config).unwrap();
        rt.create_entity(None, "concept", "Persistent", None, None, vec![])
            .await
            .unwrap();
    }

    // Re-open the same file
    {
        let config = RuntimeConfig {
            db_path: Some(path.clone()),
            default_namespace: "local".to_string(),
            embedding_model: None,
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let entities = rt.list_entities(None, None, 50).await.unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].name, "Persistent");
    }
}
