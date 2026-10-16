#[test]
fn agent_preserves_empty_string_under_properties() {
    // ADR-045 Amendment 3 (issue #1995): a key under `properties` exists
    // only because a caller wrote it, so `""` there distinguishes
    // set-to-empty from absent and must survive Agent mode — at any
    // nesting depth under `properties`.
    let v = json!({
        "id": "a1b2c3d4",
        "summary": "",
        "properties": {"k": "", "nested": {"deep": ""}},
    });
    let out = agent(v);
    assert_eq!(out["properties"]["k"], json!(""));
    assert_eq!(out["properties"]["nested"]["deep"], json!(""));
    // Control: outside `properties` the empty-string drop still applies.
    assert!(out.get("summary").is_none());
}

#[test]
fn agent_drops_scalar_empty_properties_value() {
    // The carve-out applies only to strings nested under an object-valued
    // `properties`; a scalar `properties: ""` is the field's own value,
    // not a caller-written key under it, and drops like any empty string.
    let v = json!({
        "id": "a1b2c3d4",
        "properties": "",
    });
    let out = agent(v);
    assert!(out.get("properties").is_none());
}

#[test]
fn agent_still_drops_empty_containers_under_properties() {
    // The amendment's scope note keeps the container drop: empty arrays
    // and objects under `properties` are not carved out.
    let v = json!({
        "id": "a1b2c3d4",
        "properties": {"tags": [], "meta": {}, "kept": "x"},
    });
    let out = agent(v);
    assert!(out["properties"].get("tags").is_none());
    assert!(out["properties"].get("meta").is_none());
    assert_eq!(out["properties"]["kept"], json!("x"));
}

#[test]
fn verbose_passthrough() {
    let v = json!({"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "title": "X"});
    let out = present(v.clone(), PresentationMode::Verbose, NOW);
    assert_eq!(out, v);
}

#[test]
fn agent_shortens_uuid() {
    let v = json!({"id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"});
    let out = agent(v);
    assert_eq!(out["id"], json!("a1b2c3d4"));
}

#[test]
fn agent_drops_empty_string() {
    let v = json!({"title": "ok", "description": ""});
    let out = agent(v);
    assert!(out.get("description").is_none());
    assert_eq!(out["title"], json!("ok"));
}

#[test]
fn agent_drops_empty_array() {
    let v = json!({"tags": [], "title": "ok"});
    let out = agent(v);
    assert!(out.get("tags").is_none());
}

#[test]
fn agent_preserves_empty_list_page_arrays() {
    let v = json!({
        "items": [],
        "entities": [],
        "notes": [],
        "edges": [],
        "results": [],
        "entries": [],
        "neighbors": [],
        "next_after": null,
        "requested_limit": 10,
        "effective_limit": 10,
        "limit_clamped": false,
    });
    let out = agent(v);
    for key in EMPTY_ARRAY_PRESERVE {
        assert_eq!(out[*key], json!([]), "missing structural key {key}");
    }
    assert_eq!(out["next_after"], json!(null));
}

#[test]
fn agent_preserves_keyset_cursor_envelope_completion_signals() {
    // knowledge.list(after=…) terminal/empty page: both signals survive.
    let out = agent(json!({
        "results": [],
        "limit": 500,
        "order": "created_at ASC, id ASC",
        "next_after": null,
    }));
    assert_eq!(out["results"], json!([]));
    assert!(
        out.get("next_after").is_some_and(Value::is_null),
        "terminal cursor page must keep next_after:null: {out}"
    );
}

#[test]
fn agent_results_without_next_after_is_not_a_cursor_envelope() {
    let out = agent(json!({"results": [], "limit": 500, "title": "ordinary response"}));
    assert!(out.get("results").is_none());
}

#[test]
fn agent_still_drops_empty_arrays_outside_list_envelopes() {
    let out = agent(json!({"items": [], "entities": [], "title": "ordinary response"}));
    assert!(out.get("items").is_none());
    assert!(out.get("entities").is_none());
}

#[test]
fn agent_drops_empty_object() {
    let v = json!({"properties": {}, "title": "ok"});
    let out = agent(v);
    assert!(out.get("properties").is_none());
}

#[test]
fn agent_drops_non_lifecycle_null() {
    let v = json!({"result": null, "title": "ok"});
    let out = agent(v);
    assert!(out.get("result").is_none());
}

#[test]
fn agent_preserves_lifecycle_null() {
    let v = json!({"completed_at": null, "due_at": null, "title": "ok"});
    let out = agent(v);
    assert_eq!(out["completed_at"], json!(null));
    assert_eq!(out["due_at"], json!(null));
}

#[test]
fn agent_preserves_relationship_null() {
    let v = json!({"parent_id": null, "superseded_by": null});
    let out = agent(v);
    assert_eq!(out["parent_id"], json!(null));
    assert_eq!(out["superseded_by"], json!(null));
}
