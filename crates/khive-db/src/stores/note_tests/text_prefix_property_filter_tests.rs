#[test]
fn text_prefix_upper_bound_increments_the_last_code_point() {
    assert_eq!(text_prefix_upper_bound("email:").as_deref(), Some("email;"));
    assert_eq!(text_prefix_upper_bound("a").as_deref(), Some("b"));
    // A trailing char::MAX has no successor; the bound moves to the previous
    // code point. An empty prefix (or all char::MAX) has no upper bound.
    assert_eq!(text_prefix_upper_bound("a\u{10FFFF}").as_deref(), Some("b"));
    assert_eq!(text_prefix_upper_bound(""), None);
    assert_eq!(text_prefix_upper_bound("\u{10FFFF}"), None);
    // The surrogate gap is skipped so the bound stays a valid char.
    assert_eq!(
        text_prefix_upper_bound("x\u{D7FF}").as_deref(),
        Some("x\u{E000}")
    );
}

/// `TextStartsWithIndexed` is a byte-order range over the JSON text value:
/// it admits exactly the strings with the prefix, rejects the prefix's
/// neighbours on both sides, and never matches a missing, null, or numeric
/// field (those sort outside the text range in SQLite).
#[tokio::test]
async fn text_starts_with_indexed_matches_prefix_only() {
    use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
    use khive_storage::types::{PageRequest, SqlValue};

    let store = setup_memory_store();
    let seeded = [
        (
            "email:a@b.c",
            serde_json::json!({"to_actor": "email:a@b.c"}),
        ),
        ("email:", serde_json::json!({"to_actor": "email:"})),
        ("emaik:zzz", serde_json::json!({"to_actor": "emaik:zzz"})),
        ("email;", serde_json::json!({"to_actor": "email;"})),
        ("telegram:1", serde_json::json!({"to_actor": "telegram:1"})),
        ("number", serde_json::json!({"to_actor": 5})),
        ("null", serde_json::json!({"to_actor": null})),
        ("missing", serde_json::json!({})),
    ];
    for (name, properties) in seeded {
        let mut note = Note::new("default", "message", name);
        note.properties = Some(properties);
        store.upsert_note(note).await.unwrap();
    }
    let filter = NoteFilter {
        kind: Some("message".into()),
        property_filters: vec![PropertyFilter {
            json_path: "$.to_actor".into(),
            op: FilterOp::TextStartsWithIndexed,
            value: SqlValue::Text("email:".into()),
        }],
        ..Default::default()
    };
    let page = store
        .query_notes_filtered_count_free(
            "default",
            &filter,
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let mut names: Vec<_> = page.items.iter().map(|n| n.content.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["email:", "email:a@b.c"]);

    let mut bucket_filter = filter.clone();
    bucket_filter.property_filters[0].op = FilterOp::TextColonPrefixBucketIndexed;
    let bucket_page = store
        .query_notes_filtered_count_free(
            "default",
            &bucket_filter,
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .unwrap();
    let mut bucket_names: Vec<_> = bucket_page
        .items
        .iter()
        .map(|n| n.content.clone())
        .collect();
    bucket_names.sort();
    assert_eq!(
        bucket_names, names,
        "channel bucket must match the full prefix"
    );
    for invalid in ["", "email", "email::"] {
        bucket_filter.property_filters[0].value = SqlValue::Text(invalid.into());
        assert!(build_note_filter_where("default", &bucket_filter).is_err());
    }

    let every_text = NoteFilter {
        kind: Some("message".into()),
        property_filters: vec![PropertyFilter {
            json_path: "$.to_actor".into(),
            op: FilterOp::TextStartsWithIndexed,
            value: SqlValue::Text(String::new()),
        }],
        ..Default::default()
    };
    let page = store
        .query_notes_filtered_count_free(
            "default",
            &every_text,
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page.items.len(),
        5,
        "an empty prefix admits every text value and nothing else"
    );
}
