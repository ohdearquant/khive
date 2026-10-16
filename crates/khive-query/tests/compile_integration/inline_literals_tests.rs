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
