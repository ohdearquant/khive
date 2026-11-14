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

async fn gql_metadata_entity(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    name: &str,
) -> Uuid {
    runtime
        .create_entity_with_embedding_report(
            token,
            "concept",
            None,
            name,
            Some("A description is not part of RETURN node."),
            Some(serde_json::json!({"order": -999, "number": "node decoy", "fixture_name": name})),
            vec![],
        )
        .await
        .unwrap()
        .0
        .id
}

fn gql_metadata_ids(rows: &[khive_storage::SqlRow]) -> Vec<String> {
    rows.iter()
        .map(|row| row.text("e_id").expect("text edge UUID").to_owned())
        .collect()
}

async fn gql_metadata_snapshot(runtime: &KhiveRuntime) -> serde_json::Value {
    let mut snapshot = serde_json::Map::new();
    let mut reader = runtime.sql().reader().await.unwrap();
    for table in ["entities", "notes", "graph_edges", "events"] {
        let rows = reader
            .query_all(khive_storage::SqlStatement::new(
                format!("SELECT * FROM {table} ORDER BY id"),
                vec![],
            ))
            .await
            .unwrap();
        assert!(!rows.is_empty(), "populated {table} snapshot");
        // SqlRow serializes tagged SqlValue cells, preserving SQL NULL versus TEXT.
        snapshot.insert(table.into(), serde_json::to_value(rows).unwrap());
    }
    snapshot.into()
}

