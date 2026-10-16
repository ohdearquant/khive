#[test]
fn endpoint_validation_uses_the_shared_table_and_supplied_subtype_rules() {
    let entities = r#"{"id":"c","kind":"concept"}
{"id":"p","kind":"project"}
{"id":"typed","kind":"document","entity_type":"paper"}
{"id":"plain","kind":"document"}
"#;
    let notes = r#"{"id":"n","kind":"observation"}"#;
    let edges = r#"{"source":"p","target":"c","relation":"implements"}
{"source":"c","target":"p","relation":"implements"}
{"source":"typed","target":"n","relation":"contains"}
{"source":"plain","target":"n","relation":"contains"}
{"source":"missing","target":"c","relation":"contains"}
{"source":"c","target":"p","relation":"not-a-relation"}
"#;
    let pack_rules = [EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::EntityOfType {
            kind: "document",
            entity_type: "paper",
        },
        target: EndpointKind::NoteOfKind("observation"),
    }];
    let results = run(
        "[edge_endpoint_types]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &pack_rules,
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("c"), Some("plain")]
    );
    assert_eq!(results[0].violations[0].message, "[c→p] (entity concept) -[implements]-> (entity project) is not a permitted endpoint pairing for this relation");
    let without = run(
        "[edge_endpoint_types]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(without[0].violations.len(), 3);
}

#[test]
fn resolved_edge_endpoints_and_special_relations_retain_substrate_rules() {
    let entities = r#"{"id":"c","kind":"concept"}"#;
    let notes = "{\"id\":\"n\",\"kind\":\"observation\"}\n{\"id\":\"m\",\"kind\":\"decision\"}";
    let edges = r#"{"edge_id":"x","source":"n","target":"c","relation":"annotates"}
{"source":"n","target":"x","relation":"annotates"}
{"source":"c","target":"x","relation":"annotates"}
{"source":"n","target":"m","relation":"supports"}
{"source":"n","target":"c","relation":"supports"}
{"source":"x","target":"x","relation":"supersedes"}
{"source":"x","target":"c","relation":"contains"}
"#;
    let results = run(
        "[edge_endpoint_types]\n[dangling_refs]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("c"), Some("n"), Some("x"), Some("x")]
    );
    assert!(
        results[1].passed,
        "edge IDs remain known dangling-reference targets"
    );
}

#[test]
fn duplicate_kind_map_prefers_last_note_then_preserves_it_over_edge_id() {
    let entities = "{\"id\":\"x\",\"kind\":\"concept\"}\n{\"id\":\"target\",\"kind\":\"concept\"}";
    let notes = "{\"id\":\"x\",\"kind\":\"observation\"}\n{\"id\":\"x\",\"kind\":\"decision\"}";
    let edges = r#"{"edge_id":"x","source":"x","target":"target","relation":"contains"}"#;
    let pack = [EdgeEndpointRule {
        relation: EdgeRelation::Contains,
        source: EndpointKind::NoteOfKind("decision"),
        target: EndpointKind::EntityOfKind("concept"),
    }];
    assert!(
        run(
            "[edge_endpoint_types]",
            entities,
            edges,
            notes,
            EvaluationMode::FullDataset,
            &pack
        )[0]
        .passed
    );
    let wrong = [EdgeEndpointRule {
        source: EndpointKind::NoteOfKind("observation"),
        ..pack[0]
    }];
    let results = run(
        "[edge_endpoint_types]",
        entities,
        edges,
        notes,
        EvaluationMode::FullDataset,
        &wrong,
    );
    assert_eq!(
        results[0].violations[0].entity_kind.as_deref(),
        Some("decision")
    );
}

#[test]
fn direction_forward_match_wins_and_reversed_alias_edges_report_in_input_order() {
    let config = r#"
[edge_direction_conventions]
[[edge_direction_conventions.relations]]
relation="implements"
forward_source_kinds=["project"]
forward_target_kinds=["concept"]
[[edge_direction_conventions.relations]]
relation="contains"
forward_source_kinds=["concept", "project"]
forward_target_kinds=["concept", "project"]
"#;
    let entities = "{\"id\":\"c\",\"kind\":\"concept\"}\n{\"id\":\"p\",\"kind\":\"project\"}";
    let edges = r#"{"source":"p","target":"c","relation":"implements"}
{"source_id":"c","target_id":"p","relation":"implements"}
{"source":"c","target":"p","relation":"contains"}
{"source":"absent","target":"p","relation":"implements"}
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
    assert_eq!(results[0].violations[0].message, "[c→p] implements from concept to project matches the reversed direction convention configured for this relation; likely inverted");
}

#[test]
fn dangling_scan_preserves_edge_order_source_before_target_and_edge_id_aliases() {
    let edges = r#"{"edge_id":"x","source":"z","target":"a"}
{"id":"legacy","source_id":"x","target_id":"legacy"}
{"source":"b","target":"x"}
"#;
    let results = run(
        "[dangling_refs]",
        "",
        edges,
        "",
        EvaluationMode::FullDataset,
        &[],
    );
    assert_eq!(
        results[0]
            .violations
            .iter()
            .map(|v| v.entity_id.as_deref())
            .collect::<Vec<_>>(),
        [Some("z"), Some("a"), Some("b")]
    );
    assert!(results[0].violations[0]
        .message
        .starts_with("edge source z"));
    assert!(results[0].violations[1]
        .message
        .starts_with("edge target a"));
}
