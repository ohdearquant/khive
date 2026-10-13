use super::*;
use crate::parsers::gql;

fn opts() -> CompileOptions {
    CompileOptions::default()
}

fn scoped(namespace: &str) -> CompileOptions {
    CompileOptions {
        scopes: vec![namespace.to_string()],
        max_limit: 500,
    }
}

#[test]
fn fixed_length_basic() {
    let q = gql::parse("MATCH (a:concept)-[e:introduced_by]->(b:paper) RETURN a, e, b LIMIT 10")
        .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("JOIN graph_edges"));
    assert!(compiled.sql.contains("LIMIT"));
    assert_eq!(
        compiled.return_vars,
        vec![
            ReturnItem::Variable("a".into()),
            ReturnItem::Variable("e".into()),
            ReturnItem::Variable("b".into()),
        ]
    );
    assert!(!compiled.sql.contains("WITH RECURSIVE"));
}

#[test]
fn namespace_scoping_injected() {
    let q = gql::parse("MATCH (a:concept)-[e:introduced_by]->(b:paper) RETURN a LIMIT 5").unwrap();
    let compiled = compile(&q, &scoped("research")).unwrap();
    assert!(compiled.sql.contains("namespace"));
    let has_ns_param = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "research"));
    assert!(has_ns_param, "namespace must be a bound parameter");
}

#[test]
fn edge_property_whitelist_rejects_unknown() {
    let q =
        gql::parse("MATCH (a)-[e:introduced_by]->(b) WHERE e.source_id = 'x' RETURN a").unwrap();
    let result = compile(&q, &opts());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("source_id") || err.contains("not queryable"),
        "error: {err}"
    );
}

#[test]
fn edge_property_relation_allowed() {
    let q = gql::parse("MATCH (a)-[e]->(b) WHERE e.relation = 'extends' RETURN a").unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "relation should be allowed: {:?}",
        result.err()
    );
}

#[test]
fn edge_property_weight_allowed() {
    let q = gql::parse("MATCH (a)-[e]->(b) WHERE e.weight > 0.5 RETURN a").unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "weight should be allowed: {:?}",
        result.err()
    );
}

#[test]
fn variable_length_uses_cte() {
    let q = gql::parse("MATCH (a {name: 'LoRA'})-[:extends*1..3]->(b) RETURN b LIMIT 20").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("WITH RECURSIVE"));
    assert!(compiled.sql.contains("traverse"));
}

#[test]
fn depth_cap_at_ten_rejects_above_max() {
    let q = gql::parse("MATCH (a)-[:extends*1..50]->(b) RETURN b").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)),
        "expected InvalidInput for depth > 10, got {err:?}"
    );
}

#[test]
fn depth_within_cap_compiles() {
    let q = gql::parse("MATCH (a)-[:extends*1..10]->(b) RETURN b").unwrap();
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
fn limit_capped_by_max_limit() {
    let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 1000").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    let limit_param = compiled.params.last().unwrap();
    assert!(
        matches!(limit_param, QueryValue::Integer(501)),
        "expected Integer(501), got {limit_param:?}"
    );
}

#[test]
fn limit_over_cap_requests_sentinel_row() {
    let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 1000").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.truncation_check,
        Some(TruncationCheck {
            max_limit: 500,
            requested_limit: Some(1000),
        })
    );
    let limit_param = compiled.params.last().unwrap();
    assert!(
        matches!(limit_param, QueryValue::Integer(501)),
        "expected sentinel LIMIT 501, got {limit_param:?}"
    );
}

#[test]
fn limit_exactly_at_cap_no_sentinel() {
    let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 500").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(compiled.truncation_check, None);
    let limit_param = compiled.params.last().unwrap();
    assert!(matches!(limit_param, QueryValue::Integer(500)));
}

#[test]
fn limit_below_cap_no_sentinel() {
    let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a LIMIT 100").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(compiled.truncation_check, None);
    let limit_param = compiled.params.last().unwrap();
    assert!(matches!(limit_param, QueryValue::Integer(100)));
}

