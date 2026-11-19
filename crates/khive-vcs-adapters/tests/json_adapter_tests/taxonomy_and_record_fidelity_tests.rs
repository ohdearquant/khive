// ---------------------------------------------------------------------------
// Test 9 — unknown entity kind returns UnknownKind (KVA-AUD-001)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_unknown_entity_kind_returns_error() {
    // "gadget" is not in the ADR-001 closed set.
    let mut adapter = JsonFormatAdapter::new(r#"[{"kind":"gadget","name":"X"}]"#)
        .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::UnknownKind { kind, .. }) if kind == "gadget"),
        "unknown kind must produce AdapterError::UnknownKind"
    );
}

// ---------------------------------------------------------------------------
// Test 10 — unknown edge relation returns UnknownRelation (KVA-AUD-001)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_unknown_edge_relation_returns_error() {
    // "related_to" is not in the ADR-002 closed set.
    let mut adapter =
        JsonFormatAdapter::new(r#"[{"source":"aa","target":"bb","relation":"related_to"}]"#)
            .expect("structurally valid JSON must construct");
    let first = adapter.edges().next().expect("one edge record present");
    assert!(
        matches!(first, Err(AdapterError::UnknownRelation { relation, .. }) if relation == "related_to"),
        "unknown relation must produce AdapterError::UnknownRelation"
    );
}

// ---------------------------------------------------------------------------
// Test 11 — missing kind is fatal (KVA-AUD-001)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_missing_kind_is_fatal() {
    // Missing 'kind' must return MissingField, not silently default to "concept".
    let mut adapter = JsonFormatAdapter::new(r#"[{"name":"NoKindEntity"}]"#)
        .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::MissingField { field, .. }) if field == "kind"),
        "missing 'kind' must produce AdapterError::MissingField"
    );
}

// ---------------------------------------------------------------------------
// Test 12 — edge weight out of [0.0, 1.0] returns InvalidField (KVA-AUD-002)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_edge_weight_out_of_range_returns_error() {
    // weight < 0.0
    let mut adapter = JsonFormatAdapter::new(
        r#"[{
        "source": "aa", "target": "bb", "relation": "extends", "weight": -0.1
    }]"#,
    )
    .expect("structurally valid JSON");
    let first = adapter.edges().next().expect("one edge");
    assert!(
        matches!(first, Err(AdapterError::InvalidField { field, .. }) if field == "weight"),
        "weight -0.1 must produce AdapterError::InvalidField"
    );

    // weight > 1.0
    let mut adapter2 = JsonFormatAdapter::new(
        r#"[{
        "source": "aa", "target": "bb", "relation": "extends", "weight": 1.1
    }]"#,
    )
    .expect("structurally valid JSON");
    let first2 = adapter2.edges().next().expect("one edge");
    assert!(
        matches!(first2, Err(AdapterError::InvalidField { field, .. }) if field == "weight"),
        "weight 1.1 must produce AdapterError::InvalidField"
    );
}

// ---------------------------------------------------------------------------
// Test 13 — known kind aliases are canonicalized (KVA-AUD-001)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_kind_aliases_canonicalized() {
    // "paper" is a recognized alias for "document" in EntityKind::from_str.
    let mut adapter =
        JsonFormatAdapter::new(r#"[{"kind":"paper","name":"Some Paper"}]"#).expect("valid JSON");
    let first = adapter.entities().next().expect("one record");
    let entity = first.expect("alias must parse as valid kind");
    assert_eq!(
        entity.kind, "document",
        "alias 'paper' must canonicalize to 'document'"
    );
}

// ---------------------------------------------------------------------------
// Test 8 — byte-identical entity round-trip
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_entity_byte_identical_roundtrip() {
    // Build a fully-populated EntityRecord, serialize it, parse it through the
    // adapter, serialize again, and assert byte equality.
    use uuid::Uuid;

    let id: Uuid = "55555555-5555-5555-5555-555555555555".parse().unwrap();
    let original = EntityRecord {
        id,
        kind: "dataset".into(),
        entity_type: None,
        name: "CIFAR-10".into(),
        description: Some("60k images, 10 classes".into()),
        properties: serde_json::json!({"num_classes": 10, "license": "MIT"}),
        tags: vec!["vision".into(), "benchmark".into()],
        created_at: None,
        updated_at: None,
    };

    // First serialization — this is the wire form the adapter will receive.
    let first_json = serde_json::to_string(&original).expect("EntityRecord serializes");

    // Wrap in an array so the adapter can parse it.
    let array_json = format!("[{}]", first_json);
    let mut adapter = JsonFormatAdapter::new(&array_json).expect("valid JSON");
    let parsed: Vec<EntityRecord> = adapter
        .entities()
        .map(|r| r.expect("no parse errors"))
        .collect();

    assert_eq!(parsed.len(), 1);

    // Second serialization — must be byte-identical to the first.
    let second_json = serde_json::to_string(&parsed[0]).expect("EntityRecord serializes");
    assert_eq!(
        first_json, second_json,
        "entity serialize → parse → serialize must be byte-identical"
    );
}

