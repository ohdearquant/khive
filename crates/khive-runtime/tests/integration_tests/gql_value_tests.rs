// =============================================================================
// GQL query
// =============================================================================

#[tokio::test]
async fn query_via_gql() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    // Set up entities and edges
    let lora = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "LoRA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let qlora = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "QLoRA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok, qlora.id, lora.id, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();

    // Run a GQL traversal query
    let rows = rt
        .query(
            &tok,
            "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a, e, b LIMIT 10",
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    // Verify row contains the expected column names
    let first_row = &rows[0];
    assert!(first_row.get("a_name").is_some() || first_row.get("a_kind").is_some());
}

// =============================================================================
// GQL inline property-map integer literals (issue #755)
//
// Properties are stored as JSON values; `number: 54` in the entity's props
// blob is a JSON number, so `json_extract` returns SQLite's INTEGER/REAL
// storage class for it. An inline `{number: 54}` match must bind a numeric
// parameter so the comparison actually compares equal; a quoted `{number:
// '54'}` is a deliberate string literal and must keep comparing against JSON
// strings only (it must not start matching the JSON number).
// =============================================================================

#[tokio::test]
async fn query_via_gql_inline_property_map_integer_literal_matches_json_number() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let props = serde_json::json!({"number": 54});
    rt.create_entity_with_embedding_report(
        &tok,
        "artifact",
        None,
        "PR #54",
        None,
        Some(props),
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let rows = rt
        .query(&tok, "MATCH (n:artifact {number: 54}) RETURN n")
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        1,
        "unquoted integer literal must match the JSON-number property"
    );
}

#[tokio::test]
async fn query_via_gql_inline_property_map_quoted_number_does_not_match_json_number() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let props = serde_json::json!({"number": 54});
    rt.create_entity_with_embedding_report(
        &tok,
        "artifact",
        None,
        "PR #54",
        None,
        Some(props),
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let rows = rt
        .query(&tok, "MATCH (n:artifact {number: '54'}) RETURN n")
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        0,
        "quoted string literal must not match a JSON-number property; \
         this is the decided behavior, not a residual bug"
    );
}

// =============================================================================
// GQL integer literal precision against real SQLite (issue #832)
//
// f64 cannot represent every i64 exactly past 2^53: 9007199254740993 (2^53+1)
// rounds to 9007199254740992.0 as a float. A JSON-number property storing the
// exact large value must still be matched by an inline-map or WHERE-equality
// integer literal, both of which now bind QueryValue::Integer instead of a
// lossy QueryValue::Float.
// =============================================================================

#[tokio::test]
async fn query_via_gql_inline_property_map_large_integer_matches_exact_json_number() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let props = serde_json::json!({"number": 9007199254740993i64});
    rt.create_entity_with_embedding_report(
        &tok,
        "artifact",
        None,
        "big",
        None,
        Some(props),
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let rows = rt
        .query(
            &tok,
            "MATCH (n:artifact {number: 9007199254740993}) RETURN n",
        )
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        1,
        "2^53+1 integer literal must match the exact JSON-number property, not round to 2^53"
    );
}

#[tokio::test]
async fn query_via_gql_where_equality_large_integer_matches_exact_json_number() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let props = serde_json::json!({"number": 9007199254740993i64});
    rt.create_entity_with_embedding_report(
        &tok,
        "artifact",
        None,
        "big",
        None,
        Some(props),
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let rows = rt
        .query(
            &tok,
            "MATCH (n:artifact) WHERE n.properties.number = 9007199254740993 RETURN n",
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn query_via_gql_inline_property_map_i64_bounds_match_exact_json_number() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    for bound in [i64::MIN, i64::MAX] {
        let props = serde_json::json!({"number": bound});
        rt.create_entity_with_embedding_report(
            &tok,
            "artifact",
            None,
            "bound",
            None,
            Some(props),
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

        let rows = rt
            .query(
                &tok,
                &format!("MATCH (n:artifact {{number: {bound}}}) RETURN n"),
            )
            .await
            .unwrap();

        assert_eq!(
            rows.len(),
            1,
            "i64 bound {bound} must match its exact JSON-number property"
        );
    }
}

#[tokio::test]
async fn query_via_gql_where_equality_i64_bounds_match_exact_json_number() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    for bound in [i64::MIN, i64::MAX] {
        let props = serde_json::json!({"number": bound});
        rt.create_entity_with_embedding_report(
            &tok,
            "artifact",
            None,
            "bound",
            None,
            Some(props),
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

        let rows = rt
            .query(
                &tok,
                &format!("MATCH (n:artifact) WHERE n.properties.number = {bound} RETURN n"),
            )
            .await
            .unwrap();

        assert_eq!(
            rows.len(),
            1,
            "i64 bound {bound} must match its exact JSON-number property via WHERE equality"
        );
    }
}