#[test]
fn no_explicit_limit_requests_sentinel_row() {
    let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.truncation_check,
        Some(TruncationCheck {
            max_limit: 500,
            requested_limit: None,
        })
    );
    let limit_param = compiled.params.last().unwrap();
    assert!(
        matches!(limit_param, QueryValue::Integer(501)),
        "expected sentinel LIMIT 501, got {limit_param:?}"
    );
}

#[test]
fn fixed_length_skip_is_bound_after_a_total_identity_order() {
    let q = gql::parse("MATCH (a:concept)-[e]->(b) RETURN a SKIP 500").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("ORDER BY n0.substrate_kind, n0.id, e0.id, n1.substrate_kind, n1.id"),
        "fixed-length pages need a deterministic total identity order: {}",
        compiled.sql
    );
    assert!(compiled.sql.contains("LIMIT ?") && compiled.sql.contains("OFFSET ?"));
    assert!(
        compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Integer(500))),
        "SKIP must be a bound integer parameter: {:?}",
        compiled.params
    );
}

#[test]
fn variable_length_skip_totally_orders_distinct_projected_rows() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b SKIP 25").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
            compiled.sql.contains(
                r#"ORDER BY "_depth", "_total_weight" DESC, "b_id", "b_namespace", "b_kind", "b_entity_type", "b_name""#
            ),
            "recursive DISTINCT pages need a total projected-row order: {}",
            compiled.sql
        );
    assert!(compiled.sql.contains("LIMIT ?") && compiled.sql.contains("OFFSET ?"));
    assert!(
        compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Integer(25))),
        "SKIP must be a bound integer parameter: {:?}",
        compiled.params
    );
}

#[cfg(target_pointer_width = "64")]
#[test]
fn skip_over_sql_integer_range_is_rejected() {
    let too_large = (i64::MAX as u64) + 1;
    let q = gql::parse(&format!("MATCH (a:concept) RETURN a SKIP {too_large}")).unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::InvalidInput(_)) && err.to_string().contains("SKIP"),
        "unrepresentable SKIP must fail before SQL execution, got {err:?}"
    );
}

#[test]
fn variable_length_limit_over_cap_requests_sentinel_row() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b LIMIT 5000").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.truncation_check,
        Some(TruncationCheck {
            max_limit: 500,
            requested_limit: Some(5000),
        })
    );
    let limit_param = compiled.params.last().unwrap();
    assert!(
        matches!(limit_param, QueryValue::Integer(501)),
        "expected sentinel LIMIT 501, got {limit_param:?}"
    );
}

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

#[test]
fn variable_length_or_across_endpoints_rejected() {
    let q =
        gql::parse("MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR b.name = 'Y' RETURN a")
            .unwrap();
    let result = compile(&q, &opts());
    assert!(
        matches!(result, Err(QueryError::Unsupported(_))),
        "MAJ-1: OR spanning both endpoints must return Unsupported; got {result:?}"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("separate queries") || err_msg.contains("one endpoint"),
        "error must be actionable; got: {err_msg}"
    );
}

#[test]
fn variable_length_or_single_endpoint_still_works() {
    let q =
        gql::parse("MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR a.name = 'Y' RETURN a")
            .unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "single-endpoint OR must compile; got {result:?}"
    );
}

#[test]
fn variable_length_and_across_endpoints_still_works() {
    let q =
        gql::parse("MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' AND b.name = 'Y' RETURN a")
            .unwrap();
    let result = compile(&q, &opts());
    assert!(
        result.is_ok(),
        "AND across endpoints must compile; got {result:?}"
    );
}

#[test]
fn test_variable_length_or_compiles_to_or() {
    let q = gql::parse(
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(" OR "),
        "#379: variable-length single-endpoint OR must produce SQL OR; sql: {}",
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
fn test_single_endpoint_or_at_depth_1() {
    let q = gql::parse(
        "MATCH (a)-[r:extends]->(b) WHERE r.weight > 0.5 OR r.relation = 'extends' RETURN a",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(" OR "),
        "#379: fixed-length single-endpoint OR must produce SQL OR; sql: {}",
        compiled.sql
    );
    let has_extends = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "extends"));
    assert!(
        has_extends,
        "relation value 'extends' must be a bound param"
    );
}

