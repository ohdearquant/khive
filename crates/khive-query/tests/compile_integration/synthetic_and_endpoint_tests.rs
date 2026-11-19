// --- Synthetic edge compilation (ADR-041) ---

#[test]
fn synthetic_edge_joins_event_observations() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN ev, m",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("event_observations"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("graph_edges"),
        "sql: {}",
        compiled.sql
    );
    let has_role_param = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "selected"));
    assert!(has_role_param, "role 'selected' must be a bound parameter");
}

#[test]
fn synthetic_edge_event_source_binds_events_table() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN ev, m",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("FROM events "),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn synthetic_edge_event_node_projects_event_columns() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_selected]->(m) RETURN ev",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("ev_verb"), "sql: {}", compiled.sql);
    assert!(compiled.sql.contains("ev_outcome"), "sql: {}", compiled.sql);
    assert!(
        !compiled.sql.contains("ev_name,") && !compiled.sql.contains("ev_name "),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn synthetic_edge_namespace_filter_on_events_table() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_selected]->(m) RETURN m",
    )
    .unwrap();
    let compiled = compile(&q, &scoped("test-ns")).unwrap();
    let ns_count = compiled
        .params
        .iter()
        .filter(|p| matches!(p, QueryValue::Text(s) if s == "test-ns"))
        .count();
    assert!(
        ns_count >= 2,
        "namespace must be filtered on both events and target; params: {:?}",
        compiled.params
    );
}

#[test]
fn synthetic_edge_candidate_role() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_candidate]->(m) RETURN ev, m",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("event_observations"),
        "sql: {}",
        compiled.sql
    );
    let has_candidate = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "candidate"));
    assert!(has_candidate, "role 'candidate' must be bound");
}

#[test]
fn synthetic_edge_multi_role() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_candidate|observed_as_selected]->(m) RETURN m",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("event_observations"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("IN"),
        "multi-role must use IN; sql: {}",
        compiled.sql
    );
}

#[test]
fn mixed_synthetic_and_canonical_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (ev)-[:observed_as_selected|extends]->(m) RETURN m",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(matches!(err, QueryError::Compile(_)), "got {err:?}");
}

#[test]
fn synthetic_edge_inbound_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (m)<-[:observed_as_selected]-(ev) RETURN m",
    )
    .unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(matches!(err, QueryError::Compile(_)), "got {err:?}");
}

// --- Variable-length OR ---

#[test]
fn variable_length_or_across_endpoints_rejected() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR b.name = 'Y' RETURN a",
    )
    .unwrap();
    let result = compile(&q, &opts());
    assert!(
        matches!(result, Err(QueryError::Unsupported(_))),
        "got {result:?}"
    );
}

#[test]
fn variable_length_or_single_endpoint_still_works() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR a.name = 'Y' RETURN a",
    )
    .unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "single-endpoint OR must compile; got {result:?}"
    );
}

#[test]
fn variable_length_and_across_endpoints_still_works() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' AND b.name = 'Y' RETURN a",
    )
    .unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "AND across endpoints must compile; got {result:?}"
    );
}

#[test]
fn test_variable_length_or_compiles_to_or() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains(" OR "), "sql: {}", compiled.sql);
}

#[test]
fn test_single_endpoint_or_at_depth_1() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[r:extends]->(b) WHERE r.weight > 0.5 OR r.relation = 'extends' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains(" OR "), "sql: {}", compiled.sql);
}

#[test]
fn test_and_still_works() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'LoRA' AND a.kind = 'concept' RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(!compiled.sql.contains(" OR "), "sql: {}", compiled.sql);
}

// --- parse_auto ---

#[test]
fn parse_auto_gql() {
    let q = parse_auto("MATCH (a:concept)-[:extends]->(b) RETURN b LIMIT 5").unwrap();
    assert_eq!(q.return_items, vec![ReturnItem::Variable("b".into())]);
}

#[test]
fn parse_auto_sparql() {
    let q = parse_auto("SELECT ?a ?b WHERE { ?a :extends ?b . }").unwrap();
    assert_eq!(
        q.return_items,
        vec![
            ReturnItem::Variable("a".into()),
            ReturnItem::Variable("b".into()),
        ]
    );
}
