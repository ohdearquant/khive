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

#[tokio::test]
async fn set_note_property_initializes_null_and_preserves_json_type() {
    let store = setup_memory_store();
    let note = make_note("default", "observation", "atomic property set");
    let id = note.id;
    let updated_at = note.updated_at + 1;
    store.upsert_note(note).await.unwrap();

    assert!(store
        .set_note_property(
            id,
            "delivery.stamp",
            serde_json::json!({ "channel": "email", "attempt": 1 }),
            updated_at,
        )
        .await
        .unwrap());
    assert!(store
        .set_note_property(id, "explicit_null", serde_json::Value::Null, updated_at + 1)
        .await
        .unwrap());

    let fetched = store.get_note(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.properties,
        Some(serde_json::json!({
            "delivery.stamp": { "channel": "email", "attempt": 1 },
            "explicit_null": null
        })),
        "keys must be literal top-level segments and values must keep their JSON types"
    );
    assert_eq!(fetched.updated_at, updated_at + 1);
}
