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
    let err = where_property_expression(
        &PropertyRef::JsonPath(Vec::new()),
        &VarKind::Node,
        "n0",
        &mut Vec::new(),
    )
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
        &mut Vec::new(),
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
            &mut Vec::new(),
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
fn variable_length_trailing_pattern_compiles() {
    let q = gql::parse("MATCH (a {name: 'root'})-[e:extends*1..3]->(b)-[f:implements]->(c {name: 'attachment'}) RETURN a.name, b.name, c.name, e.id, f.id").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    for value in ["root", "attachment", "extends", "implements"] {
        assert!(
            compiled
                .params
                .iter()
                .any(|p| matches!(p, QueryValue::Text(s) if s == value)),
            "missing parameter {value}"
        );
    }
    for alias in ["a_name", "b_name", "c_name", "e_id", "f_id"] {
        assert!(
            compiled.sql.contains(&format!("AS \"{alias}\"")),
            "{}",
            compiled.sql
        );
    }
}

#[test]
fn variable_length_mixed_chain_compiles() {
    let q = gql::parse("MATCH (a {name: 'prefix'})-[f:extends]->(b)-[e:implements*1..2]->(c {name: 'leaf'}) RETURN a.name, b.id, c.name, f.id, e.id").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    for value in ["prefix", "leaf", "extends", "implements"] {
        assert!(
            compiled
                .params
                .iter()
                .any(|p| matches!(p, QueryValue::Text(s) if s == value)),
            "missing parameter {value}"
        );
    }
    for alias in ["a_name", "b_id", "c_name", "f_id", "e_id"] {
        assert!(
            compiled.sql.contains(&format!("AS \"{alias}\"")),
            "{}",
            compiled.sql
        );
    }
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
#[test]
fn mixed_chain_order_and_projection_keep_per_binding_contracts() {
    let query = gql::parse("MATCH (a)-[e:extends*0..2]->(b)-[f:implements]->(c) RETURN e, f, e.metadata.rank, f.metadata.rank, c.properties.label ORDER BY e.metadata.rank, f.metadata.rank").unwrap();
    let result = compile(&query, &opts()).unwrap();
    for alias in [
        "e_id",
        "e_relation",
        "e_weight",
        "f_id",
        "f_source",
        "f_target",
        "f_relation",
        "f_weight",
        "e_metadata_rank",
        "f_metadata_rank",
        "c_properties_label",
        "_depth",
        "_total_weight",
    ] {
        assert!(
            result.sql.contains(&format!("AS \"{alias}\"")),
            "missing {alias}: {}",
            result.sql
        );
    }
    for alias in ["e_source", "e_target", "e_source_id", "e_target_id"] {
        assert!(
            !result.sql.contains(&format!("AS \"{alias}\"")),
            "{}",
            result.sql
        );
    }
    for path in ["$.rank", "$.label"] {
        assert!(result
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == path)));
        assert!(
            !result.sql.contains(path),
            "projection JSON paths must remain bound: {}",
            result.sql
        );
    }
    for source in [
        "MATCH (a)-[e:extends*0..2]->(b)-[f:implements]->(c) RETURN e.source_id",
        "MATCH (a)-[e:extends*0..2]->(b)-[f:implements]->(c) RETURN c.name ORDER BY e.metadata.rank",
        "MATCH (a)-[e:extends*0..2]->(b)-[f:implements]->(c) RETURN e.metadata.a_b, e.metadata.a.b",
    ] {
        assert!(compile(&gql::parse(source).unwrap(), &opts()).is_err(), "{source}");
    }
    for source in [
        "MATCH (a)-[:observed_as_selected]->(b)-[:extends*1..2]->(c) RETURN c.id",
        "MATCH (a)-[:extends*1..2]->(b)-[:observed_as_selected]->(c) RETURN c.id",
    ] {
        assert!(
            matches!(
                compile(&gql::parse(source).unwrap(), &opts()),
                Err(QueryError::Unsupported(_))
            ),
            "{source}"
        );
    }
}

#[test]
fn mixed_or_acceptance_preserves_single_segment_phase_refusal() {
    let mixed = gql::parse("MATCH (a)-[f:contains]->(b)-[e:extends*1..2]->(c) WHERE f.metadata.pick = 1 OR e.metadata.pick = 1 RETURN c.name").unwrap();
    compile(&mixed, &opts()).unwrap();
    let single = gql::parse(
        "MATCH (a)-[e:extends*1..2]->(b) WHERE a.name = 'x' OR b.name = 'y' RETURN b.name",
    )
    .unwrap();
    let error = compile(&single, &opts()).unwrap_err();
    assert!(matches!(error, QueryError::Unsupported(_)), "{error:?}");
    assert!(error.to_string().contains("OR"));
    assert!(
        error.to_string().contains("start") && error.to_string().contains("final"),
        "{error}"
    );
}
