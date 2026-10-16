#[test]
fn compile_rejects_unknown_relation() {
    let q = gql::parse("MATCH (a)-[:not_a_relation]->(b) RETURN a").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not_a_relation"), "msg: {msg}");
}

#[test]
fn compile_unknown_kind_passes_through() {
    let q = gql::parse("MATCH (a:gizmo)-[:extends]->(b) RETURN a").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    let has_gizmo = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "gizmo"));
    assert!(
        has_gizmo,
        "pack-agnostic: unknown kind must pass through into SQL params"
    );
}

#[test]
fn compile_kind_passes_through_unchanged() {
    let q = gql::parse("MATCH (a:paper)-[:introduced_by]->(b:concept) RETURN a LIMIT 1").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    let has_paper = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
    assert!(
        has_paper,
        "kind 'paper' must pass through unchanged into SQL params"
    );
}

#[test]
fn compile_rejects_namespace_in_where() {
    let q = gql::parse("MATCH (a:concept)-[:extends]->(b) WHERE a.namespace = 'other' RETURN a")
        .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(err.to_string().contains("namespace"), "msg: {err}");
}

#[test]
fn compile_explicit_nested_json_property_in_where() {
    let q = gql::parse(
        "MATCH (a)-[:extends]->(b) \
             WHERE a.properties.finding.severity = 'high' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("json_extract(n0.properties, '$.finding.severity') = ?"),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn compile_rejects_unknown_unqualified_where_field() {
    let q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.severity = 'high' RETURN a").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg)
                if msg.contains("unknown node field 'severity'")
                    && msg.contains("properties.<path>")),
        "got {err:?}"
    );
}

#[test]
fn compile_rejects_bare_properties_where_field() {
    let q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.properties = '{}' RETURN a").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg)
                if msg.contains("reserved as the JSON path root")),
        "got {err:?}"
    );
}

#[test]
fn where_json_path_rejects_empty_hand_built_path() {
    let err = where_property_expression(&PropertyRef::JsonPath(Vec::new()), &VarKind::Node, "n0")
        .unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg) if msg.contains("cannot be empty")),
        "got {err:?}"
    );
}

#[test]
fn where_json_path_rejects_injection_shaped_hand_built_segment() {
    let err = where_property_expression(
        &PropertyRef::JsonPath(vec!["severity') OR 1=1 --".into()]),
        &VarKind::Node,
        "n0",
    )
    .unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg)
                if msg.contains("invalid JSON property path segment")),
        "got {err:?}"
    );
}

#[test]
fn where_json_path_rejects_edge_and_event_bindings() {
    for kind in [VarKind::Edge, VarKind::EventNode] {
        let err = where_property_expression(
            &PropertyRef::JsonPath(vec!["severity".into()]),
            &kind,
            "bound",
        )
        .unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(ref msg)
                    if msg.contains("do not expose JSON properties")),
            "got {err:?}"
        );
    }
}

#[test]
fn compile_rejects_unknown_relation_in_where() {
    let q =
        gql::parse("MATCH (a)-[e:extends]->(b) WHERE e.relation = 'related_to' RETURN a").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(err.to_string().contains("related_to"), "msg: {err}");
}

#[test]
fn compile_kind_in_where_passes_through_unchanged() {
    let q = gql::parse("MATCH (a)-[:extends]->(b) WHERE a.kind = 'paper' RETURN a").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    let has_paper = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
    assert!(
        has_paper,
        "kind 'paper' must pass through unchanged into SQL params"
    );
}

#[test]
fn variable_length_return_start_only_joins_end_entity() {
    let q = gql::parse("MATCH (a:concept)-[:extends*1..3]->(b) RETURN a LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("JOIN primary_nodes r"),
        "primary_nodes r must always be joined when r.* conditions are emitted; sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_trailing_pattern_unsupported() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b)-[:implements]->(c) RETURN b").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn variable_length_mixed_chain_unsupported() {
    let q = gql::parse("MATCH (a)-[:extends]->(b)-[:implements*1..2]->(c) RETURN c").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
}

#[test]
fn sparql_star_rejected_as_unsupported() {
    use crate::parsers::sparql;
    let err = sparql::parse("SELECT ?a ?b WHERE { ?a :extends* ?b . }").unwrap_err();
    assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
}

/// Preserves SPARQL subject-to-object edge direction (issue #231).
#[test]
fn sparql_subject_object_direction_compiles_outbound() {
    use crate::parsers::sparql;

    let q = sparql::parse("SELECT ?a ?b WHERE { ?a :extends ?b . }").unwrap();
    let compiled = compile(&q, &opts()).unwrap();

    assert!(
        compiled
            .sql
            .contains("JOIN graph_edges e0 ON e0.source_id = n0.id"),
        "SPARQL subject must bind graph_edges.source_id; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("ON n1.id = e0.target_id"),
        "SPARQL object must bind graph_edges.target_id; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("e0.relation = ?1"),
        "SPARQL predicate must bind graph_edges.relation; sql: {}",
        compiled.sql
    );
}
