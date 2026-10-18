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
        ["loop a", "loop b", "loop "]
    );
    assert_eq!(
        results[1].violations[0].entity_kind.as_deref(),
        Some("contains")
    );
    assert!(results[1].violations[2]
        .entity_id
        .as_ref()
        .unwrap()
        .is_empty());
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
