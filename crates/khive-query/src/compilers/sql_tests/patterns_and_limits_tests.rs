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
