// --- Fixed-length compilation ---

#[test]
fn edge_property_relation_allowed() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[e]->(b) WHERE e.relation = 'extends' RETURN a",
    )
    .unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "relation should be allowed: {:?}",
        result.err()
    );
}

#[test]
fn edge_property_weight_allowed() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[e]->(b) WHERE e.weight > 0.5 RETURN a",
    )
    .unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "weight should be allowed: {:?}",
        result.err()
    );
}

#[test]
fn compile_unknown_kind_passes_through() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:gizmo)-[:extends]->(b) RETURN a",
    )
    .unwrap();
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
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:paper)-[:introduced_by]->(b:concept) RETURN a LIMIT 1",
    )
    .unwrap();
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
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends]->(b) WHERE a.namespace = 'other' RETURN a",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(err.to_string().contains("namespace"), "msg: {err}");
}

#[test]
fn compile_rejects_unknown_relation_in_where() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[e:extends]->(b) WHERE e.relation = 'related_to' RETURN a",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(err.to_string().contains("related_to"), "msg: {err}");
}

#[test]
fn compile_resolves_explicit_node_property_path_in_where() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends]->(b) WHERE a.properties.domain = 'query' RETURN a.name",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("json_extract(n0.properties, '$.domain') = ?"),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn fixed_length_where_unknown_unqualified_field_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends]->(b) WHERE a.domain = 'query' RETURN a.name",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg)
            if msg.contains("unknown node field 'domain'")
                && msg.contains("properties.<path>")),
        "got {err:?}"
    );
}

#[test]
fn compile_kind_in_where_passes_through_unchanged() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends]->(b) WHERE a.kind = 'paper' RETURN a",
    )
    .unwrap();
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
fn return_property_projection_compiles() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 5",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(r#".name AS "a_name""#),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains(r#".name AS "b_name""#),
        "sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("a_kind"),
        "should not emit full node columns"
    );
}

#[test]
fn return_unknown_node_property_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends]->(b) RETURN a.domain LIMIT 5",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown node property 'domain'")),
        "got {err:?}"
    );
}

#[test]
fn return_unknown_depth_property_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:precedes*1..3]->(b) RETURN b._depth",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown node property '_depth'") && msg.contains("Valid:")),
        "got {err:?}"
    );
}

#[test]
fn return_unknown_edge_property_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[e:extends]->(b) RETURN e.label LIMIT 5",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown edge property 'label'")),
        "got {err:?}"
    );
}

#[test]
fn return_valid_edge_property_compiles() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[e:extends]->(b) RETURN e.relation, e.weight LIMIT 5",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(r#".relation AS "e_relation""#),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains(r#".weight AS "e_weight""#),
        "sql: {}",
        compiled.sql
    );
}