#[test]
fn test_and_still_works() {
    let q = gql::parse(
        "MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'LoRA' AND a.kind = 'concept' RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled.sql.contains(" OR "),
        "#379: AND must not produce OR; sql: {}",
        compiled.sql
    );
    let has_lora = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "LoRA"));
    let has_concept = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "concept"));
    assert!(
        has_lora && has_concept,
        "both AND values must be bound params"
    );
}

/// Rejects row caps that cannot be represented by SQL's integer parameter.
#[test]
fn max_limit_overflow_returns_error() {
    let q = gql::parse("MATCH (a)-[:extends]->(b) RETURN a").unwrap();
    let opts = CompileOptions {
        scopes: vec![],
        max_limit: usize::MAX,
    };
    let result = compile(&q, &opts);
    match result {
        Err(QueryError::InvalidInput(_)) => {}
        Ok(compiled) => {
            let limit_param = compiled.params.last().unwrap();
            assert!(
                matches!(limit_param, QueryValue::Integer(n) if *n >= 0),
                "limit must never be negative; got {limit_param:?}"
            );
        }
        Err(e) => panic!("unexpected error: {e:?}"),
    }
}

/// A zero cap still permits one sentinel row for truncation detection.
#[test]
fn max_limit_zero_compiles() {
    let q = gql::parse("MATCH (a)-[:extends]->(b) RETURN a").unwrap();
    let opts = CompileOptions {
        scopes: vec![],
        max_limit: 0,
    };
    let compiled = compile(&q, &opts).unwrap();
    assert_eq!(
        compiled.truncation_check,
        Some(TruncationCheck {
            max_limit: 0,
            requested_limit: None,
        })
    );
    let limit_param = compiled.params.last().unwrap();
    assert!(
        matches!(limit_param, QueryValue::Integer(1)),
        "max_limit=0 should produce sentinel LIMIT 1; got {limit_param:?}"
    );
}

#[test]
fn variable_length_synthetic_edge_rejected() {
    let q = gql::parse("MATCH (ev)-[:observed_as_selected*1..3]->(m) RETURN m").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "variable-length synthetic edge must return Unsupported; got {err:?}"
    );
    assert!(
        err.to_string().contains("synthetic") || err.to_string().contains("observed_as"),
        "error should mention synthetic edges: {err}"
    );
}

/// Recursive traversal filters deleted intermediate nodes.
#[test]
fn variable_length_recursive_member_joins_next_node_for_deleted_filter() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
            compiled.sql.contains("JOIN primary_nodes next_node"),
            "recursive CTE must join primary_nodes next_node for deleted-intermediate filtering; sql: {}",
            compiled.sql
        );
    assert!(
        compiled.sql.contains("next_node.deleted_at IS NULL"),
        "recursive CTE must filter next_node.deleted_at IS NULL; sql: {}",
        compiled.sql
    );
}

/// Recursive traversal scopes intermediate nodes.
#[test]
fn variable_length_recursive_member_namespace_scopes_intermediates() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b").unwrap();
    let compiled = compile(&q, &scoped("test-ns")).unwrap();
    assert!(
        compiled.sql.contains("next_node.namespace"),
        "recursive CTE next_node join must filter namespace; sql: {}",
        compiled.sql
    );
}

/// Malformed public ASTs return errors rather than panicking.
#[test]
fn compile_malformed_ast_returns_error_not_panic() {
    use crate::ast::{EdgeDirection, EdgePattern, GqlQuery, MatchPattern, PatternElement};
    let q = GqlQuery {
        pattern: MatchPattern {
            elements: vec![PatternElement::Edge(EdgePattern {
                variable: None,
                relations: vec!["extends".to_string()],
                direction: EdgeDirection::Out,
                min_hops: 1,
                max_hops: 1,
            })],
        },
        where_clause: WhereExpr::True,
        return_items: vec![],
        offset: 0,
        limit: None,
    };
    let result = compile(&q, &opts());
    assert!(
        result.is_err(),
        "malformed AST (starts with Edge) must return error, not panic"
    );
}

