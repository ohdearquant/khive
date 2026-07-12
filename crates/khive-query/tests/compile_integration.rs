//! Integration tests for the SQL compiler through the public API.
//!
//! Tests cover fixed-length, variable-length, synthetic edge, and WHERE clause
//! compilation paths. Formerly inline in `compilers/sql.rs`; moved here per
//! QUERY-AUD-002.

use khive_query::ast::{QueryValue, ReturnItem};
use khive_query::{
    compile, parse, parse_auto, CompileOptions, CompiledQuery, QueryError, QueryLanguage,
};

fn opts() -> CompileOptions {
    CompileOptions::default()
}

fn scoped(namespace: &str) -> CompileOptions {
    CompileOptions {
        scopes: vec![namespace.to_string()],
        max_limit: 500,
    }
}

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
        compiled.sql.contains(".name AS a_name"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains(".name AS b_name"),
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
        compiled.sql.contains(".relation AS e_relation"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains(".weight AS e_weight"),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn entity_type_compiles_as_direct_column_not_json_extract() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {entity_type: 'paper'})-[:extends]->(m) RETURN n",
    )
    .unwrap();
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

// --- Issue #755: inline property-map integer literals ---

#[test]
fn gql_inline_property_map_accepts_integer_literal() {
    // Previously a parse error: "expected string literal".
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {number: 54}) RETURN n",
    );
    assert!(q.is_ok(), "integer literal must parse: {:?}", q.err());
}

#[test]
fn gql_inline_property_map_accepts_negative_integer_literal() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {offset: -54}) RETURN n",
    );
    assert!(
        q.is_ok(),
        "negative integer literal must parse: {:?}",
        q.err()
    );
}

#[test]
fn gql_inline_property_map_integer_compiles_to_numeric_param() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {number: 54}) RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();

    let has_numeric_param = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Integer(54)));
    assert!(
        has_numeric_param,
        "integer literal must bind as QueryValue::Integer, not Float or text \
         (Float loses precision past 2^53 -- issue #832); params: {:?}",
        compiled.params
    );

    let number_predicate = compiled
        .sql
        .lines()
        .find(|l| l.contains("'$.number'"))
        .unwrap_or(&compiled.sql);
    assert!(
        !number_predicate.contains("COLLATE NOCASE"),
        "numeric comparison must not use text COLLATE NOCASE; sql: {}",
        compiled.sql
    );
}

#[test]
fn gql_inline_property_map_integer_matches_json_number_not_json_string() {
    // Regression for the root cause: json_extract() returns a JSON number as a
    // SQLite numeric storage class. Binding it as QueryValue::Text produced a
    // predicate that could never match (TEXT vs INTEGER/REAL never compare
    // equal in SQLite), which is exactly the silent-mismatch bug in #755.
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {number: 54}) RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Integer(_) | QueryValue::Float(_))),
        "must bind a numeric parameter so it compares equal to json_extract's \
         numeric result; params: {:?}",
        compiled.params
    );
}

#[test]
fn gql_inline_property_map_quoted_number_still_binds_as_text() {
    // Decided behavior (documented in PR body for #755): a quoted numeric
    // string in an inline property map is a deliberate string literal and
    // keeps matching JSON strings only — it must NOT be coerced to a number.
    // This is what previously produced the "silently matches nothing" trap
    // against a JSON-number-typed property; the fix is that the *unquoted*
    // form (above) now works, not that the quoted form starts matching numbers.
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {number: '54'}) RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    let has_text_param = compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Text(s) if s == "54"));
    assert!(
        has_text_param,
        "quoted numeric literal must still bind as TEXT; params: {:?}",
        compiled.params
    );
}

#[test]
fn gql_inline_property_map_accepts_float_and_bool_literals() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {score: 4.5, archived: true}) RETURN n",
    );
    assert!(q.is_ok(), "float/bool literals must parse: {:?}", q.err());
    let compiled = compile(&q.unwrap(), &opts()).unwrap();
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Float(n) if *n == 4.5)));
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Integer(1))));
}

#[test]
fn gql_inline_property_map_entity_type_must_be_string() {
    let err = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {entity_type: 54}) RETURN n",
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::Parse { .. }));
}

#[test]
fn variable_length_inline_property_map_integer_compiles_to_numeric_param() {
    // Same fix, exercised through the variable-length (recursive CTE) compile path.
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {number: 54})-[:extends*1..2]->(b) RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Integer(54))));
}

