// ---------------------------------------------------------------------------
// Test 1 — roundtrip
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_roundtrip() {
    // --- Entities ---
    let mut adapter =
        JsonFormatAdapter::new(ENTITY_FIXTURE_JSON).expect("valid JSON must not fail construction");

    assert_eq!(adapter.name(), "json");

    let entities: Vec<EntityRecord> = adapter
        .entities()
        .map(|r| r.expect("fixture has no errors"))
        .collect();

    assert_eq!(entities.len(), 2, "two entities expected");

    // First entity: FlashAttention
    let fa = &entities[0];
    assert_eq!(fa.id.to_string(), "11111111-1111-1111-1111-111111111111");
    assert_eq!(fa.kind, "concept");
    assert_eq!(fa.name, "FlashAttention");
    assert_eq!(fa.description.as_deref(), Some("IO-aware exact attention"));
    assert_eq!(fa.tags, vec!["attention", "efficiency"]);

    // Extra 'properties' key should be merged
    let year = fa
        .properties
        .get("year")
        .and_then(|v| v.as_i64())
        .expect("year should be in properties");
    assert_eq!(year, 2022);

    // Second entity: Tri Dao (no description, no tags)
    let td = &entities[1];
    assert_eq!(td.kind, "person");
    assert_eq!(td.name, "Tri Dao");
    assert!(td.description.is_none());
    assert!(td.tags.is_empty());

    // No edges in an entity-only array
    let edges: Vec<EdgeRecord> = adapter
        .edges()
        .map(|r| r.expect("no edge errors"))
        .collect();
    assert!(edges.is_empty(), "no edges expected from entity-only input");

    // No warnings
    assert!(adapter.warnings().is_empty(), "no warnings expected");

    // --- Edges ---
    let mut adapter2 =
        JsonFormatAdapter::new(EDGE_FIXTURE_JSON).expect("valid JSON must not fail construction");

    let edges2: Vec<EdgeRecord> = adapter2
        .edges()
        .map(|r| r.expect("fixture has no errors"))
        .collect();
    assert_eq!(edges2.len(), 1, "one edge expected");

    let e = &edges2[0];
    assert_eq!(
        e.edge_id.to_string(),
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
    );
    assert_eq!(e.source, "11111111-1111-1111-1111-111111111111");
    assert_eq!(e.target, "22222222-2222-2222-2222-222222222222");
    assert_eq!(e.relation, "introduced_by");
    assert!((e.weight - 1.0).abs() < f64::EPSILON);

    // Re-serialize and check the relation survives
    let json = serde_json::to_string(e).expect("edges serialize to JSON");
    let parsed: Value = serde_json::from_str(&json).expect("re-parse");
    assert_eq!(parsed["relation"], "introduced_by");

    // --- Mixed ---
    let mut adapter3 =
        JsonFormatAdapter::new(MIXED_FIXTURE_JSON).expect("mixed fixture is valid JSON");

    let ents3: Vec<_> = adapter3
        .entities()
        .map(|r| r.expect("no entity errors"))
        .collect();
    let edgs3: Vec<_> = adapter3
        .edges()
        .map(|r| r.expect("no edge errors"))
        .collect();

    assert_eq!(ents3.len(), 2, "two entities in mixed fixture");
    assert_eq!(edgs3.len(), 1, "one edge in mixed fixture");
    assert!((edgs3[0].weight - 0.9).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------
// Test 2 — empty input produces valid empty JSON (zero records, no errors)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_handles_empty() {
    let mut adapter = JsonFormatAdapter::new("[]").expect("empty array is valid JSON");

    let entities: Vec<_> = adapter.entities().collect();
    let edges: Vec<_> = adapter.edges().collect();

    assert!(entities.is_empty(), "empty input: no entities");
    assert!(edges.is_empty(), "empty input: no edges");
    assert!(adapter.warnings().is_empty(), "empty input: no warnings");

    // Serialize the empty results to confirm nothing panics and the output is valid
    let entity_json =
        serde_json::to_string(&Vec::<EntityRecord>::new()).expect("empty entity vec serializes");
    let edge_json =
        serde_json::to_string(&Vec::<EdgeRecord>::new()).expect("empty edge vec serializes");

    assert_eq!(entity_json, "[]");
    assert_eq!(edge_json, "[]");
}

// ---------------------------------------------------------------------------
// Test 3 — malformed input returns an error, does not panic
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_rejects_malformed() {
    // Case A: not valid JSON at all
    let result = JsonFormatAdapter::new("{ not json }");
    assert!(
        matches!(result, Err(AdapterError::Parse(_))),
        "non-JSON must produce AdapterError::Parse"
    );

    // Case B: valid JSON but not an array (object)
    let result = JsonFormatAdapter::new(r#"{"key": "value"}"#);
    assert!(
        matches!(result, Err(AdapterError::Parse(_))),
        "JSON object at top level must produce AdapterError::Parse"
    );

    // Case C: valid JSON but not an array (bare string)
    let result = JsonFormatAdapter::new(r#""just a string""#);
    assert!(
        matches!(result, Err(AdapterError::Parse(_))),
        "JSON string at top level must produce AdapterError::Parse"
    );

    // Case D: array with a valid-structure entity missing the required 'name' field
    let mut adapter = JsonFormatAdapter::new(r#"[{"kind": "concept"}]"#)
        .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::MissingField { field, .. }) if field == "name"),
        "missing 'name' must produce MissingField error"
    );

    // Case E: edge missing 'relation'
    let mut adapter2 = JsonFormatAdapter::new(r#"[{"source": "aa", "target": "bb"}]"#)
        .expect("structurally valid JSON");
    let first_edge = adapter2.edges().next().expect("one edge record present");
    assert!(
        matches!(first_edge, Err(AdapterError::MissingField { field, .. }) if field == "relation"),
        "missing 'relation' on edge must produce MissingField error"
    );
}
