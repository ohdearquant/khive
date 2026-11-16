fn gql_sql_value_equal(left: &khive_storage::SqlValue, right: &khive_storage::SqlValue) -> bool {
    use khive_storage::SqlValue;
    match (left, right) {
        (SqlValue::Null, SqlValue::Null) => true,
        (SqlValue::Bool(left), SqlValue::Bool(right)) => left == right,
        (SqlValue::Integer(left), SqlValue::Integer(right)) => left == right,
        (SqlValue::Float(left), SqlValue::Float(right)) => left == right,
        (SqlValue::Text(left), SqlValue::Text(right)) => left == right,
        (SqlValue::Blob(left), SqlValue::Blob(right)) => left == right,
        (SqlValue::Json(left), SqlValue::Json(right)) => left == right,
        (SqlValue::Uuid(left), SqlValue::Uuid(right)) => left == right,
        (SqlValue::Timestamp(left), SqlValue::Timestamp(right)) => left == right,
        _ => false,
    }
}

fn gql_sql_optional_value_equal(
    left: Option<&khive_storage::SqlValue>,
    right: Option<&khive_storage::SqlValue>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => gql_sql_value_equal(left, right),
        _ => false,
    }
}

fn gql_sql_rows_equal<const N: usize>(
    left: &[[khive_storage::SqlValue; N]],
    right: &[[khive_storage::SqlValue; N]],
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.iter()
                .zip(right)
                .all(|(left, right)| gql_sql_value_equal(left, right))
        })
}

macro_rules! assert_gql_sql_value_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left, right) => assert!(
                gql_sql_optional_value_equal(*left, *right),
                "assertion `left == right` failed\n  left: {left:?}\n right: {right:?}"
            ),
        }
    };
    ($left:expr, $right:expr, $($message:tt)+) => {
        match (&$left, &$right) {
            (left, right) => assert!(
                gql_sql_optional_value_equal(*left, *right),
                "{}\n  left: {left:?}\n right: {right:?}",
                format_args!($($message)+)
            ),
        }
    };
}

macro_rules! assert_gql_sql_rows_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left, right) => assert!(
                gql_sql_rows_equal(left, right),
                "assertion `left == right` failed\n  left: {left:?}\n right: {right:?}"
            ),
        }
    };
    ($left:expr, $right:expr, $($message:tt)+) => {
        match (&$left, &$right) {
            (left, right) => assert!(
                gql_sql_rows_equal(left, right),
                "{}\n  left: {left:?}\n right: {right:?}",
                format_args!($($message)+)
            ),
        }
    };
}

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

#[tokio::test]
async fn gql_zero_hop_isolated_roots_and_null_final_edges() {
    use khive_storage::SqlValue;

    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let root = gql_metadata_entity(&runtime, &token, "isolated zero root").await;
    for max in [0, 1] {
        for projection in [
            "a.id, b.id, e.id, e.relation, e.weight, e.metadata",
            "a.id, b.id, e, e.metadata",
        ] {
            let rows = runtime.query(&token, &format!(
                "MATCH (a:concept)-[e:variant_of*0..{max}]->(b:concept) WHERE a.id = '{root}' RETURN {projection}"
            )).await.unwrap();
            assert_eq!(rows.len(), 1, "isolated 0..{max}: {projection}");
            let row = &rows[0];
            assert_eq!(row.text("a_id").unwrap(), root.to_string());
            assert_eq!(row.text("b_id").unwrap(), root.to_string());
            assert_eq!(
                row.columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>(),
                [
                    "a_id",
                    "b_id",
                    "e_id",
                    "e_relation",
                    "e_weight",
                    "e_metadata",
                    "_depth",
                    "_total_weight"
                ]
            );
            assert!(matches!(row.get("_depth"), Some(SqlValue::Integer(0))));
            assert!(
                matches!(row.get("_total_weight"), Some(SqlValue::Float(weight)) if *weight == 0.0)
            );
            for column in ["e_id", "e_relation", "e_weight", "e_metadata"] {
                assert!(
                    matches!(row.get(column), Some(SqlValue::Null)),
                    "{column}: {:?}",
                    row.get(column)
                );
            }
        }
    }
    for range in ["1..1", "1..2"] {
        let rows = runtime
            .query(
                &token,
                &format!(
            "MATCH (a:concept)-[:variant_of*{range}]->(b:concept) WHERE a.id = '{root}' RETURN b.id"
        ),
            )
            .await
            .unwrap();
        assert!(rows.is_empty(), "isolated {range}");
    }
}