#[test]
fn variable_length_inline_property_map_large_integer_binds_exact_i64() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:pull_request {number: 9007199254740993})-[:extends*1..2]->(b) RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Integer(9007199254740993))));
}

// --- Issue #832: integer literal precision (2^53+1, i64 bounds, overflow) ---

#[test]
fn gql_inline_property_map_large_integer_binds_exact_i64_not_lossy_float() {
    // 2^53 + 1 = 9007199254740993 is the smallest positive integer that
    // cannot be represented exactly as f64 -- it rounds to 9007199254740992.0.
    // A JSON-number property with this value must be matched by an exact i64
    // parameter, not a lossy float.
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:artifact {number: 9007199254740993}) RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Integer(9007199254740993))),
        "large integer literal must bind as the exact i64, not a rounded f64; params: {:?}",
        compiled.params
    );
}

#[test]
fn gql_inline_property_map_i64_max_binds_exact() {
    let q = parse(
        QueryLanguage::Gql,
        &format!("MATCH (n:artifact {{number: {}}}) RETURN n", i64::MAX),
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Integer(n) if *n == i64::MAX)));
}

#[test]
fn gql_inline_property_map_i64_min_binds_exact() {
    let q = parse(
        QueryLanguage::Gql,
        &format!("MATCH (n:artifact {{number: {}}}) RETURN n", i64::MIN),
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Integer(n) if *n == i64::MIN)));
}

#[test]
fn gql_where_equality_large_integer_binds_exact_i64_not_lossy_float() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:artifact) WHERE n.number = 9007199254740993 RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(
        compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Integer(9007199254740993))),
        "WHERE equality with a large integer literal must bind exact i64; params: {:?}",
        compiled.params
    );
}

#[test]
fn gql_where_equality_i64_bounds_bind_exact() {
    for bound in [i64::MIN, i64::MAX] {
        let q = parse(
            QueryLanguage::Gql,
            &format!("MATCH (n:artifact) WHERE n.number = {bound} RETURN n"),
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled
                .params
                .iter()
                .any(|p| matches!(p, QueryValue::Integer(n) if *n == bound)),
            "bound {bound} must bind exact; params: {:?}",
            compiled.params
        );
    }
}

#[test]
fn variable_length_where_i64_bounds_bind_exact() {
    // Exercises the variable-length WHERE compiler branch (compile_var_len_condition,
    // sql.rs:919), not the start-node inline property map path already covered above.
    for bound in [i64::MIN, i64::MAX] {
        let q = parse(
            QueryLanguage::Gql,
            &format!("MATCH (a)-[:extends*1..3]->(b) WHERE b.number = {bound} RETURN b"),
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled
                .params
                .iter()
                .any(|p| matches!(p, QueryValue::Integer(n) if *n == bound)),
            "variable-length WHERE bound {bound} must bind exact i64; params: {:?}",
            compiled.params
        );
    }
}

#[test]
fn variable_length_end_node_inline_property_map_i64_bounds_bind_exact() {
    // Exercises the end-node inline-map CTE path (end.properties compiled via
    // compile_property_equality), not the start-node inline map already covered above.
    for bound in [i64::MIN, i64::MAX] {
        let q = parse(
            QueryLanguage::Gql,
            &format!("MATCH (a)-[:extends*1..3]->(b:artifact {{number: {bound}}}) RETURN b"),
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled
                .params
                .iter()
                .any(|p| matches!(p, QueryValue::Integer(n) if *n == bound)),
            "end-node inline-map bound {bound} must bind exact i64; params: {:?}",
            compiled.params
        );
    }
}

#[test]
fn gql_inline_property_map_integer_overflow_rejected_at_parse_time() {
    // i64::MAX + 1 -- one digit sequence beyond the supported integer range.
    let overflow = "9223372036854775808";
    let err = parse(
        QueryLanguage::Gql,
        &format!("MATCH (n:artifact {{number: {overflow}}}) RETURN n"),
    )
    .unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "out-of-range integer literal must be rejected at parse time, not silently \
         truncated or coerced to float; got: {err:?}"
    );
}

#[test]
fn gql_where_equality_integer_overflow_rejected_at_parse_time() {
    let overflow = "9223372036854775808";
    let err = parse(
        QueryLanguage::Gql,
        &format!("MATCH (n:artifact) WHERE n.number = {overflow} RETURN n"),
    )
    .unwrap_err();
    assert!(matches!(err, QueryError::Parse { .. }));
}

