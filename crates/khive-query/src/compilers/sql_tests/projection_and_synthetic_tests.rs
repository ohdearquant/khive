#[test]
fn return_property_projection_compiles() {
    let q = gql::parse("MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 5")
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
    let q = gql::parse("MATCH (a:concept)-[:extends]->(b) RETURN a.domain LIMIT 5").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown node property 'domain'")),
        "got {err:?}"
    );
}

#[test]
fn return_unknown_edge_property_rejected() {
    let q = gql::parse("MATCH (a)-[e:extends]->(b) RETURN e.label LIMIT 5").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(ref msg) if msg.contains("unknown edge property 'label'")),
        "got {err:?}"
    );
}

#[test]
fn return_valid_edge_property_compiles() {
    let q = gql::parse("MATCH (a)-[e:extends]->(b) RETURN e.relation, e.weight LIMIT 5").unwrap();
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

#[test]
fn documented_single_path_parallel_edge_audit_compiles_exact_projection_aliases() {
    const QUERY: &str = "MATCH (a)-[e]->(b) RETURN a.id, e.id, e.relation, b.id";

    let query = gql::parse(QUERY).unwrap();
    let compiled = compile(&query, &opts()).unwrap();
    let select_list = compiled
        .sql
        .strip_prefix("SELECT ")
        .and_then(|sql| sql.split_once(" FROM ").map(|(select, _)| select))
        .unwrap_or_else(|| {
            panic!(
                "compiled query must be a SELECT statement: {}",
                compiled.sql
            )
        });
    let aliases: Vec<&str> = select_list
        .split(", ")
        .map(|projection| {
            projection
                .rsplit_once(" AS ")
                .map(|(_, alias)| alias)
                .unwrap_or_else(|| panic!("projection must have an alias: {projection}"))
        })
        .collect();

    assert_eq!(
        aliases,
        [r#""a_id""#, r#""e_id""#, r#""e_relation""#, r#""b_id""#]
    );
}

#[test]
fn entity_type_compiles_as_direct_column_not_json_extract() {
    let q =
        gql::parse("MATCH (n:document {entity_type: 'paper'})-[:extends]->(m) RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(".entity_type = ?"),
        "entity_type must compile to a direct column comparison; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("json_extract"),
        "entity_type must NOT use json_extract; sql: {}",
        compiled.sql
    );
    let has_paper_param = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "paper"));
    assert!(
        has_paper_param,
        "entity_type value 'paper' must appear as a bound parameter"
    );
}

#[test]
fn where_or_compiles_to_sql_or() {
    let q = gql::parse(
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
    let q = gql::parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a"
        ).unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(" OR "),
        "expected OR in sql; sql: {}",
        compiled.sql
    );
}

#[test]
fn synthetic_edge_joins_event_observations() {
    let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN ev, m").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("event_observations"),
        "synthetic edge must join event_observations; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("graph_edges"),
        "synthetic edge must NOT join graph_edges; sql: {}",
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
    let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m:memory) RETURN ev, m").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("FROM events "),
        "CRIT-1: event source must come FROM events table, not entities; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled
            .sql
            .starts_with("SELECT * FROM entities n0 JOIN event_observations"),
        "CRIT-1: must not join events via entities table; sql: {}",
        compiled.sql
    );
}

#[test]
fn synthetic_edge_event_observation_join_uses_events_id() {
    let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m) RETURN m").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("JOIN event_observations e0 ON e0.event_id = n0.id"),
        "CRIT-1: event_observations must join on events.id (n0 is now events); sql: {}",
        compiled.sql
    );
}

#[test]
fn synthetic_edge_event_node_projects_event_columns() {
    let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m) RETURN ev").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("ev_verb"),
        "CRIT-1: event variable must project verb column; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("ev_outcome"),
        "CRIT-1: event variable must project outcome column; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("ev_name,") && !compiled.sql.contains("ev_name "),
        "CRIT-1: event variable must NOT project entity name column; sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("ev_properties"),
        "CRIT-1: event variable must NOT project entity properties column; sql: {}",
        compiled.sql
    );
}

#[test]
fn synthetic_edge_namespace_filter_on_events_table() {
    let q = gql::parse("MATCH (ev)-[:observed_as_selected]->(m) RETURN m").unwrap();
    let compiled = compile(&q, &scoped("test-ns")).unwrap();
    let ns_count = compiled
        .params
        .iter()
        .filter(|p| matches!(p, QueryValue::Text(s) if s == "test-ns"))
        .count();
    assert!(
        ns_count >= 2,
        "MIN-2: namespace must be filtered on both events and target; params: {:?}",
        compiled.params
    );
}

#[test]
fn synthetic_edge_candidate_role() {
    let q = gql::parse("MATCH (ev)-[:observed_as_candidate]->(m) RETURN ev, m").unwrap();
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
    let q = gql::parse("MATCH (ev)-[:observed_as_candidate|observed_as_selected]->(m) RETURN m")
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
    let q = gql::parse("MATCH (ev)-[:observed_as_selected|extends]->(m) RETURN m").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(_)),
        "mixed synthetic+canonical must be rejected; got {err:?}"
    );
}

#[test]
fn synthetic_edge_inbound_rejected() {
    let q = gql::parse("MATCH (m)<-[:observed_as_selected]-(ev) RETURN m").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Compile(_)),
        "inbound synthetic edge must be rejected; got {err:?}"
    );
}