/// Rejects an edge pattern missing the closing dash.
#[test]
fn edge_pattern_without_suffix_dash_rejected() {
    let result = gql::parse("MATCH (a)-[e:extends](b) RETURN a");
    assert!(
        result.is_err(),
        "edge pattern without suffix '-' must be rejected as a parse error"
    );
}

#[test]
fn assert_select_only_accepts_select_and_with() {
    assert!(
        assert_select_only("SELECT a FROM entities WHERE 1=1").is_ok(),
        "SELECT must be accepted"
    );
    assert!(
        assert_select_only("WITH RECURSIVE traverse AS (...) SELECT ...").is_ok(),
        "WITH must be accepted (recursive CTE)"
    );
}

#[test]
fn assert_select_only_rejects_write_sql_with_readonly_message() {
    for stmt in &[
        "INSERT INTO entities VALUES (?)",
        "UPDATE entities SET name = ?",
        "DELETE FROM entities WHERE id = ?",
        "DROP TABLE entities",
    ] {
        let err = assert_select_only(stmt).unwrap_err();
        assert!(
            matches!(err, QueryError::Compile(_)),
            "write SQL must return Compile error for '{stmt}'; got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("read-only"),
            "error must mention 'read-only' for '{stmt}'; got: {msg}"
        );
        assert!(
            msg.contains("create") && msg.contains("delete"),
            "error must name the mutation verbs for '{stmt}'; got: {msg}"
        );
    }
}

#[test]
fn readonly_guard_does_not_break_valid_gql_compile() {
    let q = gql::parse("MATCH (a:concept)-[:extends]->(b) RETURN a LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.starts_with("SELECT"),
        "valid GQL must compile to SELECT; sql: {}",
        compiled.sql
    );
}

#[test]
fn readonly_guard_does_not_break_valid_cte_compile() {
    let q = gql::parse("MATCH (a)-[:extends*1..3]->(b) RETURN b LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.starts_with("WITH RECURSIVE"),
        "variable-length GQL must compile to WITH RECURSIVE; sql: {}",
        compiled.sql
    );
}

#[test]
fn gql_write_form_rejected_before_compile() {
    use crate::parsers::gql;
    let err = gql::parse("CREATE (n:concept) RETURN n").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "GQL CREATE must be Unsupported; got {err:?}"
    );
    assert!(
        err.to_string().contains("read-only"),
        "error must mention read-only; got: {err}"
    );
}

#[test]
fn sparql_write_form_rejected_before_compile() {
    use crate::parsers::sparql;
    let err = sparql::parse("INSERT DATA { ?a :extends ?b }").unwrap_err();
    assert!(
        matches!(err, QueryError::Unsupported(_)),
        "SPARQL INSERT must be Unsupported; got {err:?}"
    );
    assert!(
        err.to_string().contains("read-only"),
        "error must mention read-only; got: {err}"
    );
}

#[test]
fn duplicate_inline_property_rejected() {
    let result = gql::parse("MATCH (n {name: 'A', name: 'B'}) RETURN n");
    assert!(
        result.is_err(),
        "duplicate property 'name' in node props must be rejected"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("duplicate") || err.contains("name"),
        "error should mention duplicate or key name: {err}"
    );
}

#[test]
fn unknown_synthetic_relation_rejected_at_compile() {
    let q = gql::parse("MATCH (a)-[:observed_as_bogus]->(b) RETURN a").unwrap();
    let err = compile(&q, &opts()).unwrap_err();
    assert!(
        matches!(err, QueryError::Validation(_)),
        "unknown synthetic relation must return Validation error; got {err:?}"
    );
}

/// Canonical `annotates` can bind every legal target substrate (issue #467).
#[test]
fn canonical_edge_annotates_compiles_note_source_and_any_target_substrate() {
    let q = gql::parse("MATCH (n)-[:annotates]->(x) RETURN n.id, x.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled.sql.contains("FROM entities n0"),
        "endpoint must not be hard-bound to entities only; sql: {}",
        compiled.sql
    );
    for table in ["FROM notes", "FROM events", "FROM graph_edges"] {
        assert!(
            compiled.sql.contains(table),
            "endpoint source must admit {table} rows for annotates; sql: {}",
            compiled.sql
        );
    }
}