#[test]
fn gql_inline_property_map_float_overflow_rejected() {
    // A decimal literal (has a '.') whose magnitude overflows f64 to infinity.
    let huge = format!("{}.0", "9".repeat(400));
    let err = parse(
        QueryLanguage::Gql,
        &format!("MATCH (n:document {{score: {huge}}}) RETURN n"),
    )
    .unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "non-finite float literal must be rejected, not silently bound as Infinity; got: {err:?}"
    );
}

// --- Numeric literal grammar (docs/design.md): digits required on both
// sides of the dot. `1.`, `-.5`, and `.5` must all be rejected, not
// delegated to f64::parse's looser rules. ---

#[test]
fn gql_inline_property_map_trailing_dot_float_rejected() {
    let err = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {score: 1.}) RETURN n",
    )
    .unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "'1.' has no digits after the dot and must be rejected; got: {err:?}"
    );
}

#[test]
fn gql_inline_property_map_negative_leading_dot_float_rejected() {
    let err = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {score: -.5}) RETURN n",
    )
    .unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "'-.5' has no digits before the dot and must be rejected; got: {err:?}"
    );
}

#[test]
fn gql_inline_property_map_leading_dot_float_rejected() {
    let err = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {score: .5}) RETURN n",
    )
    .unwrap_err();
    assert!(
        matches!(err, QueryError::Parse { .. }),
        "'.5' has no digits before the dot and must be rejected; got: {err:?}"
    );
}

#[test]
fn gql_inline_property_map_well_formed_float_still_parses() {
    // Digits on both sides of the dot must keep working (regression guard
    // against over-tightening the grammar check).
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n:document {score: 1.5}) RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled
        .params
        .iter()
        .any(|p| matches!(p, QueryValue::Float(n) if *n == 1.5)));
}

// --- GQL extended WHERE operators (#804, #864) ---

