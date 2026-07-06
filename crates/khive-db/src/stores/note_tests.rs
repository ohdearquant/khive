use super::*;
use crate::pool::PoolConfig;

fn setup_pool() -> Arc<ConnectionPool> {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }
    pool
}

fn setup_memory_store() -> SqlNoteStore {
    SqlNoteStore::new(setup_pool(), false)
}

fn make_note(namespace: &str, kind: &str, content: &str) -> Note {
    Note::new(namespace, kind, content)
}

#[tokio::test]
async fn test_upsert_and_get_note() {
    let store = setup_memory_store();

    let note = make_note("default", "observation", "Hello world");
    let id = note.id;

    store.upsert_note(note).await.unwrap();

    let fetched = store.get_note(id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.content, "Hello world");
    assert_eq!(fetched.kind, "observation");
}

#[tokio::test]
async fn test_kind_roundtrip_all_variants() {
    let store = setup_memory_store();
    for kind in [
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
    ] {
        let note = make_note("default", kind, "content");
        let id = note.id;
        store.upsert_note(note).await.unwrap();
        let fetched = store.get_note(id).await.unwrap().unwrap();
        assert_eq!(fetched.kind, kind);
    }
}

#[tokio::test]
async fn test_soft_delete() {
    let store = setup_memory_store();

    let note = make_note("default", "observation", "to be deleted");
    let id = note.id;
    store.upsert_note(note).await.unwrap();

    let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
    assert!(deleted);

    let fetched = store.get_note(id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn test_hard_delete() {
    let store = setup_memory_store();

    let note = make_note("default", "observation", "to be hard deleted");
    let id = note.id;
    store.upsert_note(note).await.unwrap();

    let deleted = store.delete_note(id, DeleteMode::Hard).await.unwrap();
    assert!(deleted);

    let fetched = store.get_note(id).await.unwrap();
    assert!(fetched.is_none());
}

/// Namespace isolation: one store, two namespaces — each query sees only its own.
#[tokio::test]
async fn test_namespace_isolation() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);

    for _ in 0..3 {
        store
            .upsert_note(make_note("ns1", "observation", "content"))
            .await
            .unwrap();
    }
    store
        .upsert_note(make_note("ns2", "observation", "other"))
        .await
        .unwrap();

    let count_ns1 = store.count_notes("ns1", None).await.unwrap();
    assert_eq!(count_ns1, 3);

    let count_ns2 = store.count_notes("ns2", None).await.unwrap();
    assert_eq!(count_ns2, 1);
}

/// query_notes and count_notes use the namespace parameter as passed.
#[tokio::test]
async fn test_query_and_count_use_caller_namespace() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);

    store
        .upsert_note(make_note("ns_a", "observation", "A"))
        .await
        .unwrap();
    store
        .upsert_note(make_note("ns_b", "insight", "B"))
        .await
        .unwrap();

    let page_a = store
        .query_notes("ns_a", None, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_a.items.len(), 1);
    assert_eq!(page_a.items[0].content, "A");
    assert_eq!(page_a.total, Some(1));

    let page_b = store
        .query_notes("ns_b", None, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_b.items.len(), 1);
    assert_eq!(page_b.items[0].content, "B");
    assert_eq!(page_b.total, Some(1));

    let count_a = store.count_notes("ns_a", None).await.unwrap();
    let count_b = store.count_notes("ns_b", None).await.unwrap();
    assert_eq!(count_a, 1);
    assert_eq!(count_b, 1);
}

#[tokio::test]
async fn test_soft_delete_sets_status_deleted() {
    let pool = setup_pool();
    let store = SqlNoteStore::new(Arc::clone(&pool), false);
    let note = make_note("default", "observation", "to delete");
    let id = note.id;
    store.upsert_note(note).await.unwrap();
    let deleted = store.delete_note(id, DeleteMode::Soft).await.unwrap();
    assert!(deleted);
    // Verify directly via raw SQL
    let writer = pool.writer().unwrap();
    let status: String = writer
        .conn()
        .query_row(
            "SELECT status FROM notes WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "deleted");
}

#[tokio::test]
async fn test_note_status_field_roundtrip() {
    let store = setup_memory_store();
    let note = make_note("default", "observation", "status test");
    let id = note.id;
    store.upsert_note(note).await.unwrap();
    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(fetched.status, "active");
}

// -- query_notes_filtered tests --

fn make_note_with_props(
    namespace: &str,
    kind: &str,
    content: &str,
    props: serde_json::Value,
) -> Note {
    Note::new(namespace, kind, content).with_properties(props)
}

#[tokio::test]
async fn test_filtered_namespace_and_kind_isolation() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    // Insert "scheduled_event" with status=pending in ns1
    let n1 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "event1",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-01T00:00:00Z"}),
    );
    let n2 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "event2",
        serde_json::json!({"status": "done", "trigger_at": "2027-01-02T00:00:00Z"}),
    );
    let n3 = make_note_with_props(
        "ns2",
        "scheduled_event",
        "event3",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-03T00:00:00Z"}),
    );
    store.upsert_note(n1).await.unwrap();
    store.upsert_note(n2).await.unwrap();
    store.upsert_note(n3).await.unwrap();

    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![NotePropFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: None,
        ..Default::default()
    };

    let page = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        1,
        "only the pending ns1 event should appear"
    );
    assert_eq!(page.items[0].content, "event1");
    assert_eq!(page.total, Some(1));
}

