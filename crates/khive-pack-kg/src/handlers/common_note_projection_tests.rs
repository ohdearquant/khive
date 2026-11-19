use super::*;

fn note(value: serde_json::Value) -> serde_json::Map<String, Value> {
    remap_note_status(value)
        .as_object()
        .expect("projection returns an object for an object input")
        .clone()
}

#[test]
fn projection_reports_the_tags_the_filter_matches_on() {
    // Stored shape: notes have no tags column, so the tags a caller passed live
    // under `properties.tags`, which is also the path the tag filter extracts.
    let out = note(json!({
        "kind": "observation",
        "content": "body",
        "properties": {"tags": ["fleet-incident", "merge-safety"]},
    }));
    assert_eq!(
        out.get("tags"),
        Some(&json!(["fleet-incident", "merge-safety"])),
        "a note must report the tags it is findable by"
    );

    // A non-array value is left where it is: promoting it would hand callers a
    // `tags` field they still have to type-check.
    let out = note(json!({
        "kind": "observation",
        "content": "body",
        "properties": {"tags": "fleet-incident,merge-safety"},
    }));
    assert_eq!(out.get("tags"), None, "only an array is lifted");

    // An explicit top-level value wins over the stored one, so a substrate that
    // does carry its own tags column is never overwritten by this projection.
    let out = note(json!({
        "kind": "observation",
        "content": "body",
        "tags": ["explicit"],
        "properties": {"tags": ["stored"]},
    }));
    assert_eq!(out.get("tags"), Some(&json!(["explicit"])));
}

#[test]
fn projection_labels_a_nameless_note_from_its_first_content_line() {
    // A name is optional and nothing defaults it, so this is the common shape.
    let out = note(json!({
        "kind": "observation",
        "name": Value::Null,
        "content": "\n  A review status in a mutable flag is a side effect.\nSecond line.",
    }));
    assert_eq!(
        out.get("display_name"),
        Some(&json!(
            "A review status in a mutable flag is a side effect."
        )),
        "the label is the first non-empty line, trimmed"
    );
    assert_eq!(
        out.get("name"),
        Some(&Value::Null),
        "the stored name is untouched: the projection labels, it does not title"
    );

    // With a name, the label is that name. An implementation that always derives
    // from content fails here.
    let out = note(json!({"kind": "observation", "name": "Real title", "content": "body"}));
    assert_eq!(out.get("display_name"), Some(&json!("Real title")));

    // A whitespace-only name is as blank as a null one. An implementation testing
    // only for null fails here.
    let out = note(json!({"kind": "observation", "name": "   ", "content": "derived"}));
    assert_eq!(out.get("display_name"), Some(&json!("derived")));

    // Whitespace-only content yields no label rather than an empty one.
    let out = note(json!({"kind": "observation", "name": Value::Null, "content": "   \n\n"}));
    assert_eq!(out.get("display_name"), None);

    // A long first line is capped and marked, so a listing column stays readable.
    let long = "x".repeat(DERIVED_LABEL_MAX_CHARS + 10);
    let out = note(json!({"kind": "observation", "name": Value::Null, "content": long}));
    let label = out
        .get("display_name")
        .and_then(Value::as_str)
        .expect("a long line still yields a label");
    assert_eq!(label.chars().count(), DERIVED_LABEL_MAX_CHARS + 1);
    assert!(label.ends_with('\u{2026}'), "got {label}");
}
