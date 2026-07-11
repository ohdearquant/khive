// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Integration tests for [`JsonFormatAdapter`].
//!
//! These tests drive the public API — no access to private fields.
//! They cover the three required scenarios from issue #366:
//!   1. Roundtrip: entities + edges survive parse → re-serialize
//!   2. Empty input: valid JSON empty array produces zero records
//!   3. Malformed input: invalid JSON returns a parse error, no panic
//!
//! Tests 9-12 cover the taxonomy and weight validation invariants added for KVA-AUD-001
//! and KVA-AUD-002.

use khive_vcs_adapters::{
    AdapterError, EdgeRecord, EntityRecord, FormatAdapter, JsonFormatAdapter,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ENTITY_FIXTURE_JSON: &str = r#"[
  {
    "id": "11111111-1111-1111-1111-111111111111",
    "kind": "concept",
    "name": "FlashAttention",
    "description": "IO-aware exact attention",
    "tags": ["attention", "efficiency"],
    "properties": { "year": 2022 }
  },
  {
    "id": "22222222-2222-2222-2222-222222222222",
    "kind": "person",
    "name": "Tri Dao"
  }
]"#;

const EDGE_FIXTURE_JSON: &str = r#"[
  {
    "edge_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
    "source": "11111111-1111-1111-1111-111111111111",
    "target": "22222222-2222-2222-2222-222222222222",
    "relation": "introduced_by",
    "weight": 1.0
  }
]"#;

const MIXED_FIXTURE_JSON: &str = r#"[
  {
    "id": "11111111-1111-1111-1111-111111111111",
    "kind": "concept",
    "name": "FlashAttention"
  },
  {
    "id": "22222222-2222-2222-2222-222222222222",
    "kind": "person",
    "name": "Tri Dao"
  },
  {
    "edge_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
    "source": "11111111-1111-1111-1111-111111111111",
    "target": "22222222-2222-2222-2222-222222222222",
    "relation": "introduced_by",
    "weight": 0.9
  }
]"#;

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

// ---------------------------------------------------------------------------
// Test 13 — new_with_valid_kinds accepts a caller-supplied pack kind (#530)
// ---------------------------------------------------------------------------

#[test]
fn test_json_adapter_default_new_still_rejects_resource_kind() {
    // Plain `new()` (no injected registry) must keep rejecting kinds outside
    // the base ADR-001 set — `resource` is pack-registered (ADR-048), not a
    // base kind or alias.
    let mut adapter = JsonFormatAdapter::new(r#"[{"kind":"resource","name":"X"}]"#)
        .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::UnknownKind { kind, .. }) if kind == "resource"),
        "resource must be rejected without an injected valid-kinds registry"
    );
}

#[test]
fn test_json_adapter_new_with_valid_kinds_accepts_resource_kind() {
    let valid_kinds = vec!["resource".to_string()];
    let mut adapter = JsonFormatAdapter::new_with_valid_kinds(
        r#"[{"kind":"resource","name":"X"}]"#,
        &valid_kinds,
    )
    .expect("structurally valid JSON must construct");
    let first = adapter
        .entities()
        .next()
        .expect("one record present")
        .expect("resource kind must be accepted when present in the injected registry");
    assert_eq!(first.kind, "resource");
}

#[test]
fn test_json_adapter_new_with_valid_kinds_still_rejects_unregistered_kind() {
    // A kind that is neither a base ADR-001 kind nor present in the injected
    // registry must still be rejected — the injected registry widens, it does
    // not disable, validation.
    let valid_kinds = vec!["resource".to_string()];
    let mut adapter =
        JsonFormatAdapter::new_with_valid_kinds(r#"[{"kind":"gadget","name":"X"}]"#, &valid_kinds)
            .expect("structurally valid JSON must construct");
    let first = adapter.entities().next().expect("one record present");
    assert!(
        matches!(first, Err(AdapterError::UnknownKind { kind, .. }) if kind == "gadget"),
        "unregistered kind must still be rejected with a non-empty valid-kinds registry"
    );
}