async fn gql_metadata_observation(runtime: &KhiveRuntime, token: &khive_runtime::NamespaceToken) {
    runtime
        .create_note(
            token,
            "observation",
            None,
            "Metadata query fixture",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn gql_edge_metadata_projects_stored_json_and_preserves_whole_edge_shape() {
    use khive_storage::{SqlStatement, SqlValue};

    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let source = gql_metadata_entity(&runtime, &token, "metadata source").await;
    let mut accepted = Vec::new();
    for (index, metadata) in [
        Some(serde_json::json!({"order": 3, "nested": {"text": "quoted \"value\"", "items": [1, false, null]}})),
        Some(serde_json::json!({})),
        None,
        Some(serde_json::Value::Null),
    ]
    .into_iter()
    .enumerate()
    {
        let target = gql_metadata_entity(&runtime, &token, &format!("metadata target {index}")).await;
        let edge = runtime.link(&token, source, target, EdgeRelation::VariantOf, 1.0, metadata).await.unwrap();
        accepted.push(edge.id.to_string());
    }
    // Preserve the accepted JSON value but change its stored whitespace: parsing
    // and reserializing metadata must not pass a raw-text projection assertion.
    let formatted = "{\n  \"order\" : 3, \"nested\" : {\"text\":\"quoted \\\"value\\\"\", \"items\":[1,false,null]}\n}";
    let original = runtime
        .get_edge(&token, Uuid::parse_str(&accepted[0]).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(formatted).unwrap(),
        original.metadata.unwrap()
    );
    let changed = runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "UPDATE graph_edges SET metadata = ?1 WHERE id = ?2",
            vec![
                SqlValue::Text(formatted.into()),
                SqlValue::Text(accepted[0].clone()),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(changed, 1);
    let before = gql_metadata_snapshot(&runtime).await;
    let stored = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement::new(
            "SELECT id, metadata FROM graph_edges WHERE source_id = ?1 ORDER BY id",
            vec![SqlValue::Text(source.to_string())],
        ))
        .await
        .unwrap();
    let rows = runtime.query(&token, &format!(
        "MATCH (a:concept)-[e:variant_of]->(b:concept) WHERE a.id = '{source}' RETURN e.id, e.metadata"
    )).await.unwrap();
    assert_eq!(rows.len(), 4);
    for (actual, expected) in rows.iter().zip(&stored) {
        assert_eq!(actual.text("e_id").unwrap(), expected.text("id").unwrap());
        match (actual.get("e_metadata"), expected.get("metadata")) {
            (Some(SqlValue::Text(actual)), Some(SqlValue::Text(expected))) => {
                assert_eq!(actual, expected)
            }
            (Some(SqlValue::Null), Some(SqlValue::Null)) => {}
            pair => {
                panic!("metadata must retain its stored SQL text/null representation: {pair:?}")
            }
        }
    }
    let by_id = |id: &str| {
        rows.iter()
            .find(|row| row.text("e_id").unwrap() == id)
            .unwrap()
    };
    assert_eq!(by_id(&accepted[0]).text("e_metadata").unwrap(), formatted);
    assert!(matches!(
        by_id(&accepted[2]).get("e_metadata"),
        Some(SqlValue::Null)
    ));
    assert_eq!(by_id(&accepted[3]).text("e_metadata").unwrap(), "null");
    let whole = runtime
        .query(
            &token,
            &format!(
                "MATCH (a:concept)-[e:variant_of]->(b:concept) WHERE a.id = '{source}' RETURN e"
            ),
        )
        .await
        .unwrap();
    assert_eq!(gql_metadata_ids(&whole), gql_metadata_ids(&rows));
    for row in &whole {
        assert_eq!(
            row.columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["e_id", "e_source", "e_target", "e_relation", "e_weight"]
        );
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_edge_metadata_predicates_preserve_numbers_booleans_nulls_and_literal_text() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let source = gql_metadata_entity(&runtime, &token, "predicate source").await;
    let mut ids = Vec::new();
    for (index, metadata) in [
        serde_json::json!({"number": 9007199254740993i64, "enabled": true, "nested": {"name": "Alpha_%tail", "path": "root\\leaf"}, "namespace": "ordinary-data"}),
        serde_json::json!({"number": 9007199254740992i64, "enabled": false, "nested": {"name": "AlphaXYtail", "path": "rootXleaf"}}),
        serde_json::json!({"number": "9007199254740993", "enabled": 1, "nested": {"name": "beta"}}),
        serde_json::json!({}),
        serde_json::json!({"number": null}),
    ].into_iter().enumerate() {
        let target = gql_metadata_entity(&runtime, &token, &format!("predicate target {index}")).await;
        ids.push(runtime.link(&token, source, target, EdgeRelation::VariantOf, 1.0, Some(metadata)).await.unwrap().id.to_string());
    }
    for (predicate, expected_indices) in [
        ("e.metadata.number = 9007199254740993", vec![0]),
        ("e.metadata.number = '9007199254740993'", vec![2]),
        (
            "e.metadata.number > 9007199254740992 AND e.metadata.number < 9007199254740994",
            vec![0],
        ),
        ("e.metadata.enabled = true", vec![0, 2]),
        ("e.metadata.enabled = false", vec![1]),
        ("e.metadata.number IS NULL", vec![3, 4]),
        ("e.metadata.number IS NOT NULL", vec![0, 1, 2]),
        ("e.metadata.nested.name = 'alpha_%TAIL'", vec![0]),
        ("e.metadata.nested.name CONTAINS '_%'", vec![0]),
        ("e.metadata.nested.name STARTS WITH 'Alpha_'", vec![0]),
        (r"e.metadata.nested.path CONTAINS '\'", vec![0]),
        (r"e.metadata.nested.path STARTS WITH 'root\'", vec![0]),
        ("e.metadata.namespace = 'ordinary-data'", vec![0]),
    ] {
        let rows = runtime.query(&token, &format!(
            "MATCH (a:concept)-[e:variant_of]->(b:concept) WHERE a.id = '{source}' AND {predicate} RETURN e.id"
        )).await.unwrap_or_else(|error| panic!("{predicate}: {error}"));
        let expected = expected_indices
            .into_iter()
            .map(|index| ids[index].clone())
            .collect::<BTreeSet<_>>();
        assert!(!expected.is_empty());
        assert_eq!(
            gql_metadata_ids(&rows).into_iter().collect::<BTreeSet<_>>(),
            expected,
            "{predicate}"
        );
    }
}

#[tokio::test]
async fn gql_recursive_edge_metadata_uses_the_final_edge() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let root = gql_metadata_entity(&runtime, &token, "recursive root").await;
    let middle = gql_metadata_entity(&runtime, &token, "recursive middle").await;
    let end = gql_metadata_entity(&runtime, &token, "recursive end").await;
    let short = gql_metadata_entity(&runtime, &token, "recursive short").await;
    let seed = runtime
        .link(
            &token,
            root,
            middle,
            EdgeRelation::VariantOf,
            1.0,
            Some(serde_json::json!({"order": 90, "phase": "seed"})),
        )
        .await
        .unwrap();
    let last = runtime
        .link(
            &token,
            middle,
            end,
            EdgeRelation::VariantOf,
            1.0,
            Some(serde_json::json!({"order": 2, "phase": "final"})),
        )
        .await
        .unwrap();
    let direct = runtime
        .link(
            &token,
            root,
            short,
            EdgeRelation::VariantOf,
            1.0,
            Some(serde_json::json!({"order": 1, "phase": "direct"})),
        )
        .await
        .unwrap();
    let pattern = "MATCH (a:concept)-[e:variant_of*1..2]->(b:concept)";
    let rows = runtime
        .query(
            &token,
            &format!(
                "{pattern} WHERE a.id = '{root}' RETURN e.id, e.metadata ORDER BY e.metadata.order"
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        gql_metadata_ids(&rows),
        [
            direct.id.to_string(),
            last.id.to_string(),
            seed.id.to_string()
        ]
    );
    assert_eq!(
        rows[1].text("e_metadata").unwrap(),
        serde_json::to_string(last.metadata.as_ref().unwrap()).unwrap()
    );
    assert!(matches!(
        rows[1].get("_depth"),
        Some(khive_storage::SqlValue::Integer(2))
    ));
    let filtered = runtime.query(&token, &format!("{pattern} WHERE a.id = '{root}' AND e.metadata.phase = 'final' RETURN e.id, e.metadata ORDER BY e.metadata.order")).await.unwrap();
    assert_eq!(gql_metadata_ids(&filtered), [last.id.to_string()]);
    // End-node and final-edge predicates belong to the same traversal phase.
    let same_phase = runtime.query(&token, &format!("MATCH (a:concept {{fixture_name: 'recursive root'}})-[e:variant_of*1..2]->(b:concept) WHERE b.id = '{short}' OR e.metadata.phase = 'final' RETURN e.id, e.metadata ORDER BY e.metadata.order")).await.unwrap();
    assert_eq!(
        gql_metadata_ids(&same_phase),
        [direct.id.to_string(), last.id.to_string()]
    );
    for query in [
        format!("{pattern} WHERE a.id = '{root}' RETURN e ORDER BY e.metadata.order"),
        format!("{pattern} WHERE a.id = '{root}' RETURN a ORDER BY a.description"),
    ] {
        let error = runtime.query(&token, &query).await.unwrap_err();
        assert!(
            matches!(error.refusal_source(), khive_runtime::RuntimeError::Query(khive_query::QueryError::Unsupported(message)) if message.contains("variable-length ORDER BY must be derived from RETURN projections")),
            "{error}"
        );
    }
    let error = runtime.query(&token, &format!("{pattern} WHERE a.id = '{root}' OR e.metadata.phase = 'final' RETURN e.id, e.metadata")).await.unwrap_err();
    assert!(
        matches!(
            error.refusal_source(),
            khive_runtime::RuntimeError::Query(khive_query::QueryError::Unsupported(_))
        ),
        "{error}"
    );
}