// ---------------------------------------------------------------------------
// Test 14 — ADR-020 entity fields are reserved, not folded into properties (#472)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_adr020_entity_fields_are_reserved() {
    let json = r#"[{
        "id": "66666666-6666-6666-6666-666666666666",
        "kind": "document",
        "name": "Attention Is All You Need",
        "entity_type": "paper",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-02-02T00:00:00Z"
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid fixture");
    let ents: Vec<EntityRecord> = adapter.entities().map(|r| r.expect("no errors")).collect();

    assert_eq!(ents.len(), 1);
    let e = &ents[0];
    assert_eq!(e.entity_type.as_deref(), Some("paper"));
    assert_eq!(e.created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(e.updated_at.as_deref(), Some("2026-02-02T00:00:00Z"));

    assert!(
        e.properties.get("entity_type").is_none(),
        "entity_type must not be folded into properties"
    );
    assert!(
        e.properties.get("created_at").is_none(),
        "created_at must not be folded into properties"
    );
    assert!(
        e.properties.get("updated_at").is_none(),
        "updated_at must not be folded into properties"
    );
}

// ---------------------------------------------------------------------------
// Test 15 — kind="paper" alias preserves entity_type="paper" (#472)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_kind_paper_alias_sets_entity_type() {
    let mut adapter =
        JsonFormatAdapter::new(r#"[{"kind":"paper","name":"Some Paper"}]"#).expect("valid JSON");
    let first = adapter.entities().next().expect("one record");
    let entity = first.expect("alias must parse as valid kind");

    assert_eq!(
        entity.kind, "document",
        "alias 'paper' must canonicalize kind to 'document'"
    );
    assert_eq!(
        entity.entity_type.as_deref(),
        Some("paper"),
        "alias 'paper' must be preserved as entity_type"
    );
    assert!(entity.properties.get("entity_type").is_none());
}

// ---------------------------------------------------------------------------
// Test 16 — ADR-020 edge timestamp fields are reserved (#472)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_edge_timestamps_are_reserved() {
    let json = r#"[{
        "source": "11111111-1111-1111-1111-111111111111",
        "target": "22222222-2222-2222-2222-222222222222",
        "relation": "extends",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-02-02T00:00:00Z"
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid");
    let edges: Vec<EdgeRecord> = adapter.edges().map(|r| r.expect("no errors")).collect();

    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(edges[0].updated_at.as_deref(), Some("2026-02-02T00:00:00Z"));
    assert!(edges[0].properties.get("created_at").is_none());
    assert!(edges[0].properties.get("updated_at").is_none());
}

// ---------------------------------------------------------------------------
// Test 17 — a non-object top-level array element is fatal, not skipped (#488a)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_non_object_array_element_is_fatal() {
    // A bare string in the top-level array must abort construction with a
    // fatal error identifying the offending record index, not be silently
    // skipped with a warning.
    let json = r#"[
        {"kind":"concept","name":"Good"},
        "not-a-record",
        {"kind":"concept","name":"NeverParsed"}
    ]"#;

    let err = match JsonFormatAdapter::new(json) {
        Err(e) => e,
        Ok(_) => panic!("a non-object array element must be a fatal construction error"),
    };
    match &err {
        AdapterError::InvalidField { index, field, .. } => {
            assert_eq!(*index, 1, "error must point at the non-object element");
            assert_eq!(field, "$record");
        }
        other => panic!("expected InvalidField at index 1, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 18 — non-object edge properties warns, never silently becomes {} (#488b)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_edge_properties_non_object_warns() {
    let json = r#"[{
        "source": "11111111-1111-1111-1111-111111111111",
        "target": "22222222-2222-2222-2222-222222222222",
        "relation": "extends",
        "properties": "not-an-object"
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("structurally valid JSON");
    let edges: Vec<EdgeRecord> = adapter
        .edges()
        .map(|r| r.expect("no parse errors"))
        .collect();

    assert_eq!(edges.len(), 1);
    assert_eq!(
        edges[0].properties,
        Value::Object(serde_json::Map::new()),
        "non-object properties must fall back to an empty object"
    );
    assert!(
        adapter
            .warnings()
            .iter()
            .any(|w| w.contains("properties") && w.contains("not an object")),
        "non-object edge properties must be reported as a warning, got: {:?}",
        adapter.warnings()
    );
}