#[test]
fn extended_where_operators_compile_with_bound_parameters() {
    let q = parse(
        QueryLanguage::Gql,
        r#"MATCH (n) WHERE n.name CONTAINS "%_\' OR 1=1 --" AND n.name STARTS WITH "pre%_\" AND n.kind IN ["concept", "' OR 1=1 --"] AND n.domain IS NOT NULL RETURN n"#,
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();

    assert!(
        compiled.sql.contains("LIKE ?1 COLLATE NOCASE ESCAPE '\\'"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("LIKE ?2 COLLATE NOCASE ESCAPE '\\'"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("n0.kind COLLATE NOCASE IN (?3, ?4)"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled
            .sql
            .contains("json_extract(n0.properties, '$.domain') IS NOT NULL"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        !compiled.sql.contains("OR 1=1"),
        "injection-shaped data must not appear in SQL: {}",
        compiled.sql
    );
    assert!(matches!(
        compiled.params.as_slice(),
        [
            QueryValue::Text(contains),
            QueryValue::Text(starts),
            QueryValue::Text(first),
            QueryValue::Text(second),
            QueryValue::Integer(_),
        ] if contains == r#"%\%\_\\' OR 1=1 --%"#
            && starts == r#"pre\%\_\\%"#
            && first == "concept"
            && second == "' OR 1=1 --"
    ));
}

#[test]
fn extended_where_operators_compile_for_variable_length_patterns() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (a)-[:extends*1..2]->(b) WHERE a.name CONTAINS 'Lo' \
         AND a.name STARTS WITH 'L' AND b.kind IN ['concept', 'document'] \
         AND b.domain IS NOT NULL RETURN b",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();

    assert!(
        compiled.sql.contains("s.name LIKE ?"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("s.name LIKE ?") && compiled.sql.contains("ESCAPE '\\'"),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled.sql.contains("r.kind COLLATE NOCASE IN ("),
        "sql: {}",
        compiled.sql
    );
    assert!(
        compiled
            .sql
            .contains("json_extract(r.properties, '$.domain') IS NOT NULL"),
        "sql: {}",
        compiled.sql
    );
}

#[test]
fn mixed_type_in_with_string_compiles_case_insensitive() {
    let q = parse(
        QueryLanguage::Gql,
        "MATCH (n) WHERE n.kind IN ['CONCEPT', 1] RETURN n",
    )
    .unwrap();
    let compiled = compile(&q, &opts()).unwrap();

    assert!(
        compiled.sql.contains("n0.kind COLLATE NOCASE IN (?1, ?2)"),
        "sql: {}",
        compiled.sql
    );
    assert!(matches!(
        compiled.params.as_slice(),
        [
            QueryValue::Text(kind),
            QueryValue::Integer(1),
            QueryValue::Integer(_),
        ] if kind == "CONCEPT"
    ));
}

#[test]
fn empty_in_list_compiles_to_false_without_value_parameters() {
    let q = parse(QueryLanguage::Gql, "MATCH (n) WHERE n.name IN [] RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("WHERE") && compiled.sql.contains(" AND 0 LIMIT"));
    assert_eq!(
        compiled.params.len(),
        1,
        "only the LIMIT parameter is expected"
    );
}

// --- Issue #849: substrate node labels (entity/note) must be satisfiable ---
//
// Stored `kind` values are always granular (concept/document/task/...); the
// substrate words entity/note/edge/event name a *table*, not a stored `kind`.
// Compiling a bare substrate label straight into `kind = ?` (as fixed/`event`
// filters did) makes the predicate unsatisfiable by construction. The fix
// filters the union's `substrate_kind` discriminator column instead. These
// tests exercise the compiled SQL shape (fixed-length + variable-length +
// SPARQL) and, for the fixed-length case, execute the compiled SQL against a
// minimal in-memory fixture matching `khive-db`'s substrate schema to prove
// the query is actually satisfiable end-to-end, not just shaped correctly.

mod substrate_labels {
    use super::*;
    use rusqlite::Connection;

    /// Minimal fixture matching `crates/khive-db/sql/schema.sql`'s substrate
    /// tables. All four must exist (even empty) because the compiler always
    /// binds a plain node pattern through the `entities UNION notes UNION
    /// events UNION graph_edges` primary-substrate source.
    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entities (
                id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
                name TEXT NOT NULL, description TEXT, properties TEXT,
                tags TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted_at INTEGER,
                entity_type TEXT, merged_into TEXT, merge_event_id TEXT
            );
            CREATE TABLE notes (
                id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active', name TEXT,
                content TEXT NOT NULL DEFAULT '', salience REAL, decay_factor REAL,
                expires_at INTEGER, properties TEXT, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted_at INTEGER
            );
            CREATE TABLE events (
                id TEXT PRIMARY KEY, namespace TEXT NOT NULL, verb TEXT NOT NULL,
                substrate TEXT NOT NULL, actor TEXT NOT NULL, outcome TEXT NOT NULL,
                data TEXT, duration_us INTEGER NOT NULL DEFAULT 0, target_id TEXT,
                created_at INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT 'audit',
                payload TEXT NOT NULL DEFAULT '{}',
                payload_schema_version INTEGER NOT NULL DEFAULT 1,
                profile_state_version INTEGER, session_id TEXT,
                aggregate_kind TEXT, aggregate_id TEXT
            );
            CREATE TABLE graph_edges (
                namespace TEXT NOT NULL, id TEXT NOT NULL, source_id TEXT NOT NULL,
                target_id TEXT NOT NULL, relation TEXT NOT NULL,
                weight REAL NOT NULL DEFAULT 1.0, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted_at INTEGER, metadata TEXT,
                target_backend TEXT, PRIMARY KEY (namespace, id)
            );
            INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
            VALUES
                ('e-fixture-1', 'local', 'concept', 'X', NULL, '{}', '[]',
                 0, 0, NULL, NULL),
                ('e-fixture-spaces', 'local', 'project', 'Mixed Case Entity',
                 NULL, '{}', '[]', 0, 0, NULL, NULL),
                ('e-fixture-cjk', 'local', 'document', '知识 图谱', NULL, '{}', '[]',
                 0, 0, NULL, NULL);
            INSERT INTO notes
                (id, namespace, kind, status, name, content, salience,
                 decay_factor, expires_at, properties, created_at, updated_at,
                 deleted_at)
            VALUES
                ('n-fixture-1', 'local', 'observation', 'active', 'X', 'body',
                 NULL, NULL, NULL, '{}', 0, 0, NULL);",
        )
        .unwrap();
        conn
    }

    fn run(conn: &Connection, compiled: &CompiledQuery) -> Vec<String> {
        let db_params: Vec<Box<dyn rusqlite::ToSql>> = compiled
            .params
            .iter()
            .map(|p| -> Box<dyn rusqlite::ToSql> {
                match p {
                    QueryValue::Null => Box::new(Option::<i64>::None),
                    QueryValue::Integer(n) => Box::new(*n),
                    QueryValue::Float(n) => Box::new(*n),
                    QueryValue::Text(s) => Box::new(s.clone()),
                    QueryValue::Blob(b) => Box::new(b.clone()),
                }
            })
            .collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = db_params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&compiled.sql).unwrap();
        stmt.query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn insert_entity(conn: &Connection, id: &str, name: &str, properties: &str) {
        conn.execute(
            "INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
             VALUES (?1, 'local', 'concept', ?2, NULL, ?3, '[]', 0, 0, NULL, NULL)",
            rusqlite::params![id, name, properties],
        )
        .unwrap();
    }

    #[test]
    fn contains_escapes_wildcards_backslash_and_quote_end_to_end() {
        let conn = fixture_db();
        insert_entity(&conn, "e-literal", r#"prefix%_\' OR 1=1 --suffix"#, "{}");
        insert_entity(
            &conn,
            "e-wildcard-distractor",
            r#"prefixAB\' OR 1=1 --suffix"#,
            "{}",
        );
        let q = parse(
            QueryLanguage::Gql,
            r#"MATCH (e:entity) WHERE e.name CONTAINS "%_\' OR 1=1 --" RETURN e.id"#,
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert!(!compiled.sql.contains("OR 1=1"), "sql: {}", compiled.sql);
        assert_eq!(run(&conn, &compiled), vec!["e-literal"]);
    }

    #[test]
    fn starts_with_escapes_wildcards_and_backslash_end_to_end() {
        let conn = fixture_db();
        insert_entity(&conn, "e-prefix", r#"pre%_\suffix"#, "{}");
        insert_entity(&conn, "e-prefix-distractor", r#"preAB\suffix"#, "{}");
        let q = parse(
            QueryLanguage::Gql,
            r#"MATCH (e:entity) WHERE e.name STARTS WITH "pre%_\" RETURN e.id"#,
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-prefix"]);
    }

    #[test]
    fn in_list_binds_injection_shaped_strings_end_to_end() {
        let conn = fixture_db();
        insert_entity(&conn, "e-percent", "%", "{}");
        insert_entity(&conn, "e-underscore", "_", "{}");
        insert_entity(&conn, "e-quote", "' OR 1=1 --", "{}");
        insert_entity(&conn, "e-other", "not a match", "{}");
        let q = parse(
            QueryLanguage::Gql,
            r#"MATCH (e:entity) WHERE e.name IN ["%", "_", "' OR 1=1 --"] RETURN e.id"#,
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(!compiled.sql.contains("OR 1=1"), "sql: {}", compiled.sql);

        let mut rows = run(&conn, &compiled);
        rows.sort();
        assert_eq!(rows, vec!["e-percent", "e-quote", "e-underscore"]);
    }

    #[test]
    fn mixed_type_in_preserves_case_insensitive_string_matching_end_to_end() {
        let conn = fixture_db();
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) WHERE e.kind IN ['CONCEPT', 1] RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-fixture-1"]);
    }

    #[test]
    fn is_not_null_filters_json_property_presence_end_to_end() {
        let conn = fixture_db();
        insert_entity(
            &conn,
            "e-with-domain",
            "with domain",
            r#"{"domain":"attention"}"#,
        );
        insert_entity(&conn, "e-without-domain", "without domain", "{}");
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) WHERE e.domain IS NOT NULL RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-with-domain"]);
    }

    /// `fixture_db()` plus a second entity and a `graph_edges` row connecting
    /// it to `e-fixture-1`. SPARQL requires at least one variable-to-variable
    /// relation triple to parse (`no edge patterns found` otherwise), so any
    /// SPARQL regression test needs a connected graph, unlike the single-node
    /// GQL substrate-label tests below.
    fn fixture_db_with_edge() -> Connection {
        let conn = fixture_db();
        conn.execute_batch(
            "INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
            VALUES
                ('e-fixture-2', 'local', 'document', 'Y', NULL, '{}', '[]',
                 0, 0, NULL, NULL);
            INSERT INTO graph_edges
                (namespace, id, source_id, target_id, relation, weight,
                 created_at, updated_at, deleted_at, metadata, target_backend)
            VALUES
                ('local', 'edge-fixture-1', 'e-fixture-1', 'e-fixture-2',
                 'extends', 1.0, 0, 0, NULL, NULL, NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn entity_substrate_label_compiles_without_unsatisfiable_kind_filter() {
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) WHERE e.name = 'X' RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("substrate_kind = ?"),
            "substrate label 'entity' must filter substrate_kind, not kind; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("n0.kind = ?"),
            "substrate label 'entity' must not also emit an unsatisfiable kind filter; sql: {}",
            compiled.sql
        );

        let conn = fixture_db();
        let rows = run(&conn, &compiled);
        assert_eq!(
            rows,
            vec!["e-fixture-1".to_string()],
            "MATCH (e:entity) WHERE e.name = 'X' must return the existing entity row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn entity_name_equality_handles_case_spaces_and_cjk() {
        let conn = fixture_db();
        let cases = [
            ("Mixed Case Entity", "e-fixture-spaces"),
            ("mixed case entity", "e-fixture-spaces"),
            ("知识 图谱", "e-fixture-cjk"),
        ];

        for (name, expected_id) in cases {
            let q = parse(
                QueryLanguage::Gql,
                &format!(r#"MATCH (e:entity) WHERE e.name = "{name}" RETURN e.id"#),
            )
            .unwrap();
            let compiled = compile(&q, &opts()).unwrap();
            assert!(
                compiled.sql.contains("COLLATE NOCASE"),
                "GQL string equality must remain case-insensitive; sql: {}",
                compiled.sql
            );
            assert!(
                compiled.sql.contains(".name = ?"),
                "GQL name equality must use a SQL placeholder; sql: {}",
                compiled.sql
            );
            assert!(
                compiled
                    .params
                    .iter()
                    .any(|param| matches!(param, QueryValue::Text(value) if value == name)),
                "name {name:?} must be passed as a bound text parameter; params: {:?}",
                compiled.params
            );
            assert!(
                !compiled.sql.contains(name),
                "name {name:?} must not be interpolated into SQL: {}",
                compiled.sql
            );
            assert_eq!(
                run(&conn, &compiled),
                vec![expected_id.to_string()],
                "name equality must find {name:?}; sql: {}",
                compiled.sql
            );
        }
    }

    #[test]
    fn note_substrate_label_compiles_without_unsatisfiable_kind_filter() {
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (n:note) WHERE n.name = 'X' RETURN n.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("substrate_kind = ?"),
            "substrate label 'note' must filter substrate_kind, not kind; sql: {}",
            compiled.sql
        );

        let conn = fixture_db();
        let rows = run(&conn, &compiled);
        assert_eq!(
            rows,
            vec!["n-fixture-1".to_string()],
            "MATCH (n:note) WHERE n.name = 'X' must return the existing note row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn granular_label_still_filters_kind_column() {
        let q = parse(QueryLanguage::Gql, "MATCH (e:concept) RETURN e.id").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("n0.kind = ?"),
            "granular label 'concept' must still emit kind = ?; sql: {}",
            compiled.sql
        );
        let has_concept_param = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "concept"));
        assert!(has_concept_param, "params: {:?}", compiled.params);

        let conn = fixture_db();
        let rows = run(&conn, &compiled);
        assert_eq!(
            rows,
            vec!["e-fixture-1".to_string()],
            "MATCH (e:concept) must still return the concept-kind entity row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn sparql_entity_substrate_label_compiles_and_returns_row() {
        let q = parse(
            QueryLanguage::Sparql,
            "SELECT ?e WHERE { ?e a :entity . ?e :name \"X\" . ?e :extends ?c . }",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("substrate_kind = ?"),
            "SPARQL 'a :entity' must filter substrate_kind, not kind (frontend parity); sql: {}",
            compiled.sql
        );

        let conn = fixture_db_with_edge();
        let db_params: Vec<Box<dyn rusqlite::ToSql>> = compiled
            .params
            .iter()
            .map(|p| -> Box<dyn rusqlite::ToSql> {
                match p {
                    QueryValue::Null => Box::new(Option::<i64>::None),
                    QueryValue::Integer(n) => Box::new(*n),
                    QueryValue::Float(n) => Box::new(*n),
                    QueryValue::Text(s) => Box::new(s.clone()),
                    QueryValue::Blob(b) => Box::new(b.clone()),
                }
            })
            .collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = db_params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&compiled.sql).unwrap();
        let ids: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>("e_id"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            ids,
            vec!["e-fixture-1".to_string()],
            "SPARQL entity-substrate query must return the existing entity row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn variable_length_entity_substrate_label_filters_substrate_kind() {
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (a:entity)-[:extends*1..2]->(b) RETURN b LIMIT 5",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("s.substrate_kind = ?"),
            "variable-length seed with substrate label 'entity' must filter \
             s.substrate_kind, not s.kind; sql: {}",
            compiled.sql
        );
    }
}
