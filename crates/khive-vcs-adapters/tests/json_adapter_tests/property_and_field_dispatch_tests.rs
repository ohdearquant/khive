// ---------------------------------------------------------------------------
// Test 4 — unknown extra keys on entities fold into properties
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_extra_keys_fold_into_properties() {
    let json = r#"[{
        "id": "33333333-3333-3333-3333-333333333333",
        "kind": "dataset",
        "name": "ImageNet",
        "download_url": "https://image-net.org",
        "num_classes": 1000
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid fixture");
    let ents: Vec<EntityRecord> = adapter.entities().map(|r| r.expect("no errors")).collect();

    assert_eq!(ents.len(), 1);
    let e = &ents[0];
    assert_eq!(e.name, "ImageNet");

    let url = e
        .properties
        .get("download_url")
        .and_then(|v| v.as_str())
        .expect("download_url folded into properties");
    assert_eq!(url, "https://image-net.org");

    let classes = e
        .properties
        .get("num_classes")
        .and_then(|v| v.as_i64())
        .expect("num_classes folded into properties");
    assert_eq!(classes, 1000);
}

// ---------------------------------------------------------------------------
// Test 5 — default weight on edge when absent
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_edge_default_weight() {
    let json = r#"[{
        "source": "11111111-1111-1111-1111-111111111111",
        "target": "22222222-2222-2222-2222-222222222222",
        "relation": "extends"
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid");
    let edges: Vec<EdgeRecord> = adapter.edges().map(|r| r.expect("no errors")).collect();

    assert_eq!(edges.len(), 1);
    assert!(
        (edges[0].weight - 0.7).abs() < f64::EPSILON,
        "default weight must be 0.7 (per EdgeRecord default_weight)"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — case-insensitive field lookup (ADR-036 §2)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_case_insensitive_entity_fields() {
    // Mixed-case field names: "Name" and "Kind" should be recognised.
    let json = r#"[{
        "Name": "MixedCaseEntity",
        "Kind": "concept",
        "Description": "upper-cased fields",
        "Tags": ["a", "b"],
        "ID": "44444444-4444-4444-4444-444444444444"
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid fixture");
    let ents: Vec<EntityRecord> = adapter.entities().map(|r| r.expect("no errors")).collect();

    assert_eq!(ents.len(), 1);
    let e = &ents[0];
    assert_eq!(e.name, "MixedCaseEntity");
    assert_eq!(e.kind, "concept");
    assert_eq!(e.description.as_deref(), Some("upper-cased fields"));
    assert_eq!(e.tags, vec!["a", "b"]);
    assert_eq!(e.id.to_string(), "44444444-4444-4444-4444-444444444444");
}

#[test]
fn test_json_adapter_case_insensitive_edge_dispatch() {
    // "Source" and "Target" (capitalised) must trigger edge dispatch.
    let json = r#"[{
        "Source": "11111111-1111-1111-1111-111111111111",
        "Target": "22222222-2222-2222-2222-222222222222",
        "Relation": "extends",
        "Weight": 0.5
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid");
    let edges: Vec<EdgeRecord> = adapter.edges().map(|r| r.expect("no errors")).collect();

    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].source, "11111111-1111-1111-1111-111111111111");
    assert_eq!(edges[0].target, "22222222-2222-2222-2222-222222222222");
    assert_eq!(edges[0].relation, "extends");
    assert!((edges[0].weight - 0.5).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------
// Test 7 — edge properties round-trip (no properties.properties nesting)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_edge_properties_no_double_nesting() {
    // An edge with a top-level "properties" object must round-trip cleanly —
    // EdgeRecord.properties must equal the original object, not nest it under
    // another "properties" key.
    let json = r#"[{
        "source": "11111111-1111-1111-1111-111111111111",
        "target": "22222222-2222-2222-2222-222222222222",
        "relation": "annotates",
        "properties": {"confidence": "high", "note": "verified"}
    }]"#;

    let mut adapter = JsonFormatAdapter::new(json).expect("valid");
    let edges: Vec<EdgeRecord> = adapter.edges().map(|r| r.expect("no errors")).collect();

    assert_eq!(edges.len(), 1);
    let props = &edges[0].properties;

    // Must NOT be nested
    assert!(
        props.get("properties").is_none(),
        "properties must not be nested under 'properties'"
    );

    // Must contain the original keys at the top level
    assert_eq!(
        props.get("confidence").and_then(|v| v.as_str()),
        Some("high")
    );
    assert_eq!(props.get("note").and_then(|v| v.as_str()), Some("verified"));
}
