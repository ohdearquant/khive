// --- Numeric literal grammar (docs/api/parsing.md): digits required on both
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
        r#"MATCH (n) WHERE n.name CONTAINS "%_\' OR 1=1 --" AND n.name STARTS WITH "pre%_\" AND n.kind IN ["concept", "' OR 1=1 --"] AND n.properties.domain IS NOT NULL RETURN n"#,
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
            QueryValue::Integer(0),
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
         AND b.properties.domain IS NOT NULL RETURN b",
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
            QueryValue::Integer(0),
            QueryValue::Integer(_),
        ] if kind == "CONCEPT"
    ));
}

#[test]
fn empty_in_list_compiles_to_false_without_value_parameters() {
    let q = parse(QueryLanguage::Gql, "MATCH (n) WHERE n.name IN [] RETURN n").unwrap();
    let compiled = compile(&q, &opts()).unwrap();
    assert!(compiled.sql.contains("WHERE") && compiled.sql.contains(" AND 0 ORDER BY"));
    assert_eq!(
        compiled.params.len(),
        2,
        "only the OFFSET and LIMIT parameters are expected"
    );
}
