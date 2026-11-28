#[test]
fn variable_length_or_across_endpoints_rejected() {
    let q =
        gql::parse("MATCH (a)-[:extends*1..3]->(b) WHERE a.name = 'X' OR b.name = 'Y' RETURN a")
            .unwrap();
    let result = compile(&q, &opts());
    assert!(
        matches!(result, Err(QueryError::Unsupported(_))),
        "OR spanning both endpoints must return Unsupported; got {result:?}"
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

#[test]
fn zero_hop_narrow_ranges_use_recursive_lowering() {
    for max in [0, 1, 10] {
        let q = gql::parse(&format!(
            "MATCH (a)-[e:extends*0..{max}]->(b) RETURN a.id, b.id, e.metadata"
        ))
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.starts_with("WITH RECURSIVE"),
            "0..{max}: {}",
            compiled.sql
        );
    }
}

#[test]
fn zero_hop_keeps_synthetic_comma_and_cross_phase_or_refusals() {
    for source in [
        "MATCH (ev)-[:observed_as_selected*0..0]->(m) RETURN m",
        "MATCH (ev)-[:observed_as_selected*0..1]->(m) RETURN m",
        "MATCH (a)-[:extends*0..0]->(b), (a)-[:variant_of]->(c) RETURN b",
        "MATCH (a)-[:extends*0..1]->(b), (a)-[:variant_of]->(c) RETURN b",
        "MATCH (a)-[:extends*0..1]->(b) WHERE a.name = 'X' OR b.name = 'Y' RETURN a",
        "MATCH (a)-[:extends*0..1]->(a) RETURN a",
    ] {
        let q = gql::parse(source).unwrap();
        let error = compile(&q, &opts()).unwrap_err();
        assert!(
            matches!(error, QueryError::Unsupported(_)),
            "{source}: {error:?}"
        );
    }
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
        distinct: false,
        additional_patterns: Vec::new(),
        order_by: Vec::new(),
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
fn grouped_and_distinct_order_requires_returned_values() {
    for pattern in [
        "MATCH (a)-[e:extends]->(b)",
        "MATCH (a)-[e:extends*0..2]->(b)",
        "MATCH (a)-[e:extends*1..2]->(b)",
    ] {
        for tail in [
            "DISTINCT b.name ORDER BY a.name",
            "count(b) ORDER BY b.name",
            "b.name, count(e) ORDER BY b.id",
            "DISTINCT b.properties.rank ORDER BY b.properties.hidden",
        ] {
            let source = format!("{pattern} RETURN {tail}");
            let error = compile(&gql::parse(&source).unwrap(), &opts()).unwrap_err();
            assert!(
                matches!(
                    error,
                    QueryError::Unsupported(_) | QueryError::InvalidInput(_)
                ),
                "{source}: {error}"
            );
        }
        for tail in [
            "DISTINCT b.name ORDER BY b.name",
            "b.properties.rank, count(e) ORDER BY b.properties.rank",
            "b.properties, count(e) ORDER BY b.properties.rank",
            "b, count(e) ORDER BY b.name",
        ] {
            let source = format!("{pattern} RETURN {tail}");
            compile(&gql::parse(&source).unwrap(), &opts())
                .unwrap_or_else(|error| panic!("{source}: {error}"));
        }
    }
    for source in [
        "MATCH (n) RETURN count(missing)",
        "MATCH (count_n)-[:extends]->(n_id) RETURN count(n_id), count_n.id",
    ] {
        assert!(
            compile(&gql::parse(source).unwrap(), &opts()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn not_exists_scope_refusals_are_explicit_and_block_local() {
    for source in [
        "MATCH (r) WHERE NOT EXISTS { (f)-[:supports]->(x) } RETURN r.id",
        "MATCH (r) WHERE NOT EXISTS { (f) } RETURN r.id",
        "MATCH (r) WHERE NOT EXISTS { (r)-[:supports]->(r) } RETURN r.id",
        "MATCH (r)-[e:extends]->(s) WHERE NOT EXISTS { (e)-[:supports]->(r) } RETURN r.id",
        "MATCH (r) WHERE NOT EXISTS { (f)-[r:supports]->(r) } RETURN r.id",
        "MATCH (r) WHERE NOT EXISTS { (f)-[x:supports]->(r)-[x:extends]->(s) } RETURN r.id",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:supports]->(r) } RETURN f.id",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:supports]->(r) } RETURN count(f)",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:supports]->(r) } AND f.name = 'leaked' RETURN r.id",
        "MATCH (r) WHERE NOT EXISTS { (f)-[:supports]->(r) } RETURN r.id ORDER BY f.name",
    ] {
        let query = gql::parse(source).unwrap();
        assert!(compile(&query, &opts()).is_err(), "must refuse: {source}");
    }
    let query = gql::parse(
        "MATCH (r)-[e:extends]->(s) WHERE NOT EXISTS { (f)-[e:supports]->(r) } RETURN r.id",
    )
    .unwrap();
    let error = compile(&query, &opts()).unwrap_err().to_string();
    assert!(error.contains("'e'"), "{error}");
    assert!(
        error.contains("edge variables are not correlated into NOT EXISTS"),
        "{error}"
    );
    let query = gql::parse("MATCH (r) WHERE NOT EXISTS { (f)-[e:supports]->(r) } AND NOT EXISTS { (f)-[e:extends]->(r) } RETURN r.id").unwrap();
    compile(&query, &opts()).unwrap();
}

#[test]
fn not_exists_recursive_phase_guards_keep_boolean_meaning() {
    for predicate in [
        "NOT EXISTS { (f)-[:supports]->(a) } AND NOT EXISTS { (f)-[:supports]->(b) }",
        "a.name = 'root' OR NOT EXISTS { (f)-[:supports]->(a) }",
        "b.name = 'leaf' OR NOT EXISTS { (f)-[:supports]->(b) }",
        "NOT EXISTS { (a)-[:supports]->(b) }",
    ] {
        let query = gql::parse(&format!(
            "MATCH (a)-[:extends*1..3]->(b) WHERE {predicate} RETURN b.id"
        ))
        .unwrap();
        compile(&query, &opts()).unwrap_or_else(|error| panic!("{predicate}: {error}"));
    }
    for predicate in [
        "a.name = 'root' OR NOT EXISTS { (f)-[:supports]->(b) }",
        "NOT EXISTS { (f)-[:supports]->(a) } OR b.name = 'leaf'",
        "NOT EXISTS { (a)-[:supports]->(b) } OR a.name = 'root'",
    ] {
        let query = gql::parse(&format!(
            "MATCH (a)-[:extends*1..3]->(b) WHERE {predicate} RETURN b.id"
        ))
        .unwrap();
        assert!(
            matches!(compile(&query, &opts()), Err(QueryError::Unsupported(_))),
            "cross-phase OR: {predicate}"
        );
    }
}
