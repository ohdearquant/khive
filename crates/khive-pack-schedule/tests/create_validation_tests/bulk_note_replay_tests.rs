// ── Bulk create note items (issue #3705) ────────────────────────────────────
//
// `khive-pack-kg`'s bulk `create` takes note items (`content`, `note_kind`,
// `salience`, an optional `name`) as well as entity items. The schedule-time
// check must accept and refuse exactly what the live handler does, or a
// scheduled bulk note create is refused at write time although it would
// replay cleanly.

/// Dispatches `schedule.schedule` with a bulk `create(items=...)` action.
async fn schedule_bulk_create(
    registry: &VerbRegistry,
    items: &serde_json::Value,
) -> Result<serde_json::Value, khive_runtime::RuntimeError> {
    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": format!("create(items={items})"),
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
}

#[tokio::test]
async fn schedule_schedule_accepts_create_bulk_note_item() {
    let (registry, _rt) = build_registry();
    let items = serde_json::json!([{"kind": "observation", "content": "x"}]);

    schedule_bulk_create(&registry, &items)
        .await
        .unwrap_or_else(|e| panic!("a bulk note item must be accepted; got: {e}"));

    registry
        .dispatch("create", serde_json::json!({ "items": items }))
        .await
        .expect("sanity: the live KG bulk create handler must accept this note item too");
}

#[tokio::test]
async fn schedule_schedule_accepts_create_bulk_note_items_using_every_note_field() {
    let (registry, _rt) = build_registry();
    let items = serde_json::json!([
        {
            "kind": "note", "note_kind": "insight", "name": "n", "content": "c",
            "salience": 0.7, "tags": ["t"], "properties": {"k": "v"}
        },
        {"kind": "decision", "content": "d"},
        {"kind": "concept", "name": "mixed-entity-item"}
    ]);

    schedule_bulk_create(&registry, &items)
        .await
        .unwrap_or_else(|e| panic!("a mixed entity/note batch must be accepted; got: {e}"));

    registry
        .dispatch("create", serde_json::json!({ "items": items }))
        .await
        .expect("sanity: the live KG bulk create handler must accept this batch too");
}

/// Widening the entry shape must keep `deny_unknown_fields`: an unrecognized
/// key is refused on a note item and on an entity item, as the live handler does.
#[tokio::test]
async fn schedule_schedule_still_rejects_unknown_field_in_create_bulk_item() {
    let (registry, _rt) = build_registry();

    for items in [
        serde_json::json!([{"kind": "observation", "content": "x", "bogus": 1}]),
        serde_json::json!([{"kind": "concept", "name": "x", "bogus": 1}]),
    ] {
        let err = schedule_bulk_create(&registry, &items)
            .await
            .expect_err("an unknown bulk item field must be refused at schedule time");
        assert!(
            err.to_string().contains("unknown field `bogus`"),
            "unexpected schedule-time error for {items}: {err}"
        );

        let kg_err = registry
            .dispatch("create", serde_json::json!({ "items": items }))
            .await
            .expect_err("sanity: the live KG bulk create handler must refuse it too");
        assert!(
            kg_err.to_string().contains("unknown field `bogus`"),
            "unexpected KG error for {items}: {kg_err}"
        );
    }
}

/// Field/substrate combinations the live bulk handler refuses per item must be
/// refused at schedule time too: note-only fields on an entity item,
/// entity-only fields on a note item, a missing `name` or `content`, a bad or
/// contradicting `note_kind`, and the one note kind bulk create never creates.
#[tokio::test]
async fn schedule_schedule_rejects_create_bulk_items_the_kg_handler_rejects() {
    let (registry, _rt) = build_registry();

    // (item, fragment of the schedule-time error, fragment of the live handler's error)
    let cases = [
        (
            serde_json::json!({"kind": "concept", "name": "x", "content": "c"}),
            "apply only to note items",
            "apply only to note items",
        ),
        (
            serde_json::json!({"kind": "concept", "name": "x", "note_kind": "insight"}),
            "apply only to note items",
            "apply only to note items",
        ),
        (
            serde_json::json!({"kind": "concept", "name": "x", "salience": 0.5}),
            "apply only to note items",
            "apply only to note items",
        ),
        (
            serde_json::json!({"kind": "concept"}),
            "requires `name`",
            "requires 'name'",
        ),
        (
            serde_json::json!({"kind": "concept", "name": "  "}),
            "requires `name`",
            "name must not be empty",
        ),
        (
            serde_json::json!({"kind": "observation", "content": "c", "description": "d"}),
            "apply only to entity items",
            "apply only to entity items",
        ),
        (
            serde_json::json!({"kind": "observation", "content": "c", "entity_type": "technique"}),
            "apply only to entity items",
            "apply only to entity items",
        ),
        (
            serde_json::json!({"kind": "observation", "content": "c", "entity_kind": "concept"}),
            "apply only to entity items",
            "apply only to entity items",
        ),
        (
            serde_json::json!({"kind": "observation"}),
            "requires `content`",
            "requires content",
        ),
        (
            serde_json::json!({"kind": "note", "note_kind": "nope", "content": "c"}),
            "unknown note_kind",
            "unknown note_kind",
        ),
        (
            serde_json::json!({"kind": "observation", "note_kind": "insight", "content": "c"}),
            "contradicts",
            "contradicts",
        ),
        (
            serde_json::json!({"kind": "scheduled_event", "content": "c"}),
            "not creatable via bulk create",
            "not creatable via bulk create",
        ),
    ];

    for (item, schedule_fragment, kg_fragment) in cases {
        let items = serde_json::json!([item]);

        let err = schedule_bulk_create(&registry, &items)
            .await
            .expect_err("schedule.schedule must refuse a bulk item the KG handler refuses");
        assert!(
            err.to_string().contains(schedule_fragment),
            "{items}: expected {schedule_fragment:?} in the schedule-time error; got: {err}"
        );

        let kg_err = registry
            .dispatch("create", serde_json::json!({ "items": items }))
            .await
            .expect_err("sanity: the live KG bulk create handler must refuse the item");
        assert!(
            kg_err.to_string().contains(kg_fragment),
            "{items}: expected {kg_fragment:?} in the KG error; got: {kg_err}"
        );
    }
}

/// Minimal bulk items that between them use every field of the KG pack's
/// `BulkCreateEntry`. The test below fails when the KG struct gains a field
/// that none of these items uses.
const KG_BULK_ENTRY_ITEMS: &[&str] = &[
    r#"{"kind":"entity","entity_kind":"concept","name":"drift-entity"}"#,
    r#"{"kind":"document","entity_type":"paper","name":"drift-typed","description":"d"}"#,
    r#"{"kind":"note","note_kind":"insight","name":"drift-note","content":"c","salience":0.5}"#,
    r#"{"kind":"observation","content":"drift-props","properties":{"k":"v"},"tags":["t"]}"#,
];