#[tokio::test]
async fn test_filtered_order_by_json_path_asc() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter, SortDir};
    use khive_storage::types::{PageRequest, SqlValue};

    // Insert in reverse order — filter should return ascending by trigger_at.
    let n3 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "third",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-03T00:00:00Z"}),
    );
    let n1 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "first",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-01T00:00:00Z"}),
    );
    let n2 = make_note_with_props(
        "ns1",
        "scheduled_event",
        "second",
        serde_json::json!({"status": "pending", "trigger_at": "2027-01-02T00:00:00Z"}),
    );
    store.upsert_note(n3).await.unwrap();
    store.upsert_note(n1).await.unwrap();
    store.upsert_note(n2).await.unwrap();

    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![NotePropFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: Some(("$.trigger_at".to_string(), SortDir::Asc)),
        ..Default::default()
    };

    let page = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.items[0].content, "first");
    assert_eq!(page.items[1].content, "second");
    assert_eq!(page.items[2].content, "third");
}

#[tokio::test]
async fn test_filtered_soft_deleted_excluded() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter};
    use khive_storage::types::{DeleteMode, PageRequest, SqlValue};

    let n = make_note_with_props(
        "ns1",
        "scheduled_event",
        "to_delete",
        serde_json::json!({"status": "pending"}),
    );
    let id = n.id;
    store.upsert_note(n).await.unwrap();
    store.delete_note(id, DeleteMode::Soft).await.unwrap();

    let filter = NoteFilter {
        kind: Some("scheduled_event".to_string()),
        property_filters: vec![NotePropFilter {
            json_path: "$.status".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("pending".to_string()),
        }],
        order_by: None,
        ..Default::default()
    };

    let page = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 0, "soft-deleted rows must not appear");
}

#[tokio::test]
async fn test_filtered_invalid_json_path_rejected() {
    let store = setup_memory_store();
    use khive_storage::note::PropertyFilter as NotePropFilter;
    use khive_storage::note::{FilterOp, NoteFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    let filter = NoteFilter {
        kind: None,
        property_filters: vec![NotePropFilter {
            json_path: "DROP TABLE notes".to_string(),
            op: FilterOp::Eq,
            value: SqlValue::Text("x".to_string()),
        }],
        order_by: None,
        ..Default::default()
    };

    let result = store
        .query_notes_filtered("ns1", &filter, PageRequest::default())
        .await;
    assert!(
        result.is_err(),
        "invalid json_path must be rejected before SQL"
    );
}

// ── try_insert_note dedup-vs-error discrimination ───────────────────────────

/// A PRIMARY KEY collision on a note without external_id must surface as a
/// StorageError, not be silently misreported as a dedup hit.
#[tokio::test]
async fn test_try_insert_note_pk_collision_returns_error_not_dedup() {
    let store = setup_memory_store();

    let mut note = make_note("ns1", "message", "original content");
    let fixed_id = uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000099").unwrap();
    note.id = fixed_id;
    // No external_id on this note.

    let inserted = store
        .try_insert_note(note.clone())
        .await
        .expect("first insert must succeed");
    assert!(inserted, "first insert must return true");

    // Second attempt with the same UUID triggers a PK collision.
    // With no external_id to verify against, this must not be reported as dedup.
    let result = store.try_insert_note(note).await;
    assert!(
        result.is_err(),
        "PK collision without external_id must return StorageError, not Ok(false)"
    );
}

/// STORAGE-AUD-003 / #485: PageRequest.offset > i64::MAX must return
/// InvalidInput for both list paths instead of silently narrowing to a
/// negative i64 offset.
#[tokio::test]
async fn page_offset_over_i64max_rejected() {
    let store = setup_memory_store();
    store
        .upsert_note(make_note("ns1", "observation", "Hello world"))
        .await
        .unwrap();

    let oversized = PageRequest {
        offset: (i64::MAX as u64) + 1,
        limit: 10,
    };

    let result = store.query_notes("ns1", None, oversized.clone()).await;
    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "query_notes: expected InvalidInput, got {result:?}"
    );

    let filtered_result = store
        .query_notes_filtered("ns1", &NoteFilter::default(), oversized)
        .await;
    assert!(
        matches!(filtered_result, Err(StorageError::InvalidInput { .. })),
        "query_notes_filtered: expected InvalidInput, got {filtered_result:?}"
    );
}

/// ADR-067 Component A entry 2: with `KHIVE_WRITE_QUEUE=1`, `upsert_notes`
/// routes through the WriterTask channel instead of the pool-mutex path, and
/// both rows are actually committed and independently readable back.
///
/// `#[serial]`: mutates the process-global `KHIVE_WRITE_QUEUE` env var,
/// shared with `pool.rs`'s own env-override tests in this same test binary.
#[tokio::test]
#[serial_test::serial]
async fn upsert_notes_routes_through_writer_task_when_flag_enabled() {
    std::env::set_var("KHIVE_WRITE_QUEUE", "1");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_notes.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }

    let store = SqlNoteStore::new(Arc::clone(&pool), true);
    std::env::remove_var("KHIVE_WRITE_QUEUE");

    let n1 = make_note("default", "observation", "first");
    let n2 = make_note("default", "observation", "second");
    let id1 = n1.id;
    let id2 = n2.id;

    let summary = store.upsert_notes(vec![n1, n2]).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    assert!(store.get_note(id1).await.unwrap().is_some());
    assert!(store.get_note(id2).await.unwrap().is_some());
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}
