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
        "(a)-[e:extends]-(b), (c)-[f:variant_of]-(d), (b)-[g:extends]-(c)",
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
