//! Backend contract tests (ADR-009 §backend-contract-tests).
//!
//! Exercises the storage-capability traits (`SqlAccess`, `EntityStore`,
//! `GraphStore`, `NoteStore`, `TextSearch`, `VectorStore`) against both
//! in-memory (`:memory:`) and file-backed SQLite backends.
//!
//! The harness is structured so that when a second backend ships (e.g.
//! `khive-db-postgres`), the same helper functions become a cross-backend
//! conformance suite: each `test_*` function is parameterised over a
//! `StorageBackend`, not hardwired to in-memory or file-backed.

use khive_db::StorageBackend;
use khive_storage::entity::Entity;
use khive_storage::note::Note;
use khive_storage::types::{
    DeleteMode, Direction, Edge, LinkId, NeighborQuery, SqlStatement, SqlValue, TextDocument,
    TextFilter, TextQueryMode, TextSearchRequest,
};
use khive_types::EdgeRelation;
use uuid::Uuid;

// ---- Factory helpers ----

fn memory_backend() -> StorageBackend {
    StorageBackend::memory().expect("in-memory backend")
}

fn file_backend(dir: &tempfile::TempDir, name: &str) -> StorageBackend {
    StorageBackend::sqlite(dir.path().join(name)).expect("file backend")
}

// ---- SqlAccess contract ----

async fn test_sql_access(backend: &StorageBackend) {
    let sql = backend.sql();

    let mut writer = sql.writer().await.expect("sql writer");
    writer
        .execute_script(
            "CREATE TABLE IF NOT EXISTS ct_sql (id TEXT PRIMARY KEY, val INTEGER)".into(),
        )
        .await
        .expect("create table");

    let affected = writer
        .execute(SqlStatement {
            sql: "INSERT INTO ct_sql (id, val) VALUES (?1, ?2)".into(),
            params: vec![SqlValue::Text("r1".into()), SqlValue::Integer(99)],
            label: None,
        })
        .await
        .expect("insert");
    assert_eq!(affected, 1);

    let mut reader = sql.reader().await.expect("sql reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT val FROM ct_sql WHERE id = ?1".into(),
            params: vec![SqlValue::Text("r1".into())],
            label: None,
        })
        .await
        .expect("query_row")
        .expect("row should exist");

    match &row.columns[0].value {
        SqlValue::Integer(v) => assert_eq!(*v, 99),
        other => panic!("expected Integer(99), got {other:?}"),
    }
}

#[tokio::test]
async fn sql_access_memory_contract() {
    test_sql_access(&memory_backend()).await;
}

#[tokio::test]
async fn sql_access_file_contract() {
    let dir = tempfile::tempdir().unwrap();
    test_sql_access(&file_backend(&dir, "sql_access.db")).await;
}

// ---- EntityStore contract ----

async fn test_entity_store(backend: &StorageBackend) {
    let store = backend
        .entities_for_namespace("ct_ns")
        .expect("entity store");

    let entity = Entity::new("ct_ns", "concept", "Test Entity");
    let id = entity.id;

    store.upsert_entity(entity).await.expect("upsert_entity");

    let fetched = store
        .get_entity(id)
        .await
        .expect("get_entity")
        .expect("entity must exist");
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.name, "Test Entity");
    assert_eq!(fetched.kind, "concept");
    assert!(fetched.deleted_at.is_none());

    // Soft-delete
    let deleted = store
        .delete_entity(id, DeleteMode::Soft)
        .await
        .expect("soft delete");
    assert!(deleted);

    // After soft delete, get_entity excludes the record (deleted_at IS NULL filter).
    // This is the correct contract: soft-deleted records are invisible to get_entity.
    let after = store.get_entity(id).await.expect("get after soft delete");
    assert!(
        after.is_none(),
        "soft-deleted entity should not appear via get_entity (deleted_at IS NULL filter)"
    );
}

#[tokio::test]
async fn entity_store_memory_contract() {
    test_entity_store(&memory_backend()).await;
}

#[tokio::test]
async fn entity_store_file_contract() {
    let dir = tempfile::tempdir().unwrap();
    test_entity_store(&file_backend(&dir, "entity.db")).await;
}

// ---- GraphStore contract ----

async fn test_graph_store(backend: &StorageBackend) {
    let entities = backend
        .entities_for_namespace("ct_graph")
        .expect("entity store");
    let graph = backend
        .graph_for_namespace("ct_graph")
        .expect("graph store");

    let a_entity = Entity::new("ct_graph", "concept", "A");
    let b_entity = Entity::new("ct_graph", "concept", "B");
    let a = a_entity.id;
    let b = b_entity.id;
    entities.upsert_entity(a_entity).await.expect("upsert A");
    entities.upsert_entity(b_entity).await.expect("upsert B");

    let edge_id = LinkId(Uuid::new_v4());
    let edge = Edge {
        id: edge_id,
        namespace: "ct_graph".to_string(),
        source_id: a,
        target_id: b,
        relation: EdgeRelation::Extends,
        weight: 1.0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };

    graph.upsert_edge(edge).await.expect("upsert_edge");

    // Query outgoing neighbors
    let query = NeighborQuery {
        direction: Direction::Out,
        relations: None,
        limit: Some(10),
        min_weight: None,
    };
    let neighbors = graph.neighbors(a, query).await.expect("neighbors");
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].node_id, b);
    assert_eq!(neighbors[0].relation, EdgeRelation::Extends);

    // Per ADR-009 §target_backend: local edge must have NULL target_backend.
    // The NeighborHit doesn't carry target_backend; verify through get_edge.
    let fetched_edge = graph
        .get_edge(edge_id)
        .await
        .expect("get_edge")
        .expect("edge must exist");
    assert!(
        fetched_edge.target_backend.is_none(),
        "local edge must have NULL target_backend (ADR-009)"
    );

    // Soft-delete
    let deleted = graph
        .delete_edge(edge_id, DeleteMode::Soft)
        .await
        .expect("soft delete edge");
    assert!(deleted);

    let after = graph
        .neighbors(
            a,
            NeighborQuery {
                direction: Direction::Out,
                relations: None,
                limit: Some(10),
                min_weight: None,
            },
        )
        .await
        .expect("neighbors after delete");
    assert!(
        after.is_empty(),
        "soft-deleted edge must not appear in neighbors"
    );
}