/// Canonical `supports` can bind note-to-note endpoints.
#[test]
fn canonical_edge_supports_compiles_note_note_endpoints() {
    let q = gql::parse("MATCH (a)-[:supports]->(b) RETURN a.id, b.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.sql.matches("FROM notes").count(),
        2,
        "both supports endpoints must admit note rows; sql: {}",
        compiled.sql
    );
}

/// Pack-declared relations can bind task-note endpoints.
#[test]
fn canonical_edge_depends_on_compiles_pack_task_note_endpoints() {
    let q =
        gql::parse("MATCH (a:task)-[:depends_on]->(b:task) RETURN a.id, b.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert_eq!(
        compiled.sql.matches("FROM notes").count(),
        2,
        "task-kind depends_on endpoints must admit note rows; sql: {}",
        compiled.sql
    );
    let task_params = compiled
        .params
        .iter()
        .filter(|p| matches!(p, QueryValue::Text(s) if s == "task"))
        .count();
    assert_eq!(
        task_params, 2,
        "kind='task' must be bound for both endpoints"
    );
}

/// Recursive canonical traversal uses the primary-substrate union throughout.
#[test]
fn variable_length_canonical_compiles_primary_substrate_nodes() {
    let q = gql::parse("MATCH (a)-[:supports*1..2]->(b) RETURN b.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled.sql.contains("FROM entities s"),
        "seed must not be hard-bound to entities only; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("JOIN primary_nodes next_node"),
        "intermediate must join primary_nodes; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("JOIN primary_nodes r"),
        "final endpoint must join primary_nodes; sql: {}",
        compiled.sql
    );
}

/// `observed_as_target` admits entity and note referents (issue #468).
#[test]
fn synthetic_edge_observed_as_target_compiles_entity_referents() {
    let q = gql::parse("MATCH (ev)-[:observed_as_target]->(t) RETURN t.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        !compiled
            .sql
            .contains("JOIN notes n1 ON n1.id = e0.entity_id AND e0.referent_kind = 'note'"),
        "target join must not be hard-bound to notes; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("FROM entities"),
        "observation target source must admit entity referents; sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("n1.referent_kind = e0.referent_kind"),
        "target join must discriminate by referent_kind instead of hard-coding 'note'; sql: {}",
        compiled.sql
    );
}

/// `observed_as_signal` admits entity and note referents.
#[test]
fn synthetic_edge_observed_as_signal_compiles_entity_and_note_referents() {
    let q = gql::parse("MATCH (ev)-[:observed_as_signal]->(t) RETURN t.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .sql
            .contains("e0.role = 'signal' AND e0.referent_kind IN ('entity', 'note')"),
        "signal role must be admitted against entity or note referents; sql: {}",
        compiled.sql
    );
}

/// Search observations may select entities; recall and rerank still emit note referents.
#[test]
fn synthetic_edge_candidate_and_selected_admit_entity_and_note_referents() {
    let q =
        gql::parse("MATCH (ev)-[:observed_as_selected]->(m:entity) RETURN m.id LIMIT 10").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains(
            "e0.role IN ('candidate', 'selected') AND e0.referent_kind IN ('entity', 'note')"
        ),
        "selected/candidate roles must follow the stored entity/note referent kind; sql: {}",
        compiled.sql
    );
}

#[test]
fn where_is_null_compiles_to_is_null_sql() {
    let q = gql::parse("MATCH (n:entity) WHERE n.name IS NULL RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("IS NULL") && !compiled.sql.contains("IS NOT NULL"),
        "expected IS NULL (not IS NOT NULL) in sql: {}",
        compiled.sql
    );
}

#[test]
fn where_is_not_null_compiles_to_is_not_null_sql() {
    let q = gql::parse("MATCH (n:entity) WHERE n.name IS NOT NULL RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled.sql.contains("IS NOT NULL"),
        "expected IS NOT NULL in sql: {}",
        compiled.sql
    );
}
