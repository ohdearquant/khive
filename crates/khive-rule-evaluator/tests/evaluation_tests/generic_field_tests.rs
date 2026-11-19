#[test]
fn generic_conditions_required_field_types_and_self_loop_sentinel_are_preserved() {
    let config = r#"
[[rules]]
id="description"
kind="entity"
condition="kind=concept"
require_field="description"
message="description {id}"
[[rules]]
id="loop"
kind="edge"
condition="source_id=target_id"
require_field="ignored"
message="loop {id}"
"#;
    let entities = "invalid\n\n{\"id\":\"a\",\"kind\":\"concept\",\"description\":7}\n{\"id\":\"b\",\"kind\":\"concept\",\"description\":\" \"}\n{\"id\":\"c\",\"kind\":\"project\"}";
    let edges = r#"{"source":"a","target":"a","relation":"contains"}
{"source_id":"b","target_id":"b"}
{"source":"a","target":"b"}
{}
"#;
    let results = run(
        config,
        entities,
        edges,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(results[0].violations.len(), 1);
    assert_eq!(results[0].violations[0].entity_id.as_deref(), Some("a"));
    assert_eq!(
        results[1]
            .violations
            .iter()
            .map(|v| v.message.as_str())
            .collect::<Vec<_>>(),
        ["loop ", "loop ", "loop "]
    );
    assert_eq!(
        results[1].violations[0].entity_kind.as_deref(),
        Some("contains")
    );
    assert!(results[1].violations[2].entity_id.is_none());
}

#[test]
fn canonical_fields_shadow_aliases_even_when_null_or_wrong_type() {
    assert_eq!(
        edge_source_id(&json!({"source":"first","source_id":"second"})),
        Some("first")
    );
    assert_eq!(edge_target_id(&json!({"target_id":"alias"})), Some("alias"));
    assert_eq!(
        edge_source_id(&json!({"source":null,"source_id":"alias"})),
        None
    );
    assert_eq!(
        edge_target_id(&json!({"target":9,"target_id":"alias"})),
        None
    );
    let input = r#"{"edge_id":"canonical","id":"other"}
{"id":"legacy"}
{"edge_id":null,"id":"hidden"}
{"edge_id":7,"id":"also-hidden"}
bad
"#;
    assert_eq!(
        collect_edge_ids(input),
        ["canonical".to_string(), "legacy".to_string()]
            .into_iter()
            .collect()
    );
    assert_eq!(
        collect_ids(input),
        ["other", "legacy", "hidden", "also-hidden"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    let results = run(
        "[dangling_refs]",
        "",
        r#"{"source":null,"source_id":"hidden","target":8,"target_id":"hidden"}"#,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert!(results[0].passed);
}

// The endpoints deliberately differ from the record ID, and canonical identity
// fields retain precedence even when they are empty, null, or the wrong type.
const GENERIC_EDGE_RULE_RECORDS: &str = r#"{"edge_id":"edge-only","source":"node-only","target":"node-only","relation":"contains"}
{"edge_id":"edge-both","id":"legacy-decoy","source":"node-both","target":"node-both","relation":"contains"}
{"id":"legacy-only","source_id":"node-legacy","target_id":"node-legacy","relation":"contains"}
{"source":"node-missing","target":"node-missing","relation":"contains"}
{"edge_id":null,"id":"hidden-null","source":"node-null","target":"node-null","relation":"contains"}
{"edge_id":7,"id":"hidden-number","source":"node-number","target":"node-number","relation":"contains"}
{"edge_id":"","id":"hidden-empty","source":"node-empty","target":"node-empty","relation":"contains"}
{"edge_id":"healthy","source":"first","target":"second","relation":"contains","description":"present"}
{"edge_id":"condition-miss","source":"third","target":"fourth","relation":"links_to"}
"#;

#[test]
fn generic_edge_self_loop_reports_record_id_with_canonical_precedence() {
    let config = r#"
[[rules]]
id="edge-loop"
severity="error"
kind="edge"
condition="source_id=target_id"
require_field="ignored"
message="loop {id}"
"#;
    let results = run(
        config,
        "",
        GENERIC_EDGE_RULE_RECORDS,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed);
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [
            Some("edge-only"),
            Some("edge-both"),
            Some("legacy-only"),
            None,
            None,
            None,
            None
        ]
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.message.as_str())
            .collect::<Vec<_>>(),
        [
            "loop edge-only",
            "loop edge-both",
            "loop legacy-only",
            "loop ",
            "loop ",
            "loop ",
            "loop "
        ]
    );
}

#[test]
fn generic_edge_required_field_reports_record_id_with_canonical_precedence() {
    let config = r#"
[[rules]]
id="edge-description"
kind="edge"
condition="relation=contains"
require_field="description"
message="required {id}"
"#;
    let results = run(
        config,
        "",
        GENERIC_EDGE_RULE_RECORDS,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed);
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [
            Some("edge-only"),
            Some("edge-both"),
            Some("legacy-only"),
            None,
            None,
            None,
            None
        ]
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.message.as_str())
            .collect::<Vec<_>>(),
        [
            "required edge-only",
            "required edge-both",
            "required legacy-only",
            "required ",
            "required ",
            "required ",
            "required "
        ]
    );
}

#[test]
fn generic_entity_required_field_keeps_id_despite_incidental_edge_id() {
    let config = r#"
[[rules]]
id="entity-description"
kind="entity"
condition="kind=concept"
require_field="description"
message="entity {id}"
"#;
    let entities = r#"{"id":"entity-id","edge_id":"edge-decoy","kind":"concept"}
{"edge_id":"missing-id-decoy","kind":"concept"}
{"id":"","edge_id":"empty-id-decoy","kind":"concept"}
{"id":"healthy","edge_id":"healthy-decoy","kind":"concept","description":"present"}
"#;
    let results = run(config, entities, "", "", EvaluationMode::FullDataset, &[]);
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed);
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("entity-id"), None, None]
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.message.as_str())
            .collect::<Vec<_>>(),
        ["entity entity-id", "entity ", "entity "]
    );
}

#[test]
fn generic_entity_self_loop_sentinel_keeps_existing_source_identity() {
    let config = r#"
[[rules]]
id="entity-loop"
kind="entity"
condition="source_id=target_id"
message="entity loop {id}"
"#;
    let entities = r#"{"id":"entity-id","edge_id":"edge-decoy","source":"node-id","target":"node-id"}
{}
"#;
    let results = run(config, entities, "", "", EvaluationMode::FullDataset, &[]);
    assert_eq!(results.len(), 1);
    assert!(!results[0].passed);
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("node-id"), Some("")]
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.message.as_str())
            .collect::<Vec<_>>(),
        ["entity loop node-id", "entity loop "]
    );
}
