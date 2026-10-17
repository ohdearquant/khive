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
async fn filtered_default_order_is_stable_across_equal_timestamp_pages() {
    let store = setup_memory_store();
    let created_at = 1_750_000_000_000_000_i64;
    let mut expected_ids = Vec::new();

    for index in 0..317 {
        let mut note = make_note("ns1", "observation", &format!("note-{index}"));
        note.created_at = created_at;
        expected_ids.push(note.id);
        store.upsert_note(note).await.unwrap();
    }
    expected_ids.sort_unstable();

    let mut actual_ids = Vec::new();
    let page_size = 29_u32;
    let mut offset = 0_u64;
    loop {
        let page = store
            .query_notes_filtered(
                "ns1",
                &NoteFilter::default(),
                PageRequest {
                    offset,
                    limit: page_size,
                },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        actual_ids.extend(page.items.into_iter().map(|note| note.id));
    }

    assert_eq!(actual_ids, expected_ids);
}

/// #1671: `query_notes` (the single-namespace `list` path) had no `id`
/// tiebreak at all. Rows sharing `created_at` swept via offset pages must come
/// back exactly once — no duplicates, no misses across page boundaries.
#[tokio::test]
async fn query_notes_offset_sweep_covers_equal_created_at_exactly_once() {
    let store = setup_memory_store();
    let created_at = 1_750_000_000_000_000_i64;
    let mut expected_ids = Vec::new();

    for index in 0..211 {
        let mut note = make_note("ns1", "observation", &format!("note-{index}"));
        note.created_at = created_at;
        expected_ids.push(note.id);
        store.upsert_note(note).await.unwrap();
    }
    expected_ids.sort_unstable();

    let mut actual_ids = Vec::new();
    let page_size = 37_u32;
    let mut offset = 0_u64;
    loop {
        let page = store
            .query_notes(
                "ns1",
                None,
                PageRequest {
                    offset,
                    limit: page_size,
                },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        actual_ids.extend(page.items.into_iter().map(|note| note.id));
    }

    assert_eq!(actual_ids, expected_ids);
}

/// #1671: a custom JSON `order_by` whose extracted value repeats must also get
/// an `id` tiebreak in the primary key's direction, or offset pages over equal
/// values are unsound.
#[tokio::test]
async fn query_notes_filtered_custom_order_offset_sweep_is_total() {
    let store = setup_memory_store();
    let mut expected_ids = Vec::new();

    for index in 0..113 {
        let note = make_note_with_props(
            "ns1",
            "observation",
            &format!("note-{index}"),
            serde_json::json!({"rank": 7}),
        );
        expected_ids.push(note.id);
        store.upsert_note(note).await.unwrap();
    }
    expected_ids.sort_unstable_by(|a, b| b.cmp(a));

    let filter = NoteFilter {
        order_by: Some(("$.rank".to_string(), SortDir::Desc)),
        ..Default::default()
    };
    let mut actual_ids = Vec::new();
    let page_size = 23_u32;
    let mut offset = 0_u64;
    loop {
        let page = store
            .query_notes_filtered(
                "ns1",
                &filter,
                PageRequest {
                    offset,
                    limit: page_size,
                },
            )
            .await
            .unwrap();
        if page.items.is_empty() {
            break;
        }
        offset += page.items.len() as u64;
        actual_ids.extend(page.items.into_iter().map(|note| note.id));
    }

    assert_eq!(actual_ids, expected_ids);
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
