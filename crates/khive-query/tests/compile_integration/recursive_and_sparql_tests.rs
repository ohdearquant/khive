// --- Variable-length compilation ---

#[test]
fn variable_length_uses_cte() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a {name: 'LoRA'})-[:extends*1..3]->(b) RETURN b LIMIT 20",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("WITH RECURSIVE"));
    assert!(compiled.sql.contains("traverse"));
}

#[test]
fn depth_cap_at_ten_rejects_above_max() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..50]->(b) RETURN b",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)),
        "expected InvalidInput for depth > 10, got {err:?}"
    );
}

#[test]
fn depth_within_cap_compiles() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..10]->(b) RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("WITH RECURSIVE"));
    let depth_val = compiled.params.iter().find_map(|p| {
        if let QueryValue::Integer(n) = p {
            Some(*n)
        } else {
            None
        }
    });
    assert_eq!(depth_val, Some(10), "depth param should be 10");
}

#[test]
fn variable_length_return_start_only_joins_end_entity() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[:extends*1..3]->(b) RETURN a LIMIT 10",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("JOIN primary_nodes r"),
        "primary_nodes r must always be joined; sql: {}",
        compiled.sql
    );
}

#[test]
fn variable_length_trailing_pattern_unsupported() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..3]->(b)-[:implements]->(c) RETURN b",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "expected Unsupported, got {err:?}"
    );
}

#[test]
fn variable_length_mixed_chain_unsupported() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends]->(b)-[:implements*1..2]->(c) RETURN c",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
}

// --- SPARQL ---

#[test]
fn sparql_star_rejected_as_unsupported() {
    let err = parse(
        QueryLanguage::Sparql,
        "SELECT ?a ?b WHERE { ?a :extends* ?b . }",
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::Unsupported(_)), "got {err:?}");
}

#[test]
fn sparql_subject_object_direction_compiles_outbound() {
    let q = parse(
        QueryLanguage::Sparql,
        "SELECT ?a ?b WHERE { ?a :extends ?b . }",
    )
    .unwrap();
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
}

// --- WHERE OR support ---

#[test]
fn where_or_compiles_to_sql_or() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(" OR "),
        "WHERE OR must produce SQL OR; sql: {}",
        compiled.sql
    );
    let has_lora = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "LoRA"));
    let has_qlora = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "QLoRA"));
    assert!(has_lora && has_qlora, "both OR values must be bound params");
}

#[test]
fn where_and_or_precedence() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(" OR "),
        "expected OR in sql; sql: {}",
        compiled.sql
    );
}