#[tokio::test]
async fn gql_zero_hop_applies_both_endpoint_constraints() {
    use khive_storage::SqlValue;

    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let root = gql_metadata_entity(&runtime, &token, "zero root").await;
    let child = gql_metadata_entity(&runtime, &token, "zero child").await;
    let incident = runtime
        .link(
            &token,
            root,
            child,
            EdgeRelation::VariantOf,
            0.25,
            Some(serde_json::json!({"order": 7})),
        )
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    let zero_only = runtime
        .query(
            &token,
            &format!(
        "MATCH (a:concept)-[:variant_of*0..0]->(b:concept) WHERE a.id = '{root}' RETURN b.id"
    ),
        )
        .await
        .unwrap();
    assert_eq!(
        zero_only.len(),
        1,
        "max zero must not expand the incident edge"
    );
    assert_eq!(zero_only[0].text("b_id").unwrap(), root.to_string());
    for (start, end, predicate, count) in [
        (
            "concept {fixture_name: 'zero root'}",
            "concept {fixture_name: 'zero root'}",
            "a.entity_type IS NULL AND b.entity_type IS NULL",
            1,
        ),
        ("document", "concept", "b.name = 'zero root'", 0),
        ("concept", "document", "b.name = 'zero root'", 0),
        (
            "concept {fixture_name: 'missing'}",
            "concept",
            "b.name = 'zero root'",
            0,
        ),
        (
            "concept",
            "concept {fixture_name: 'missing'}",
            "b.name = 'zero root'",
            0,
        ),
        ("concept", "concept", "a.name = 'missing'", 0),
        ("concept", "concept", "b.name = 'missing'", 0),
        ("concept", "concept", "a.entity_type IS NOT NULL", 0),
        ("concept", "concept", "b.entity_type IS NOT NULL", 0),
    ] {
        let query = format!("MATCH (a:{start})-[e:variant_of*0..0]->(b:{end}) WHERE a.id = '{root}' AND {predicate} RETURN a.id");
        let rows = runtime.query(&token, &query).await.unwrap();
        assert_eq!(
            rows.len(),
            count,
            "end constraints apply without an end projection: {query}"
        );
        if count == 1 {
            assert_eq!(rows[0].text("a_id").unwrap(), root.to_string());
        }
    }
    for (predicate, expected_id) in [
        ("e.metadata.order IS NULL", root),
        ("e.metadata.order IS NOT NULL", child),
    ] {
        let rows = runtime.query(&token, &format!(
            "MATCH (a:concept)-[e:variant_of*0..1]->(b:concept) WHERE a.id = '{root}' AND {predicate} RETURN b.id, e.id, e.relation, e.weight, e.metadata"
        )).await.unwrap();
        assert_eq!(rows.len(), 1, "{predicate}");
        assert_eq!(rows[0].text("b_id").unwrap(), expected_id.to_string());
        if expected_id == root {
            for column in ["e_id", "e_relation", "e_weight", "e_metadata"] {
                assert!(
                    matches!(rows[0].get(column), Some(SqlValue::Null)),
                    "zero hop must not borrow the incident edge: {column}"
                );
            }
        } else {
            assert_eq!(rows[0].text("e_id").unwrap(), incident.id.to_string());
            assert_eq!(rows[0].text("e_relation").unwrap(), "variant_of");
            assert!(
                matches!(rows[0].get("e_weight"), Some(SqlValue::Float(weight)) if *weight == 0.25)
            );
        }
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_zero_hop_weighted_directions_and_cycle_limits() {
    use khive_storage::SqlValue;

    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let root = gql_metadata_entity(&runtime, &token, "weighted root").await;
    let child = gql_metadata_entity(&runtime, &token, "weighted child").await;
    let grandchild = gql_metadata_entity(&runtime, &token, "weighted grandchild").await;
    for (from, to, weight) in [(root, child, 0.25), (child, grandchild, 0.5)] {
        runtime
            .link(&token, from, to, EdgeRelation::VariantOf, weight, None)
            .await
            .unwrap();
    }
    let values = |rows: &[khive_storage::SqlRow]| {
        rows.iter()
            .map(|row| {
                let depth = match row.get("_depth") {
                    Some(SqlValue::Integer(depth)) => *depth,
                    other => panic!("integer depth required: {other:?}"),
                };
                let weight = match row.get("_total_weight") {
                    Some(SqlValue::Float(weight)) => *weight,
                    other => panic!("floating accumulated weight required: {other:?}"),
                };
                (row.text("b_id").unwrap().to_owned(), depth, weight)
            })
            .collect::<Vec<_>>()
    };
    for (direction, start, expected) in [
        (
            "out",
            root,
            vec![
                (root.to_string(), 0, 0.0),
                (child.to_string(), 1, 0.25),
                (grandchild.to_string(), 2, 0.75),
            ],
        ),
        (
            "in",
            grandchild,
            vec![
                (grandchild.to_string(), 0, 0.0),
                (child.to_string(), 1, 0.5),
                (root.to_string(), 2, 0.75),
            ],
        ),
        (
            "both",
            child,
            vec![
                (child.to_string(), 0, 0.0),
                (grandchild.to_string(), 1, 0.5),
                (root.to_string(), 1, 0.25),
            ],
        ),
    ] {
        for min in [0, 1] {
            let segment = match direction {
                "out" => format!("-[e:variant_of*{min}..2]->"),
                "in" => format!("<-[e:variant_of*{min}..2]-"),
                _ => format!("-[e:variant_of*{min}..2]-"),
            };
            let rows = runtime
                .query(
                    &token,
                    &format!(
                        "MATCH (a:concept){segment}(b:concept) WHERE a.id = '{start}' RETURN b.id"
                    ),
                )
                .await
                .unwrap();
            assert_eq!(values(&rows), expected[min..], "{direction}, min={min}");
        }
    }
    runtime
        .link(
            &token,
            grandchild,
            root,
            EdgeRelation::VariantOf,
            0.75,
            None,
        )
        .await
        .unwrap();
    for min in [0, 1] {
        let rows = runtime.query(&token, &format!(
            "MATCH (a:concept)-[:variant_of*{min}..4]->(b:concept) WHERE a.id = '{root}' RETURN b.id"
        )).await.unwrap();
        let expected = [
            (root.to_string(), 0, 0.0),
            (child.to_string(), 1, 0.25),
            (grandchild.to_string(), 2, 0.75),
        ];
        assert_eq!(
            values(&rows),
            expected[min..],
            "a later hop must not revisit the root"
        );
    }
}

struct GqlCommaStar {
    root: Uuid,
    left: Vec<khive_storage::Edge>,
    right: Vec<khive_storage::Edge>,
}

impl GqlCommaStar {
    fn pattern(&self) -> String {
        format!("MATCH (a:concept {{fixture_name: 'comma root'}})-[e:extends]->(b:concept), (a)-[f:variant_of]->(c:concept) WHERE a.id = '{}'", self.root)
    }

    fn pairs(&self) -> Vec<(String, String)> {
        self.left
            .iter()
            .flat_map(|left| {
                self.right
                    .iter()
                    .map(move |right| (left.id.to_string(), right.id.to_string()))
            })
            .collect()
    }
}

async fn gql_comma_branch(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    root: Uuid,
    relation: EdgeRelation,
    count: usize,
    label: &str,
) -> Vec<khive_storage::Edge> {
    let mut edges = Vec::new();
    for index in 0..count {
        let target = gql_metadata_entity(runtime, token, &format!("comma {label} {index}")).await;
        edges.push(
            runtime
                .link(token, root, target, relation, 1.0, None)
                .await
                .unwrap(),
        );
    }
    edges.sort_by_key(|edge| edge.id.to_string());
    for (index, edge) in edges.iter_mut().enumerate() {
        let order = if index == 0 { count } else { 1 };
        let updated = runtime
            .link(
                token,
                root,
                edge.target_id,
                relation,
                1.0,
                Some(serde_json::json!({"order": order, "enabled": true, "group": label})),
            )
            .await
            .unwrap();
        assert_eq!(updated.id, edge.id);
        *edge = updated;
    }
    edges
}

async fn gql_comma_star(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
) -> GqlCommaStar {
    gql_metadata_observation(runtime, token).await;
    let root = gql_metadata_entity(runtime, token, "comma root").await;
    let left = gql_comma_branch(runtime, token, root, EdgeRelation::Extends, 2, "left").await;
    let right = gql_comma_branch(runtime, token, root, EdgeRelation::VariantOf, 3, "right").await;
    let other = gql_metadata_entity(runtime, token, "other center").await;
    gql_comma_branch(
        runtime,
        token,
        other,
        EdgeRelation::Extends,
        1,
        "other-left",
    )
    .await;
    gql_comma_branch(
        runtime,
        token,
        other,
        EdgeRelation::VariantOf,
        1,
        "other-right",
    )
    .await;
    GqlCommaStar { root, left, right }
}

fn gql_comma_pairs(rows: &[khive_storage::SqlRow]) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| {
            (
                row.text("e_id").unwrap().to_owned(),
                row.text("f_id").unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn gql_comma_star_preserves_shared_identity_and_global_predicates() {
    use khive_storage::{SqlStatement, SqlValue};
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let fixture = gql_comma_star(&runtime, &token).await;
    let raw = "{\n  \"order\": 2, \"enabled\": true, \"group\": \"left\"\n}";
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(raw).unwrap(),
        fixture.left[0].metadata.clone().unwrap()
    );
    assert_eq!(
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement::new(
                "UPDATE graph_edges SET metadata = ?1 WHERE id = ?2",
                vec![
                    SqlValue::Text(raw.into()),
                    SqlValue::Text(fixture.left[0].id.to_string())
                ]
            ))
            .await
            .unwrap(),
        1
    );
    let before = gql_metadata_snapshot(&runtime).await;
    let rows = runtime
        .query(
            &token,
            &format!(
                "{} RETURN a.id, b.id, c.id, e.id, f.id, e.metadata, f.metadata",
                fixture.pattern()
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        gql_comma_pairs(&rows),
        fixture.pairs(),
        "exact six same-center tuples"
    );
    for row in &rows {
        assert_eq!(row.text("a_id").unwrap(), fixture.root.to_string());
        let left = fixture
            .left
            .iter()
            .find(|edge| edge.id.to_string() == row.text("e_id").unwrap())
            .unwrap();
        let right = fixture
            .right
            .iter()
            .find(|edge| edge.id.to_string() == row.text("f_id").unwrap())
            .unwrap();
        assert_eq!(row.text("b_id").unwrap(), left.target_id.to_string());
        assert_eq!(row.text("c_id").unwrap(), right.target_id.to_string());
        let left_raw = if left.id == fixture.left[0].id {
            raw.to_owned()
        } else {
            serde_json::to_string(left.metadata.as_ref().unwrap()).unwrap()
        };
        assert_eq!(row.text("e_metadata").unwrap(), left_raw);
        assert_eq!(
            row.text("f_metadata").unwrap(),
            serde_json::to_string(right.metadata.as_ref().unwrap()).unwrap()
        );
    }
    let narrowed = runtime
        .query(
            &token,
            &format!(
                "{} AND e.metadata.order = 1 AND f.metadata.order = 1 RETURN e.id, f.id",
                fixture.pattern()
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        gql_comma_pairs(&narrowed),
        vec![
            (
                fixture.left[1].id.to_string(),
                fixture.right[1].id.to_string()
            ),
            (
                fixture.left[1].id.to_string(),
                fixture.right[2].id.to_string()
            )
        ]
    );
    // Keep OR global: neither branch alone may be discarded into its path's filters.
    let global_or = runtime.query(&token, &format!("MATCH (a:concept {{fixture_name: 'comma root'}})-[e:extends]->(b), (a)-[f:variant_of]->(c) WHERE b.id = '{}' OR f.metadata.order = 3 RETURN e.id, f.id", fixture.left[0].target_id)).await.unwrap();
    let expected = fixture
        .pairs()
        .into_iter()
        .filter(|(e, f)| {
            e == &fixture.left[0].id.to_string() || f == &fixture.right[0].id.to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(gql_comma_pairs(&global_or), expected);
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_comma_shared_nodes_keep_every_occurrence_constraint() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let fixture = gql_comma_star(&runtime, &token).await;
    let document = runtime
        .create_entity_with_embedding_report(
            &token,
            "document",
            Some("paper"),
            "typed document",
            None,
            None,
            vec![],
        )
        .await
        .unwrap()
        .0;
    let before = gql_metadata_snapshot(&runtime).await;
    for (constraint, count) in [
        ("a:entity {order: -999}", 6),
        ("a:concept {fixture_name: 'comma root'}", 6),
        ("a:entity {fixture_name: 'not the root'}", 0),
        ("a:note", 0),
    ] {
        let query = format!("MATCH (a:concept {{fixture_name: 'comma root'}})-[e:extends]->(b), ({constraint})-[f:variant_of]->(c) RETURN e.id, f.id");
        let rows = runtime.query(&token, &query).await.unwrap();
        assert_eq!(rows.len(), count, "{constraint}");
        if count > 0 {
            assert_eq!(gql_comma_pairs(&rows), fixture.pairs());
        }
    }
    // Singleton paths add conjunctive constraints; subtype filters cannot overwrite each other.
    for (subtype, count) in [("paper", 1), ("summary", 0)] {
        let query = format!("MATCH (a:document {{entity_type: 'paper'}}), (a:entity {{entity_type: '{subtype}'}}) WHERE a.id = '{}' RETURN a.id", document.id);
        let rows = runtime.query(&token, &query).await.unwrap();
        assert_eq!(rows.len(), count, "{subtype}");
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_comma_shared_identity_includes_the_substrate() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let fixture = gql_comma_star(&runtime, &token).await;
    // Trusted store setup intentionally overlaps IDs; ordinary create is not claimed to do so.
    let mut note =
        khive_storage::Note::new("local", "observation", "same UUID, different substrate");
    note.id = fixture.root;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(fixture.root)
        .await
        .unwrap()
        .is_some());
    assert!(runtime
        .notes(&token)
        .unwrap()
        .get_note(fixture.root)
        .await
        .unwrap()
        .is_some());
    let before = gql_metadata_snapshot(&runtime).await;
    for kind in ["entity", "note"] {
        let rows = runtime
            .query(
                &token,
                &format!(
                    "MATCH (a:{kind}), (a:{kind}) WHERE a.id = '{}' RETURN a.id",
                    fixture.root
                ),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "same-substrate positive: {kind}");
        assert_eq!(rows[0].text("a_id").unwrap(), fixture.root.to_string());
    }
    let rows = runtime
        .query(
            &token,
            &format!(
                "MATCH (a:entity), (a:note) WHERE a.id = '{}' RETURN a.id",
                fixture.root
            ),
        )
        .await
        .unwrap();
    assert!(
        rows.is_empty(),
        "equal UUIDs on different substrates must not unify"
    );
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_comma_connectivity_is_global_and_directions_preserve_bindings() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let a = gql_metadata_entity(&runtime, &token, "bridge-a").await;
    let b = gql_metadata_entity(&runtime, &token, "bridge-b").await;
    let c = gql_metadata_entity(&runtime, &token, "bridge-c").await;
    let d = gql_metadata_entity(&runtime, &token, "bridge-d").await;
    gql_metadata_observation(&runtime, &token).await;
    let ab = runtime
        .link(&token, a, b, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    let bc = runtime
        .link(&token, b, c, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    let cd = runtime
        .link(&token, c, d, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();
    let da = runtime
        .link(&token, d, a, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    for pattern in [
        "(a)-[e:extends]->(b), (c)-[f:variant_of]->(d), (b)-[g:extends]->(c)",
        "(b)<-[e:extends]-(a), (d)<-[f:variant_of]-(c), (c)<-[g:extends]-(b)",
        "(c)-[f:variant_of]->(d), (a)-[e:extends]->(b), (b)-[g:extends]->(c)",
        "(a)-[e:extends]->(b)-[g:extends]->(c), (c)-[f:variant_of]->(d)",
    ] {
        let rows = runtime
            .query(
                &token,
                &format!(
                    "MATCH {pattern} WHERE a.id = '{a}' AND d.id = '{d}' RETURN e.id, f.id, g.id"
                ),
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{pattern}");
        assert_eq!(rows[0].text("e_id").unwrap(), ab.id.to_string());
        assert_eq!(rows[0].text("f_id").unwrap(), cd.id.to_string());
        assert_eq!(rows[0].text("g_id").unwrap(), bc.id.to_string());
    }
    let undirected = runtime
        .query(
            &token,
            &format!(
                "MATCH (a)-[e:extends]-(b), (c)-[f:variant_of]-(d), (b)-[g:extends]-(c) WHERE a.id = '{a}' AND d.id = '{d}' RETURN a.id, b.id, c.id, d.id, e.id, f.id, g.id"
            ),
        )
        .await
        .unwrap();
    let mut undirected_bindings = undirected
        .iter()
        .map(|row| {
            ["a_id", "b_id", "c_id", "d_id", "e_id", "f_id", "g_id"]
                .map(|column| row.text(column).unwrap().to_string())
        })
        .collect::<Vec<_>>();
    undirected_bindings.sort();
    // Different variables may reuse one edge or node when direction permits it.
    let mut expected_undirected_bindings = vec![
        [
            a.to_string(),
            b.to_string(),
            c.to_string(),
            d.to_string(),
            ab.id.to_string(),
            cd.id.to_string(),
            bc.id.to_string(),
        ],
        [
            a.to_string(),
            b.to_string(),
            a.to_string(),
            d.to_string(),
            ab.id.to_string(),
            da.id.to_string(),
            ab.id.to_string(),
        ],
    ];
    expected_undirected_bindings.sort();
    assert_eq!(undirected_bindings, expected_undirected_bindings);
    let cycle = runtime.query(&token, &format!("MATCH (a)-[e:extends]->(b)-[g:extends]->(c), (c)-[f:variant_of]->(d)-[h:variant_of]->(a) WHERE a.id = '{a}' RETURN h.id")).await.unwrap();
    assert_eq!(cycle.len(), 1);
    assert_eq!(cycle[0].text("h_id").unwrap(), da.id.to_string());
    // A and a are distinct, and an anonymous endpoint is a fresh occurrence; b connects both paths.
    for other in ["A", ""] {
        let rows = runtime.query(&token, &format!("MATCH (a)-[e:extends]->(b), (b)-[f:extends]->({other}) WHERE a.id = '{a}' RETURN e.id, f.id")).await.unwrap();
        assert_eq!(
            gql_comma_pairs(&rows),
            vec![(ab.id.to_string(), bc.id.to_string())]
        );
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_comma_later_paths_keep_static_endpoint_warnings() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let fixture = gql_comma_star(&runtime, &token).await;
    let base = format!("MATCH (a:concept)-[e:extends]->(b:concept), (a:concept)-[f:precedes]->(c:concept) WHERE a.id = '{}' RETURN a.id", fixture.root);
    let result = runtime
        .query_with_metadata(&token, &base, khive_query::CompileOptions::default())
        .await
        .unwrap();
    assert!(result.rows.is_empty());
    assert!(
        result
            .warnings
            .iter()
            .any(|warning| warning.contains("precedes")),
        "{:?}",
        result.warnings
    );
    let healthy = runtime
        .query_with_metadata(
            &token,
            &format!("{} RETURN a.id", fixture.pattern()),
            khive_query::CompileOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(healthy.rows.len(), 6);
    assert!(healthy.warnings.is_empty(), "{:?}", healthy.warnings);
}

async fn gql_return_path_entity(
    runtime: &KhiveRuntime,
    token: &khive_runtime::NamespaceToken,
    name: &str,
    properties: serde_json::Value,
) -> Uuid {
    runtime
        .create_entity_with_embedding_report(
            token,
            "concept",
            None,
            name,
            None,
            Some(properties),
            vec![],
        )
        .await
        .unwrap()
        .0
        .id
}

#[tokio::test]
async fn gql_return_paths_preserve_json_scalar_and_container_types() {
    use khive_storage::SqlValue;

    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let values = serde_json::json!({"values": {
        "integer": 7, "large": 9007199254740993_i64, "float": 1.25,
        "text": "quoted \"value\"", "yes": true, "no": false, "null": null,
        "object": {"answer": 42}, "array": [1, false, null]
    }});
    let source =
        gql_return_path_entity(&runtime, &token, "typed path source", values.clone()).await;
    let target = gql_return_path_entity(
        &runtime,
        &token,
        "typed path target",
        serde_json::json!({"values":{"integer":-99}}),
    )
    .await;
    runtime
        .link(
            &token,
            source,
            target,
            EdgeRelation::VariantOf,
            1.0,
            Some(values.clone()),
        )
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    let expected = [
        ("integer", SqlValue::Integer(7)),
        ("large", SqlValue::Integer(9007199254740993_i64)),
        ("float", SqlValue::Float(1.25)),
        ("text", SqlValue::Text("quoted \"value\"".into())),
        ("yes", SqlValue::Integer(1)),
        ("no", SqlValue::Integer(0)),
        ("null", SqlValue::Null),
        ("missing", SqlValue::Null),
        ("object", SqlValue::Text(r#"{"answer":42}"#.into())),
        ("array", SqlValue::Text("[1,false,null]".into())),
    ];
    for (prefix, alias) in [
        ("n.properties", "n_properties"),
        ("e.metadata", "e_metadata"),
    ] {
        let projection = expected
            .iter()
            .map(|(key, _)| format!("{prefix}.values.{key}"))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = runtime.query(&token, &format!("MATCH (n:concept)-[e:variant_of]->(b:concept) WHERE n.id = '{source}' RETURN {projection}")).await.unwrap();
        assert_eq!(rows.len(), 1);
        let labels = expected
            .iter()
            .map(|(key, _)| format!("{alias}_values_{key}"))
            .collect::<Vec<_>>();
        assert_eq!(
            rows[0]
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>(),
            labels
        );
        for ((key, value), label) in expected.iter().zip(&labels) {
            assert_gql_sql_value_eq!(
                rows[0].get(label),
                Some(value),
                "{prefix}.{key} must preserve SQLite type and value"
            );
        }
    }
    let whole = runtime.query(&token, &format!("MATCH (n:concept)-[e:variant_of]->(b:concept) WHERE n.id = '{source}' RETURN n.properties, e.metadata")).await.unwrap();
    assert_eq!(whole.len(), 1);
    for label in ["n_properties", "e_metadata"] {
        assert_eq!(
            whole[0].text(label).unwrap(),
            serde_json::to_string(&values).unwrap()
        );
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_recursive_return_paths_use_final_edge_and_zero_null() {
    use khive_storage::SqlValue;

    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let mut nodes = Vec::new();
    for (marker, decoy) in [
        ("start", -101),
        ("middle", -202),
        ("end", -303),
        ("direct", -404),
        ("isolated", -505),
    ] {
        nodes.push(
            gql_return_path_entity(
                &runtime,
                &token,
                marker,
                serde_json::json!({"marker":marker,"rank":{"order":decoy},"phase":"node-decoy"}),
            )
            .await,
        );
    }
    let [root, middle, end, direct, isolated] = <[Uuid; 5]>::try_from(nodes).unwrap();
    for (from, to, order, phase) in [
        (root, middle, 90, "earlier"),
        (middle, end, 2, "final"),
        (root, direct, 1, "direct"),
    ] {
        runtime
            .link(
                &token,
                from,
                to,
                EdgeRelation::VariantOf,
                1.0,
                Some(serde_json::json!({"rank":{"order":order},"phase":phase})),
            )
            .await
            .unwrap();
    }
    let before = gql_metadata_snapshot(&runtime).await;
    let projection =
        "a.properties.marker, b.properties.marker, e.metadata.rank.order, e.metadata.phase";
    let values = |rows: &[khive_storage::SqlRow]| {
        rows.iter()
            .map(|row| {
                [
                    "a_properties_marker",
                    "b_properties_marker",
                    "e_metadata_rank_order",
                    "e_metadata_phase",
                ]
                .map(|label| row.get(label).unwrap().clone())
            })
            .collect::<Vec<_>>()
    };
    let expected = [
        [
            SqlValue::Text("start".into()),
            SqlValue::Text("start".into()),
            SqlValue::Null,
            SqlValue::Null,
        ],
        [
            SqlValue::Text("start".into()),
            SqlValue::Text("direct".into()),
            SqlValue::Integer(1),
            SqlValue::Text("direct".into()),
        ],
        [
            SqlValue::Text("start".into()),
            SqlValue::Text("end".into()),
            SqlValue::Integer(2),
            SqlValue::Text("final".into()),
        ],
        [
            SqlValue::Text("start".into()),
            SqlValue::Text("middle".into()),
            SqlValue::Integer(90),
            SqlValue::Text("earlier".into()),
        ],
    ];
    let fixed = runtime.query(&token, &format!("MATCH (a:concept)-[e:variant_of]->(b:concept) WHERE a.id = '{root}' RETURN {projection} ORDER BY e.metadata.rank.order")).await.unwrap();
    assert_gql_sql_rows_eq!(
        values(&fixed),
        vec![expected[1].clone(), expected[3].clone()]
    );
    for min in [0, 1] {
        let base = format!("MATCH (a:concept)-[e:variant_of*{min}..2]->(b:concept) WHERE a.id = '{root}' RETURN {projection} ORDER BY e.metadata.rank.order");
        let rows = runtime.query(&token, &base).await.unwrap();
        assert_gql_sql_rows_eq!(values(&rows), expected[min..], "min={min}");
        let depths = [0, 1, 2, 1];
        for (row, depth) in rows.iter().zip(&depths[min..]) {
            assert_gql_sql_value_eq!(row.get("_depth"), Some(&SqlValue::Integer(*depth)));
            assert_gql_sql_value_eq!(
                row.get("_total_weight"),
                Some(&SqlValue::Float(*depth as f64))
            );
        }
        let page = runtime
            .query(&token, &format!("{base} SKIP 1 LIMIT 2"))
            .await
            .unwrap();
        assert_gql_sql_rows_eq!(
            values(&page),
            expected[min + 1..min + 3],
            "post-SKIP LIMIT: min={min}"
        );
    }
    for max in [0, 1] {
        let rows = runtime.query(&token, &format!("MATCH (a:concept)-[e:variant_of*0..{max}]->(b:concept) WHERE a.id = '{isolated}' RETURN {projection}, e.metadata.rank")).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text("a_properties_marker").unwrap(), "isolated");
        assert_eq!(rows[0].text("b_properties_marker").unwrap(), "isolated");
        for label in [
            "e_metadata_rank_order",
            "e_metadata_phase",
            "e_metadata_rank",
        ] {
            assert_gql_sql_value_eq!(
                rows[0].get(label),
                Some(&SqlValue::Null),
                "isolated0..{max} {label}"
            );
        }
        assert_gql_sql_value_eq!(rows[0].get("_depth"), Some(&SqlValue::Integer(0)));
        assert_gql_sql_value_eq!(rows[0].get("_total_weight"), Some(&SqlValue::Float(0.0)));
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_return_path_collisions_refuse_before_execution() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let source = gql_return_path_entity(
        &runtime,
        &token,
        "collision source",
        serde_json::json!({"a_b":1,"a":{"b":2},"name":"property-name"}),
    )
    .await;
    let target = gql_metadata_entity(&runtime, &token, "collision target").await;
    runtime
        .link(&token, source, target, EdgeRelation::VariantOf, 1.0, None)
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    for range in ["", "*0..2", "*1..2"] {
        for projection in [
            "a.properties.a_b, a.properties.a.b",
            "a.properties.name, a_properties.name",
            "a.properties.name, a_properties",
        ] {
            let input = format!("MATCH (a:concept)-[e:variant_of{range}]->(a_properties:concept) WHERE a.id = '{source}' RETURN {projection}");
            let error = runtime.query(&token, &input).await.unwrap_err();
            assert!(
                matches!(error.refusal_source(), khive_runtime::RuntimeError::Query(khive_query::QueryError::Compile(message)) if message.contains("RETURN projection column name collision")),
                "{input}: {error}"
            );
        }
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_distinct_and_typed_groups_use_only_returned_values() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut ids = Vec::new();
    for properties in [
        serde_json::json!({"bucket": "A"}),
        serde_json::json!({"bucket": "a"}),
        serde_json::json!({"bucket": 1}),
        serde_json::json!({"bucket": 1.0}),
        serde_json::json!({"bucket": null}),
        serde_json::json!({}),
    ] {
        ids.push(gql_return_path_entity(&runtime, &token, "grouped", properties).await);
    }
    gql_metadata_observation(&runtime, &token).await;
    runtime
        .link(&token, ids[0], ids[1], EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    let base = "MATCH (n:concept {name: 'grouped'})";
    let ordinary = runtime
        .query(&token, &format!("{base} RETURN n.name"))
        .await
        .unwrap();
    assert_eq!(
        ordinary.len(),
        6,
        "ordinary fixed projection retains bag multiplicity"
    );
    let distinct = runtime
        .query(&token, &format!("{base} RETURN DISTINCT n.name"))
        .await
        .unwrap();
    assert_eq!(distinct.len(), 1);
    assert_eq!(distinct[0].columns.len(), 1);
    assert_eq!(distinct[0].text("n_name"), Ok("grouped"));
    for prefix in ["", "DISTINCT "] {
        let rows = runtime.query(&token, &format!("{base} RETURN {prefix}n.properties.bucket, count(n) ORDER BY n.properties.bucket")).await.unwrap();
        assert_eq!(rows.len(), 4, "hidden IDs must not split equal values");
        assert_gql_sql_value_eq!(rows[0].get("n_properties_bucket"), Some(&SqlValue::Null));
        assert!(
            matches!(
                rows[1].get("n_properties_bucket"),
                Some(SqlValue::Integer(1))
            ) || matches!(rows[1].get("n_properties_bucket"), Some(SqlValue::Float(v)) if *v == 1.0)
        );
        assert_eq!(rows[2].text("n_properties_bucket"), Ok("A"));
        assert_eq!(rows[3].text("n_properties_bucket"), Ok("a"));
        for (row, count) in rows.iter().zip([2, 2, 1, 1]) {
            assert_eq!(
                row.columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>(),
                ["n_properties_bucket", "count_n"]
            );
            assert_gql_sql_value_eq!(row.get("count_n"), Some(&SqlValue::Integer(count)));
        }
    }
    let whole = runtime
        .query(&token, &format!("{base} RETURN n, count(n) ORDER BY n.id"))
        .await
        .unwrap();
    assert_eq!(
        whole.len(),
        6,
        "whole binding legitimately includes the ID in grouping"
    );
    let whole_ids = whole
        .iter()
        .map(|row| {
            assert_gql_sql_value_eq!(row.get("count_n"), Some(&SqlValue::Integer(1)));
            row.text("n_id").unwrap().to_owned()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        whole_ids,
        ids.iter().map(ToString::to_string).collect::<BTreeSet<_>>()
    );
    for tail in ["count(n)", "DISTINCT count(n)"] {
        let empty = runtime
            .query(
                &token,
                &format!("MATCH (n:concept {{name: 'absent'}}) RETURN {tail}"),
            )
            .await
            .unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].columns.len(), 1);
        assert_gql_sql_value_eq!(empty[0].get("count_n"), Some(&SqlValue::Integer(0)));
    }
    for tail in [
        "n.name, count(n)",
        "DISTINCT n.name",
        "count(n) SKIP 1",
        "count(n) LIMIT 0",
    ] {
        assert!(
            runtime
                .query(
                    &token,
                    &format!("MATCH (n:concept {{name: 'absent'}}) RETURN {tail}")
                )
                .await
                .unwrap()
                .is_empty(),
            "{tail}"
        );
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_recursive_counts_precede_projection_dedup_and_count_repeated_final_edges() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let root = gql_metadata_entity(&runtime, &token, "count root").await;
    let left = gql_metadata_entity(&runtime, &token, "count left").await;
    let right = gql_metadata_entity(&runtime, &token, "count right").await;
    let joint = gql_metadata_entity(&runtime, &token, "count joint").await;
    let end = gql_metadata_entity(&runtime, &token, "count end").await;
    for (from, to) in [
        (root, left),
        (root, right),
        (root, joint),
        (left, joint),
        (right, joint),
        (joint, end),
    ] {
        runtime
            .link(&token, from, to, EdgeRelation::Extends, 0.5, None)
            .await
            .unwrap();
    }
    let before = gql_metadata_snapshot(&runtime).await;
    for min in [0, 1] {
        let pattern = format!(
            "MATCH (a)-[e:extends*{min}..2]->(b) WHERE a.id = '{root}' AND b.id = '{joint}'"
        );
        let distinct = runtime
            .query(&token, &format!("{pattern} RETURN DISTINCT b.id"))
            .await
            .unwrap();
        assert_eq!(distinct.len(), 1);
        assert_eq!(distinct[0].columns.len(), 1, "no implicit depth/weight");
        assert_eq!(distinct[0].text("b_id"), Ok(joint.to_string().as_str()));
        let grouped = runtime
            .query(
                &token,
                &format!("{pattern} RETURN b.id, count(a), count(b), count(e)"),
            )
            .await
            .unwrap();
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].columns.len(), 4);
        for column in ["count_a", "count_b", "count_e"] {
            assert_gql_sql_value_eq!(grouped[0].get(column), Some(&SqlValue::Integer(3)));
        }
    }
    let shared_final = runtime.query(&token, &format!("MATCH (a)-[e:extends*3..3]->(b) WHERE a.id = '{root}' AND b.id = '{end}' RETURN count(b), count(e)")).await.unwrap();
    assert_eq!(shared_final.len(), 1);
    assert_gql_sql_value_eq!(shared_final[0].get("count_b"), Some(&SqlValue::Integer(2)));
    assert_gql_sql_value_eq!(
        shared_final[0].get("count_e"),
        Some(&SqlValue::Integer(2)),
        "one final edge occurs on two matching paths"
    );
    assert_eq!(shared_final[0].columns.len(), 2);
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_zero_hop_counts_bound_nodes_but_not_null_final_edges() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let root = gql_metadata_entity(&runtime, &token, "zero count root").await;
    let pattern = format!("MATCH (a)-[e:extends*0..0]->(b) WHERE a.id = '{root}'");
    let rows = runtime
        .query(
            &token,
            &format!("{pattern} RETURN count(a), count(b), count(e)"),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].columns.len(), 3);
    for (column, count) in [("count_a", 1), ("count_b", 1), ("count_e", 0)] {
        assert_gql_sql_value_eq!(rows[0].get(column), Some(&SqlValue::Integer(count)));
    }
    let positive_empty = runtime.query(&token, &format!("MATCH (a)-[e:extends*1..2]->(b) WHERE a.id = '{root}' RETURN count(a), count(b), count(e)")).await.unwrap();
    assert_eq!(positive_empty.len(), 1);
    assert_eq!(positive_empty[0].columns.len(), 3);
    for column in ["count_a", "count_b", "count_e"] {
        assert_gql_sql_value_eq!(positive_empty[0].get(column), Some(&SqlValue::Integer(0)));
    }
    let child = gql_metadata_entity(&runtime, &token, "zero count child").await;
    runtime
        .link(&token, root, child, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();
    for (range, nodes, edges) in [("0..0", 1, 0), ("0..1", 2, 1), ("1..2", 1, 1)] {
        let rows = runtime.query(&token, &format!("MATCH (a)-[e:extends*{range}]->(b) WHERE a.id = '{root}' RETURN count(b), count(e)")).await.unwrap();
        assert_gql_sql_value_eq!(
            rows[0].get("count_b"),
            Some(&SqlValue::Integer(nodes)),
            "{range}"
        );
        assert_gql_sql_value_eq!(
            rows[0].get("count_e"),
            Some(&SqlValue::Integer(edges)),
            "{range}"
        );
        assert_eq!(rows[0].columns.len(), 2);
    }
    let groups = runtime.query(&token, &format!("MATCH (a)-[e:extends*0..1]->(b) WHERE a.id = '{root}' RETURN e.id, count(e) ORDER BY e.id")).await.unwrap();
    assert_eq!(groups.len(), 2);
    assert_gql_sql_value_eq!(groups[0].get("e_id"), Some(&SqlValue::Null));
    assert_gql_sql_value_eq!(groups[0].get("count_e"), Some(&SqlValue::Integer(0)));
    assert_gql_sql_value_eq!(groups[1].get("count_e"), Some(&SqlValue::Integer(1)));
}

#[tokio::test]
async fn gql_not_exists_returns_only_requirements_without_visible_support() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let mut requirements = Vec::new();
    for name in ["absence req 1", "absence req 2", "absence req 3"] {
        requirements.push(
            gql_return_path_entity(
                &runtime,
                &token,
                name,
                serde_json::json!({"role": "requirement"}),
            )
            .await,
        );
    }
    let first = gql_return_path_entity(
        &runtime,
        &token,
        "evidence first",
        serde_json::json!({"reviewed": true}),
    )
    .await;
    let second = gql_return_path_entity(
        &runtime,
        &token,
        "evidence second",
        serde_json::json!({"reviewed": false}),
    )
    .await;
    let unrelated = gql_metadata_entity(&runtime, &token, "unrelated requirement").await;
    for (from, to) in [
        (first, requirements[0]),
        (second, requirements[0]),
        (first, unrelated),
        (first, second),
    ] {
        runtime
            .link(&token, from, to, EdgeRelation::Supports, 0.5, None)
            .await
            .unwrap();
    }
    let before = gql_metadata_snapshot(&runtime).await;
    for inner in [
        "(f:concept)-[:supports]->(r)",
        "(r)<-[:supports]-(f:concept)",
        "(r)-[:supports]-(f:concept)",
    ] {
        let rows = runtime.query(&token, &format!("MATCH (r:concept {{role: 'requirement'}}) WHERE NOT EXISTS {{ {inner} }} RETURN r.name ORDER BY r.name")).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.text("r_name").unwrap())
                .collect::<Vec<_>>(),
            ["absence req 2", "absence req 3"],
            "{inner}"
        );
    }
    for (flag, expected) in [
        ("false", vec!["absence req 2", "absence req 3"]),
        (
            "true",
            vec!["absence req 1", "absence req 2", "absence req 3"],
        ),
    ] {
        let rows = runtime.query(&token, &format!("MATCH (r:concept {{role: 'requirement'}}) WHERE NOT EXISTS {{ (f:concept)-[:supports]->(m:concept {{reviewed: {flag}}})-[:supports]->(r) }} RETURN r.name ORDER BY r.name")).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.text("r_name").unwrap())
                .collect::<Vec<_>>(),
            expected,
            "intermediate inline constraint {flag}"
        );
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_not_exists_preserves_boolean_and_comma_binding_scope() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let fixture = gql_comma_star(&runtime, &token).await;
    runtime
        .link(
            &token,
            fixture.left[0].target_id,
            fixture.right[0].target_id,
            EdgeRelation::Supports,
            0.5,
            None,
        )
        .await
        .unwrap();
    let always = gql_return_path_entity(
        &runtime,
        &token,
        "absence always",
        serde_json::json!({"role": "boolean", "eligible": false}),
    )
    .await;
    let eligible = gql_return_path_entity(
        &runtime,
        &token,
        "absence eligible",
        serde_json::json!({"role": "boolean", "eligible": true}),
    )
    .await;
    let neither = gql_return_path_entity(
        &runtime,
        &token,
        "absence neither",
        serde_json::json!({"role": "boolean", "eligible": false}),
    )
    .await;
    runtime
        .link(&token, neither, always, EdgeRelation::Supports, 0.5, None)
        .await
        .unwrap();
    runtime
        .link(&token, always, eligible, EdgeRelation::Extends, 0.5, None)
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    let rows = runtime.query(&token, "MATCH (r:concept {role: 'boolean'}) WHERE r.name = 'absence always' OR NOT EXISTS { (x)-[:supports]->(r) } AND r.properties.eligible = true RETURN r.name ORDER BY r.name").await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.text("r_name").unwrap())
            .collect::<Vec<_>>(),
        ["absence always", "absence eligible"],
        "AND precedes OR"
    );
    let rows = runtime.query(&token, "MATCH (r:concept {role: 'boolean'}) WHERE NOT EXISTS { (x)-[local:supports]->(r) } AND NOT EXISTS { (x)-[local:extends]->(r) } RETURN r.name").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].text("r_name"),
        Ok("absence neither"),
        "independent blocks may reuse local names"
    );
    let source = format!(
        "{} AND NOT EXISTS {{ (b)-[inner:supports]->(c) }}",
        fixture.pattern()
    );
    let rows = runtime
        .query(&token, &format!("{source} RETURN e.id, f.id"))
        .await
        .unwrap();
    let removed = (
        fixture.left[0].id.to_string(),
        fixture.right[0].id.to_string(),
    );
    let expected: Vec<_> = fixture
        .pairs()
        .into_iter()
        .filter(|pair| pair != &removed)
        .collect();
    assert_eq!(gql_comma_pairs(&rows), expected);
    let rows = runtime
        .query(
            &token,
            &format!("{source} RETURN count(a), count(e), count(f)"),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    for column in ["count_a", "count_e", "count_f"] {
        assert_gql_sql_value_eq!(rows[0].get(column), Some(&SqlValue::Integer(5)));
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_recursive_not_exists_correlates_start_and_end_before_counts() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let mut ids = Vec::new();
    for name in [
        "absence root",
        "absence left",
        "absence right",
        "absence join",
        "absence tail",
    ] {
        ids.push(gql_metadata_entity(&runtime, &token, name).await);
    }
    for (from, to) in [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)] {
        runtime
            .link(
                &token,
                ids[from],
                ids[to],
                EdgeRelation::Extends,
                0.25,
                None,
            )
            .await
            .unwrap();
    }
    runtime
        .link(&token, ids[0], ids[1], EdgeRelation::Supports, 0.5, None)
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    for min in [0, 1] {
        for inner in ["(a)-[:supports]->(b)", "(f)-[:supports]->(b)"] {
            let source = format!("MATCH (a)-[e:extends*{min}..3]->(b) WHERE a.id = '{}' AND NOT EXISTS {{ {inner} }}", ids[0]);
            let rows = runtime
                .query(
                    &token,
                    &format!("{source} RETURN b.name, count(b), count(e) ORDER BY b.name"),
                )
                .await
                .unwrap();
            let mut expected = vec![("absence join", 2, 2), ("absence right", 1, 1)];
            if min == 0 {
                expected.push(("absence root", 1, 0));
            }
            expected.push(("absence tail", 2, 2));
            assert_eq!(rows.len(), expected.len());
            for (row, (name, node_count, edge_count)) in rows.iter().zip(expected) {
                assert_eq!(
                    row.columns.len(),
                    3,
                    "grouped absence adds no hidden metrics"
                );
                assert_eq!(row.text("b_name"), Ok(name));
                assert_gql_sql_value_eq!(row.get("count_b"), Some(&SqlValue::Integer(node_count)));
                assert_gql_sql_value_eq!(row.get("count_e"), Some(&SqlValue::Integer(edge_count)));
            }
            let rows = runtime
                .query(
                    &token,
                    &format!("{source} RETURN DISTINCT b.name ORDER BY b.name"),
                )
                .await
                .unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| row.text("b_name").unwrap())
                    .collect::<Vec<_>>(),
                if min == 0 {
                    vec![
                        "absence join",
                        "absence right",
                        "absence root",
                        "absence tail",
                    ]
                } else {
                    vec!["absence join", "absence right", "absence tail"]
                }
            );
            assert!(rows.iter().all(|row| row.columns.len() == 1));
        }
    }
    let source = format!("MATCH (a)-[e:extends*0..0]->(b) WHERE a.id = '{}' AND NOT EXISTS {{ (a)-[:supports]->(b) }}", ids[0]);
    let rows = runtime
        .query(&token, &format!("{source} RETURN b.id, e.id, e.metadata"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text("b_id"), Ok(ids[0].to_string().as_str()));
    for column in ["e_id", "e_metadata"] {
        assert_gql_sql_value_eq!(rows[0].get(column), Some(&SqlValue::Null));
    }
    assert_gql_sql_value_eq!(rows[0].get("_depth"), Some(&SqlValue::Integer(0)));
    assert_gql_sql_value_eq!(rows[0].get("_total_weight"), Some(&SqlValue::Float(0.0)));
    let rows = runtime
        .query(
            &token,
            &format!("{source} RETURN count(a), count(b), count(e)"),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    for column in ["count_a", "count_b"] {
        assert_gql_sql_value_eq!(rows[0].get(column), Some(&SqlValue::Integer(1)));
    }
    assert_gql_sql_value_eq!(rows[0].get("count_e"), Some(&SqlValue::Integer(0)));
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_mixed_segments_preserve_each_final_edge_and_variable_only_metrics() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let mut ids = Vec::new();
    for name in [
        "prefix",
        "first start",
        "first middle",
        "bridge",
        "second start",
        "second middle",
        "second end",
        "attachment 1",
        "attachment 2",
    ] {
        ids.push(
            gql_return_path_entity(
                &runtime,
                &token,
                name,
                serde_json::json!({"fixture": "mixed bindings", "large": 9007199254740993_i64}),
            )
            .await,
        );
    }
    let mut edges = Vec::new();
    for (from, to, relation, weight, slot) in [
        (0, 1, EdgeRelation::Contains, 0.875, "prefix"),
        (1, 2, EdgeRelation::Extends, 0.125, "first intermediate"),
        (2, 3, EdgeRelation::VariantOf, 0.25, "first final"),
        (3, 4, EdgeRelation::Contains, 1.0, "bridge"),
        (5, 4, EdgeRelation::Extends, 0.0625, "second intermediate"),
        (6, 5, EdgeRelation::VariantOf, 0.125, "second final"),
        (6, 7, EdgeRelation::Contains, 0.9375, "attachment 1"),
        (6, 8, EdgeRelation::Contains, 0.75, "attachment 2"),
    ] {
        edges.push(
            runtime
                .link(
                    &token,
                    ids[from],
                    ids[to],
                    relation,
                    weight,
                    Some(serde_json::json!({"slot": slot, "large": 9007199254740993_i64})),
                )
                .await
                .unwrap(),
        );
    }
    let mut docs = Vec::new();
    for name in [
        "dependency root",
        "dependency child",
        "root source",
        "child source 1",
        "child source 2",
    ] {
        docs.push(
            runtime
                .create_entity_with_embedding_report(
                    &token,
                    "document",
                    Some("paper"),
                    name,
                    None,
                    Some(serde_json::json!({"family": "mixed dependency"})),
                    vec![],
                )
                .await
                .unwrap()
                .0
                .id,
        );
    }
    runtime
        .link(
            &token,
            docs[0],
            docs[1],
            EdgeRelation::DependsOn,
            0.25,
            None,
        )
        .await
        .unwrap();
    for (from, to) in [(0, 2), (1, 3), (1, 4)] {
        runtime
            .link(
                &token,
                docs[from],
                docs[to],
                EdgeRelation::DerivedFrom,
                0.875,
                None,
            )
            .await
            .unwrap();
    }
    let before = gql_metadata_snapshot(&runtime).await;
    for (first, second) in [
        (
            "-[e:extends|variant_of*2..2]->",
            "<-[g:extends|variant_of*2..2]-",
        ),
        (
            "-[e:extends|variant_of*2..2]-",
            "-[g:extends|variant_of*2..2]-",
        ),
    ] {
        let pattern = format!("MATCH (u)-[p:contains]->(a){first}(b:concept {{fixture: 'mixed bindings'}})-[f:contains]->(c){second}(d)-[h:contains]->(z) WHERE u.id = '{}' AND b.id = '{}' AND d.id = '{}' AND b.entity_type IS NULL", ids[0], ids[3], ids[6]);
        let rows = runtime.query(&token, &format!("{pattern} RETURN b.properties.large, e.id, e.weight, e.metadata.slot, e.metadata.large, f.id, f.metadata.slot, g.id, g.weight, g.metadata.slot, p.id, p.metadata.slot, h.id, h.metadata.slot, z.name ORDER BY z.name")).await.unwrap();
        assert_eq!(rows.len(), 2);
        for (row, suffix) in rows.iter().zip([6, 7]) {
            for (column, edge) in [
                ("p_id", 0),
                ("e_id", 2),
                ("f_id", 3),
                ("g_id", 5),
                ("h_id", suffix),
            ] {
                assert_eq!(
                    row.text(column).unwrap(),
                    edges[edge].id.to_string(),
                    "{column}"
                );
            }
            for (column, value) in [
                ("p_metadata_slot", "prefix"),
                ("e_metadata_slot", "first final"),
                ("f_metadata_slot", "bridge"),
                ("g_metadata_slot", "second final"),
            ] {
                assert_eq!(row.text(column), Ok(value));
            }
            let attachment = if suffix == 6 {
                "attachment 1"
            } else {
                "attachment 2"
            };
            assert_eq!(row.text("h_metadata_slot"), Ok(attachment));
            assert_eq!(row.text("z_name"), Ok(attachment));
            for column in ["b_properties_large", "e_metadata_large"] {
                assert_gql_sql_value_eq!(
                    row.get(column),
                    Some(&SqlValue::Integer(9007199254740993_i64))
                );
            }
            assert_gql_sql_value_eq!(row.get("e_weight"), Some(&SqlValue::Float(0.25)));
            assert_gql_sql_value_eq!(row.get("g_weight"), Some(&SqlValue::Float(0.125)));
            assert_gql_sql_value_eq!(row.get("_depth"), Some(&SqlValue::Integer(4)));
            assert_gql_sql_value_eq!(
                row.get("_total_weight"),
                Some(&SqlValue::Float(0.5625)),
                "fixed weights must not contribute"
            );
        }
        for wrong in [
            "b:document {fixture: 'mixed bindings'}",
            "b:concept {fixture: 'missing'}",
        ] {
            let query = pattern.replace("b:concept {fixture: 'mixed bindings'}", wrong);
            assert!(runtime
                .query(&token, &format!("{query} RETURN z.name"))
                .await
                .unwrap()
                .is_empty());
        }
    }
    let dependencies = format!("MATCH (a:document)-[e:depends_on*0..2]->(b:document {{entity_type: 'paper', family: 'mixed dependency'}})-[f:derived_from]->(s:document) WHERE a.id = '{}' RETURN s.name ORDER BY s.name", docs[0]);
    let rows = runtime.query(&token, &dependencies).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.text("s_name").unwrap())
            .collect::<Vec<_>>(),
        ["child source 1", "child source 2", "root source"]
    );
    assert!(runtime
        .query(
            &token,
            &dependencies.replace("entity_type: 'paper'", "entity_type: 'summary'")
        )
        .await
        .unwrap()
        .is_empty());
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_mixed_zero_segments_join_following_fixed_steps_and_count_null_edges() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let root = gql_metadata_entity(&runtime, &token, "mixed zero root").await;
    let attachment = gql_metadata_entity(&runtime, &token, "mixed zero attachment").await;
    let fixed = runtime
        .link(
            &token,
            root,
            attachment,
            EdgeRelation::Contains,
            0.875,
            None,
        )
        .await
        .unwrap();
    let mut same_uuid_note = khive_storage::Note::new("local", "observation", "typed zero decoy");
    same_uuid_note.id = root;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(same_uuid_note)
        .await
        .unwrap();
    let before = gql_metadata_snapshot(&runtime).await;
    let pattern = format!("MATCH (a:entity)-[e:extends*0..0]->(b:concept)-[f:contains]->(c:concept)-[g:variant_of*0..0]->(d:entity) WHERE a.id = '{root}'");
    let rows = runtime.query(&token, &format!("{pattern} RETURN a.id, b.id, c.id, d.id, e.id, e.relation, e.weight, e.metadata, f.id, g.id, g.relation, g.weight, g.metadata")).await.unwrap();
    assert_eq!(rows.len(), 1);
    for column in ["a_id", "b_id"] {
        assert_eq!(rows[0].text(column).unwrap(), root.to_string());
    }
    for column in ["c_id", "d_id"] {
        assert_eq!(rows[0].text(column).unwrap(), attachment.to_string());
    }
    assert_eq!(rows[0].text("f_id").unwrap(), fixed.id.to_string());
    for column in [
        "e_id",
        "e_relation",
        "e_weight",
        "e_metadata",
        "g_id",
        "g_relation",
        "g_weight",
        "g_metadata",
    ] {
        assert_gql_sql_value_eq!(rows[0].get(column), Some(&SqlValue::Null), "{column}");
    }
    assert_gql_sql_value_eq!(rows[0].get("_depth"), Some(&SqlValue::Integer(0)));
    assert_gql_sql_value_eq!(rows[0].get("_total_weight"), Some(&SqlValue::Float(0.0)));
    let counts = runtime.query(&token, &format!("{pattern} RETURN count(a), count(b), count(c), count(d), count(e), count(f), count(g)")).await.unwrap();
    assert_eq!(counts.len(), 1);
    assert_eq!(counts[0].columns.len(), 7);
    for (column, value) in [
        ("count_a", 1),
        ("count_b", 1),
        ("count_c", 1),
        ("count_d", 1),
        ("count_e", 0),
        ("count_f", 1),
        ("count_g", 0),
    ] {
        assert_gql_sql_value_eq!(counts[0].get(column), Some(&SqlValue::Integer(value)));
    }
    for (from, to) in [
        ("a:entity", "a:document"),
        ("b:concept", "b:note"),
        ("b:concept", "b:concept {fixture_name: 'missing'}"),
        ("c:concept", "c:document"),
        ("d:entity", "d:note"),
        ("e:extends*0..0", "e:extends*1..2"),
        ("g:variant_of*0..0", "g:variant_of*1..2"),
    ] {
        let source = pattern.replace(from, to);
        assert!(
            runtime
                .query(&token, &format!("{source} RETURN a.id"))
                .await
                .unwrap()
                .is_empty(),
            "{source}"
        );
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_mixed_segment_cycle_state_is_local() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let a = gql_metadata_entity(&runtime, &token, "cycle a").await;
    let b = gql_metadata_entity(&runtime, &token, "cycle b").await;
    for (from, to, relation, weight) in [
        (a, b, EdgeRelation::Extends, 0.5),
        (b, a, EdgeRelation::Extends, 0.5),
        (b, a, EdgeRelation::VariantOf, 0.25),
        (a, b, EdgeRelation::VariantOf, 0.25),
    ] {
        runtime
            .link(&token, from, to, relation, weight, None)
            .await
            .unwrap();
    }
    let x = gql_metadata_entity(&runtime, &token, "loop x").await;
    let y = gql_metadata_entity(&runtime, &token, "loop y").await;
    let unused = gql_metadata_entity(&runtime, &token, "loop fixture donor").await;
    runtime
        .link(&token, x, y, EdgeRelation::Contains, 1.0, None)
        .await
        .unwrap();
    // Trusted storage represents existing self-loops; public link refuses new ones.
    for (node, relation, weight) in [
        (x, EdgeRelation::Extends, 0.125),
        (y, EdgeRelation::VariantOf, 0.25),
    ] {
        let mut edge = runtime
            .link(&token, node, unused, relation, weight, None)
            .await
            .unwrap();
        edge.target_id = node;
        runtime
            .graph(&token)
            .unwrap()
            .upsert_edge(edge)
            .await
            .unwrap();
    }
    let before = gql_metadata_snapshot(&runtime).await;
    let source = format!("MATCH (a)-[e:extends*1..3]->(b)-[f:variant_of*1..3]->(c) WHERE a.id = '{a}' RETURN a.id, b.id, c.id");
    let rows = runtime.query(&token, &source).await.unwrap();
    assert_eq!(
        rows.len(),
        1,
        "neither segment may revisit its own start after the first expansion"
    );
    assert_eq!(rows[0].text("a_id").unwrap(), a.to_string());
    assert_eq!(rows[0].text("b_id").unwrap(), b.to_string());
    assert_eq!(
        rows[0].text("c_id").unwrap(),
        a.to_string(),
        "a later segment may revisit a node from an earlier segment"
    );
    assert_gql_sql_value_eq!(rows[0].get("_depth"), Some(&SqlValue::Integer(2)));
    assert_gql_sql_value_eq!(rows[0].get("_total_weight"), Some(&SqlValue::Float(0.75)));
    let loops = runtime.query(&token, &format!("MATCH (a)-[e:extends*1..3]->(b)-[:contains]->(c)-[g:variant_of*1..3]->(d) WHERE a.id = '{x}' RETURN b.id, d.id")).await.unwrap();
    assert_eq!(
        loops.len(),
        1,
        "first-step loops survive in both segments but cannot recur"
    );
    assert_eq!(loops[0].text("b_id").unwrap(), x.to_string());
    assert_eq!(loops[0].text("d_id").unwrap(), y.to_string());
    assert_gql_sql_value_eq!(loops[0].get("_depth"), Some(&SqlValue::Integer(2)));
    assert_gql_sql_value_eq!(loops[0].get("_total_weight"), Some(&SqlValue::Float(0.375)));
    let refusal = runtime
        .query(
            &token,
            &source
                .replace("(c)", "(a)")
                .replace("a.id, b.id, c.id", "a.id"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refusal.refusal_source(), khive_runtime::RuntimeError::Query(khive_query::QueryError::Unsupported(message)) if message.contains("repeated node variable 'a'"))
    );
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}

#[tokio::test]
async fn gql_mixed_counts_preserve_path_multiplicity_before_projection_dedup() {
    use khive_storage::SqlValue;
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();
    gql_metadata_observation(&runtime, &token).await;
    let mut ids = Vec::new();
    for name in ["diamond root", "left", "right", "joint", "end"] {
        ids.push(gql_metadata_entity(&runtime, &token, name).await);
    }
    for (from, to) in [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)] {
        runtime
            .link(
                &token,
                ids[from],
                ids[to],
                EdgeRelation::Extends,
                0.125,
                None,
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let target = gql_return_path_entity(
            &runtime,
            &token,
            "same attachment",
            serde_json::json!({"bucket": "one"}),
        )
        .await;
        runtime
            .link(&token, ids[4], target, EdgeRelation::Contains, 0.875, None)
            .await
            .unwrap();
    }
    let typed_root = gql_metadata_entity(&runtime, &token, "typed mixed root").await;
    let typed_middle = gql_metadata_entity(&runtime, &token, "typed mixed middle").await;
    runtime
        .link(
            &token,
            typed_root,
            typed_middle,
            EdgeRelation::Contains,
            1.0,
            None,
        )
        .await
        .unwrap();
    for props in [
        serde_json::json!({"bucket":null}),
        serde_json::json!({}),
        serde_json::json!({"bucket":1}),
        serde_json::json!({"bucket":1.0}),
        serde_json::json!({"bucket":"1"}),
        serde_json::json!({"bucket":"A"}),
        serde_json::json!({"bucket":"a"}),
    ] {
        let target = gql_return_path_entity(&runtime, &token, "typed mixed result", props).await;
        runtime
            .link(
                &token,
                typed_middle,
                target,
                EdgeRelation::Extends,
                0.125,
                None,
            )
            .await
            .unwrap();
    }
    let before = gql_metadata_snapshot(&runtime).await;
    let pattern = format!(
        "MATCH (a)-[e:extends*3..3]->(b)-[f:contains]->(c) WHERE a.id = '{}'",
        ids[0]
    );
    let ordinary = runtime
        .query(&token, &format!("{pattern} RETURN c.properties.bucket"))
        .await
        .unwrap();
    assert_eq!(
        ordinary.len(),
        1,
        "ordinary projection deduplicates only complete value-plus-metric rows"
    );
    assert_eq!(ordinary[0].columns.len(), 3);
    assert_gql_sql_value_eq!(ordinary[0].get("_depth"), Some(&SqlValue::Integer(3)));
    assert_gql_sql_value_eq!(
        ordinary[0].get("_total_weight"),
        Some(&SqlValue::Float(0.375))
    );
    for prefix in ["", "DISTINCT "] {
        let groups = runtime.query(&token, &format!("{pattern} RETURN {prefix}c.properties.bucket, count(a), count(b), count(c), count(e), count(f)")).await.unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].columns.len(), 6);
        assert_eq!(groups[0].text("c_properties_bucket"), Ok("one"));
        for column in ["count_a", "count_b", "count_c", "count_e", "count_f"] {
            assert_gql_sql_value_eq!(
                groups[0].get(column),
                Some(&SqlValue::Integer(4)),
                "two paths sharing the final edge times two suffix bindings: {column}"
            );
        }
    }
    let distinct = runtime
        .query(
            &token,
            &format!("{pattern} RETURN DISTINCT c.properties.bucket"),
        )
        .await
        .unwrap();
    assert_eq!(distinct.len(), 1);
    assert_eq!(distinct[0].columns.len(), 1);
    let typed =
        format!("MATCH (r)-[:contains]->(m)-[e:extends*1..2]->(n) WHERE r.id = '{typed_root}'");
    let rows = runtime
        .query(
            &token,
            &format!("{typed} RETURN n.properties.bucket, count(n) ORDER BY n.properties.bucket"),
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 5);
    assert_gql_sql_value_eq!(rows[0].get("n_properties_bucket"), Some(&SqlValue::Null));
    assert!(
        matches!(
            rows[1].get("n_properties_bucket"),
            Some(SqlValue::Integer(1))
        ) || matches!(rows[1].get("n_properties_bucket"), Some(SqlValue::Float(v)) if *v == 1.0)
    );
    for (row, text) in rows[2..].iter().zip(["1", "A", "a"]) {
        assert_eq!(row.text("n_properties_bucket"), Ok(text));
    }
    for (row, count) in rows.iter().zip([2, 2, 1, 1, 1]) {
        assert_eq!(row.columns.len(), 2);
        assert_gql_sql_value_eq!(row.get("count_n"), Some(&SqlValue::Integer(count)));
    }
    let missing = format!("{typed} AND n.name = 'absent'");
    for projection in ["count(n)", "DISTINCT count(n)"] {
        let rows = runtime
            .query(&token, &format!("{missing} RETURN {projection}"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].columns.len(), 1);
        assert_gql_sql_value_eq!(rows[0].get("count_n"), Some(&SqlValue::Integer(0)));
    }
    for projection in ["n.name, count(n)", "DISTINCT n.name"] {
        assert!(runtime
            .query(&token, &format!("{missing} RETURN {projection}"))
            .await
            .unwrap()
            .is_empty());
    }
    assert_eq!(gql_metadata_snapshot(&runtime).await, before);
}