#[tokio::test]
async fn graph_store_memory_contract() {
    test_graph_store(&memory_backend()).await;
}

#[tokio::test]
async fn graph_store_file_contract() {
    let dir = tempfile::tempdir().unwrap();
    test_graph_store(&file_backend(&dir, "graph.db")).await;
}

// ---- NoteStore contract ----

async fn test_note_store(backend: &StorageBackend) {
    let store = backend.notes_for_namespace("ct_notes").expect("note store");

    let note = Note::new("ct_notes", "observation", "Test note content");
    let id = note.id;

    store.upsert_note(note).await.expect("upsert_note");

    let fetched = store
        .get_note(id)
        .await
        .expect("get_note")
        .expect("note must exist");
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.content, "Test note content");
    assert!(fetched.deleted_at.is_none());

    // Soft-delete
    let deleted = store
        .delete_note(id, DeleteMode::Soft)
        .await
        .expect("soft delete note");
    assert!(deleted);

    // After soft delete, get_note excludes the record (deleted_at IS NULL filter).
    let after = store.get_note(id).await.expect("get after delete");
    assert!(
        after.is_none(),
        "soft-deleted note should not appear via get_note (deleted_at IS NULL filter)"
    );
}

#[tokio::test]
async fn note_store_memory_contract() {
    test_note_store(&memory_backend()).await;
}

#[tokio::test]
async fn note_store_file_contract() {
    let dir = tempfile::tempdir().unwrap();
    test_note_store(&file_backend(&dir, "notes.db")).await;
}

// ---- TextSearch contract ----

async fn test_text_search(backend: &StorageBackend) {
    use khive_types::SubstrateKind;

    let store = backend.text("ct_fts").expect("text search");

    let id = Uuid::new_v4();
    let doc = TextDocument {
        subject_id: id,
        kind: SubstrateKind::Entity,
        title: Some("Rust Programming".to_string()),
        body: "The Rust language provides memory safety without GC.".to_string(),
        tags: vec!["rust".to_string()],
        namespace: "ct_ns".to_string(),
        metadata: None,
        updated_at: chrono::Utc::now(),
    };

    store.upsert_document(doc).await.expect("upsert_document");

    let results = store
        .search(TextSearchRequest {
            query: "memory safety".to_string(),
            mode: TextQueryMode::Plain,
            filter: Some(TextFilter {
                namespaces: vec!["ct_ns".to_string()],
                ..Default::default()
            }),
            top_k: 5,
            snippet_chars: 64,
        })
        .await
        .expect("text search");

    assert!(!results.is_empty(), "should find at least one result");
    assert_eq!(results[0].subject_id, id);

    let count = store
        .count(TextFilter {
            namespaces: vec!["ct_ns".to_string()],
            ..Default::default()
        })
        .await
        .expect("count");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn text_search_memory_contract() {
    test_text_search(&memory_backend()).await;
}

#[tokio::test]
async fn text_search_file_contract() {
    let dir = tempfile::tempdir().unwrap();
    test_text_search(&file_backend(&dir, "fts.db")).await;
}

// ---- VectorStore contract (feature-gated) ----

#[cfg(feature = "vectors")]
mod vector_contract {
    use super::*;
    use khive_storage::types::VectorSearchRequest;
    use khive_types::SubstrateKind;

    async fn test_vector_store(backend: &StorageBackend) {
        let store = backend
            .vectors_for_namespace("ct_model", "ct_model", 4, "ct_ns")
            .expect("vector store");

        let id = Uuid::new_v4();
        store
            .insert(
                id,
                SubstrateKind::Entity,
                "ct_ns",
                "content",
                vec![vec![1.0, 0.0, 0.0, 0.0]],
            )
            .await
            .expect("vector insert");

        let count = store.count().await.expect("vector count");
        assert_eq!(count, 1);

        let hits = store
            .search(VectorSearchRequest {
                query_vectors: vec![vec![1.0, 0.0, 0.0, 0.0]],
                top_k: 1,
                namespace: None,
                kind: None,
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .expect("vector search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id);
        assert!(
            hits[0].score.to_f64() > 0.99,
            "cosine score for identical vector should be > 0.99"
        );
    }

    #[tokio::test]
    async fn vector_store_memory_contract() {
        test_vector_store(&memory_backend()).await;
    }

    #[tokio::test]
    async fn vector_store_file_contract() {
        let dir = tempfile::tempdir().unwrap();
        test_vector_store(&file_backend(&dir, "vectors.db")).await;
    }
}
