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
        "MATCH (n:artifact) WHERE n.properties.number = 9007199254740993 RETURN n",
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
            &format!("MATCH (n:artifact) WHERE n.properties.number = {bound} RETURN n"),
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
            &format!(
                "MATCH (a)-[:extends*1..3]->(b) \
                 WHERE b.properties.number = {bound} RETURN b"
            ),
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
        &format!("MATCH (n:artifact) WHERE n.properties.number = {overflow} RETURN n"),
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
