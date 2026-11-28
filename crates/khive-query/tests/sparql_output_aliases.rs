//! Execute lowered aliases against a private SQLite database, not just SQL text.

use khive_query::ast::{ConditionValue, PatternElement};
use khive_query::parsers::{gql, sparql};
use khive_query::{compile, CompileOptions, GqlQuery, QueryValue, ReturnItem};
use rusqlite::{params_from_iter, types::Value, Connection};

fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    // The four union members and observation table match the existing query fixture.
    conn.execute_batch(
        "CREATE TABLE entities (
            id TEXT PRIMARY KEY, namespace TEXT, kind TEXT, entity_type TEXT,
            name TEXT, description TEXT, properties TEXT, created_at INTEGER,
            updated_at INTEGER, deleted_at INTEGER
        );
        CREATE TABLE notes (
            id TEXT PRIMARY KEY, namespace TEXT, kind TEXT, name TEXT, content TEXT,
            status TEXT, salience REAL, decay_factor REAL, properties TEXT,
            created_at INTEGER, updated_at INTEGER, deleted_at INTEGER
        );
        CREATE TABLE events (
            id TEXT PRIMARY KEY, namespace TEXT, kind TEXT, verb TEXT, substrate TEXT,
            actor TEXT, outcome TEXT, payload TEXT, created_at INTEGER,
            duration_us INTEGER, target_id TEXT, session_id TEXT
        );
        CREATE TABLE graph_edges (
            id TEXT PRIMARY KEY, namespace TEXT, source_id TEXT, target_id TEXT,
            relation TEXT, weight REAL, metadata TEXT, created_at INTEGER,
            updated_at INTEGER, deleted_at INTEGER
        );
        CREATE TABLE event_observations (
            event_id TEXT, entity_id TEXT, referent_kind TEXT, role TEXT, position INTEGER
        );
        INSERT INTO entities VALUES
            ('node-a', 'local', 'concept', 'thing', 'Source', NULL, '{}', 10, 11, NULL),
            ('node-z', 'local', 'concept', NULL, 'Zulu', NULL, '{}', 30, 31, NULL),
            ('node-b', 'local', 'concept', NULL, 'Alpha', NULL, '{}', 20, 21, NULL);
        INSERT INTO graph_edges VALUES
            ('edge-z', 'local', 'node-a', 'node-z', 'extends', 2.0, '{}', 1, 1, NULL),
            ('edge-a', 'local', 'node-a', 'node-b', 'extends', 2.0, '{}', 1, 1, NULL),
            ('edge-next', 'local', 'node-b', 'node-z', 'extends', 1.0, '{}', 1, 1, NULL);
        INSERT INTO events VALUES
            ('event-1', 'local', 'recall_executed', 'memory.recall', 'note',
             'fixture', 'success', '{}', 40, 0, NULL, NULL);
        INSERT INTO notes VALUES
            ('note-1', 'local', 'memory', NULL, 'remember', 'active', 0.5, 1.0,
             '{}', 50, 51, NULL);
        INSERT INTO event_observations VALUES
            ('event-1', 'note-1', 'note', 'selected', 0);",
    )
    .unwrap();
    conn
}

fn run(conn: &Connection, query: &GqlQuery, columns: &[&str]) -> Vec<Vec<Value>> {
    let compiled = compile(
        query,
        &CompileOptions {
            scopes: vec!["local".into()],
            max_limit: 100,
        },
    )
    .unwrap();
    assert_eq!(compiled.return_vars, query.return_items);
    let params = compiled.params.iter().map(|value| match value {
        QueryValue::Null => Value::Null,
        QueryValue::Integer(value) => Value::Integer(*value),
        QueryValue::Float(value) => Value::Real(*value),
        QueryValue::Text(value) => Value::Text(value.clone()),
        QueryValue::Blob(value) => Value::Blob(value.clone()),
    });
    let mut statement = conn
        .prepare(&compiled.sql)
        .unwrap_or_else(|error| panic!("{error}; SQL: {}", compiled.sql));
    assert_eq!(statement.column_names().collect::<Vec<_>>(), columns);
    let width = statement.column_count();
    let rows = statement
        .query_map(params_from_iter(params), |row| {
            (0..width)
                .map(|index| row.get::<_, Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        !rows.is_empty(),
        "the fixture must exercise the result path"
    );
    rows
}

fn cells(rows: &[Vec<Value>], indices: &[usize]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| indices.iter().map(|&index| row[index].clone()).collect())
        .collect()
}

fn text(value: &str) -> Value {
    Value::Text(value.into())
}

fn path_query(recursive: bool) -> GqlQuery {
    sparql::parse(if recursive {
        "SELECT ?123 ?456 WHERE { ?123 :name 'Source' . ?123 :extends{1,2} ?456 } LIMIT 20"
    } else {
        "SELECT ?123 ?456 WHERE { ?123 :name 'Source' . ?123 :extends ?456 } LIMIT 20"
    })
    .unwrap()
}

fn with_edge_binding(mut query: GqlQuery) -> GqlQuery {
    let PatternElement::Edge(edge) = &mut query.pattern.elements[1] else {
        panic!("edge")
    };
    edge.variable = Some("789".into());
    query
}

#[test]
fn digit_leading_sparql_nodes_execute_in_fixed_and_recursive_paths() {
    let conn = fixture();
    let columns = [
        "123_id",
        "123_namespace",
        "123_kind",
        "123_entity_type",
        "123_name",
        "123_properties",
        "123_created_at",
        "123_updated_at",
        "456_id",
        "456_namespace",
        "456_kind",
        "456_entity_type",
        "456_name",
        "456_properties",
        "456_created_at",
        "456_updated_at",
    ];
    let fixed = run(&conn, &path_query(false), &columns);
    assert_eq!(
        cells(&fixed, &[0, 4, 8, 12]),
        vec![
            vec![
                text("node-a"),
                text("Source"),
                text("node-b"),
                text("Alpha")
            ],
            vec![text("node-a"), text("Source"), text("node-z"), text("Zulu")],
        ]
    );
    let mut recursive_columns = columns.to_vec();
    recursive_columns.extend(["_depth", "_total_weight"]);
    let recursive = run(&conn, &path_query(true), &recursive_columns);
    assert_eq!(
        cells(&recursive, &[0, 8, 16, 17]),
        vec![
            vec![
                text("node-a"),
                text("node-b"),
                Value::Integer(1),
                Value::Real(2.0)
            ],
            vec![
                text("node-a"),
                text("node-z"),
                Value::Integer(1),
                Value::Real(2.0)
            ],
            vec![
                text("node-a"),
                text("node-z"),
                Value::Integer(2),
                Value::Real(3.0)
            ],
        ]
    );
}

#[test]
fn numeric_synthetic_event_and_referent_projections_execute() {
    let conn = fixture();
    let mut query =
        sparql::parse("SELECT ?7 ?8 WHERE { ?7 :observed_as_selected ?8 } LIMIT 20").unwrap();
    let rows = run(
        &conn,
        &query,
        &[
            "7_id",
            "7_namespace",
            "7_verb",
            "7_substrate",
            "7_actor",
            "7_kind",
            "7_outcome",
            "7_payload",
            "7_created_at",
            "8_id",
            "8_namespace",
            "8_kind",
            "8_entity_type",
            "8_status",
            "8_content",
            "8_salience",
            "8_properties",
            "8_created_at",
            "8_updated_at",
            "8_referent_kind",
        ],
    );
    assert_eq!(
        cells(&rows, &[0, 2, 9, 13, 14, 19]),
        vec![vec![
            text("event-1"),
            text("memory.recall"),
            text("note-1"),
            text("active"),
            text("remember"),
            text("note"),
        ]]
    );
    // Property projections are public AST input, not additional SPARQL SELECT syntax.
    query.return_items = vec![
        ReturnItem::Property("7".into(), "verb".into()),
        ReturnItem::Property("8".into(), "content".into()),
        ReturnItem::Property("8".into(), "salience".into()),
    ];
    assert_eq!(
        run(&conn, &query, &["7_verb", "8_content", "8_salience"]),
        vec![vec![
            text("memory.recall"),
            text("remember"),
            Value::Real(0.5)
        ]]
    );
}

#[test]
fn numeric_public_ast_edge_projections_execute_in_both_paths() {
    let conn = fixture();
    for recursive in [false, true] {
        let mut query = with_edge_binding(path_query(recursive));
        query.return_items = vec![ReturnItem::Variable("789".into())];
        let (columns, expected) = if recursive {
            (
                vec![
                    "789_id",
                    "789_relation",
                    "789_weight",
                    "_depth",
                    "_total_weight",
                ],
                vec![
                    vec![
                        text("edge-a"),
                        text("extends"),
                        Value::Real(2.0),
                        Value::Integer(1),
                        Value::Real(2.0),
                    ],
                    vec![
                        text("edge-z"),
                        text("extends"),
                        Value::Real(2.0),
                        Value::Integer(1),
                        Value::Real(2.0),
                    ],
                    vec![
                        text("edge-next"),
                        text("extends"),
                        Value::Real(1.0),
                        Value::Integer(2),
                        Value::Real(3.0),
                    ],
                ],
            )
        } else {
            (
                vec![
                    "789_id",
                    "789_source",
                    "789_target",
                    "789_relation",
                    "789_weight",
                ],
                vec![
                    vec![
                        text("edge-a"),
                        text("node-a"),
                        text("node-b"),
                        text("extends"),
                        Value::Real(2.0),
                    ],
                    vec![
                        text("edge-z"),
                        text("node-a"),
                        text("node-z"),
                        text("extends"),
                        Value::Real(2.0),
                    ],
                ],
            )
        };
        assert_eq!(run(&conn, &query, &columns), expected);
    }
}

#[test]
fn numeric_public_ast_node_and_edge_properties_execute_in_both_paths() {
    let conn = fixture();
    for recursive in [false, true] {
        let mut query = with_edge_binding(path_query(recursive));
        query.return_items = vec![
            ReturnItem::Property("123".into(), "name".into()),
            ReturnItem::Property("456".into(), "name".into()),
            ReturnItem::Property("789".into(), "id".into()),
            ReturnItem::Property("789".into(), "relation".into()),
            ReturnItem::Property("789".into(), "weight".into()),
        ];
        let mut columns = vec![
            "123_name",
            "456_name",
            "789_id",
            "789_relation",
            "789_weight",
        ];
        let mut expected = vec![
            vec![
                text("Source"),
                text("Alpha"),
                text("edge-a"),
                text("extends"),
                Value::Real(2.0),
            ],
            vec![
                text("Source"),
                text("Zulu"),
                text("edge-z"),
                text("extends"),
                Value::Real(2.0),
            ],
        ];
        if recursive {
            columns.extend(["_depth", "_total_weight"]);
            for row in &mut expected {
                row.extend([Value::Integer(1), Value::Real(2.0)]);
            }
            expected.push(vec![
                text("Source"),
                text("Zulu"),
                text("edge-next"),
                text("extends"),
                Value::Real(1.0),
                Value::Integer(2),
                Value::Real(3.0),
            ]);
        }
        assert_eq!(run(&conn, &query, &columns), expected);
    }
}

#[test]
fn public_ast_delimiters_and_quotes_preserve_names_and_recursive_order() {
    let conn = fixture();
    for recursive in [false, true] {
        let mut query = path_query(recursive);
        assert!(query.where_clause.is_true());
        assert_eq!(
            query.pattern.nodes().next().unwrap().properties.get("name"),
            Some(&ConditionValue::String("Source".into()))
        );
        for (element, variable) in query.pattern.elements.iter_mut().zip([
            "left, AS \"source\"",
            "link AS , \"edge\"",
            "right, AS \"target\"",
        ]) {
            match element {
                PatternElement::Node(node) => node.variable = Some(variable.into()),
                PatternElement::Edge(edge) => edge.variable = Some(variable.into()),
            }
        }
        query.return_items = vec![
            ReturnItem::Property("left, AS \"source\"".into(), "name".into()),
            ReturnItem::Property("right, AS \"target\"".into(), "name".into()),
            ReturnItem::Property("link AS , \"edge\"".into(), "id".into()),
        ];
        let mut columns = vec![
            "left, AS \"source\"_name",
            "right, AS \"target\"_name",
            "link AS , \"edge\"_id",
        ];
        let mut expected = vec![
            vec![text("Source"), text("Alpha"), text("edge-a")],
            vec![text("Source"), text("Zulu"), text("edge-z")],
        ];
        if recursive {
            columns.extend(["_depth", "_total_weight"]);
            for row in &mut expected {
                row.extend([Value::Integer(1), Value::Real(2.0)]);
            }
            expected.push(vec![
                text("Source"),
                text("Zulu"),
                text("edge-next"),
                Value::Integer(2),
                Value::Real(3.0),
            ]);
        }
        assert_eq!(run(&conn, &query, &columns), expected);
    }
}

#[test]
fn ordinary_gql_result_column_names_remain_exact() {
    let conn = fixture();
    let query = gql::parse(
        "MATCH (a {name: 'Source'})-[_edge:extends]->(b) RETURN a.name, b.name, _edge.id LIMIT 20",
    )
    .unwrap();
    assert_eq!(
        run(&conn, &query, &["a_name", "b_name", "_edge_id"]),
        vec![
            vec![text("Source"), text("Alpha"), text("edge-a")],
            vec![text("Source"), text("Zulu"), text("edge-z")],
        ]
    );
}

#[test]
fn typed_return_paths_preserve_public_ast_output_labels() {
    use khive_query::ast::{OrderDirection, OrderItem, PropertyRef};

    let conn = fixture();
    for (id, label) in [("node-a", "start"), ("node-b", "middle"), ("node-z", "end")] {
        conn.execute(
            "UPDATE entities SET properties = ?1 WHERE id = ?2",
            rusqlite::params![format!(r#"{{"Label":"{label}"}}"#), id],
        )
        .unwrap();
    }
    for (id, rank) in [("edge-a", 1), ("edge-next", 2), ("edge-z", 3)] {
        conn.execute(
            "UPDATE graph_edges SET metadata = ?1 WHERE id = ?2",
            rusqlite::params![format!(r#"{{"rank":{{"Value":{rank}}}}}"#), id],
        )
        .unwrap();
    }
    for recursive in [false, true] {
        for [start, edge, end] in [
            ["123", "789", "456"],
            [
                "Left, AS \"Source\"",
                "Link AS , \"Edge\"",
                "Right, AS \"Target\"",
            ],
        ] {
            let mut query = with_edge_binding(path_query(recursive));
            for (element, label) in query.pattern.elements.iter_mut().zip([start, edge, end]) {
                match element {
                    PatternElement::Node(node) => node.variable = Some(label.into()),
                    PatternElement::Edge(edge) => edge.variable = Some(label.into()),
                }
            }
            query.return_items = vec![
                ReturnItem::Path(start.into(), PropertyRef::JsonPath(vec!["Label".into()])),
                ReturnItem::Path(end.into(), PropertyRef::JsonPath(vec!["Label".into()])),
                ReturnItem::Path(
                    edge.into(),
                    PropertyRef::MetadataPath(vec!["rank".into(), "Value".into()]),
                ),
            ];
            query.order_by = vec![OrderItem {
                variable: edge.into(),
                property: PropertyRef::MetadataPath(vec!["rank".into(), "Value".into()]),
                direction: OrderDirection::Asc,
            }];
            let mut columns = vec![
                format!("{start}_properties_Label"),
                format!("{end}_properties_Label"),
                format!("{edge}_metadata_rank_Value"),
            ];
            let mut expected = vec![
                vec![text("start"), text("middle"), Value::Integer(1)],
                vec![text("start"), text("end"), Value::Integer(3)],
            ];
            if recursive {
                columns.extend(["_depth".into(), "_total_weight".into()]);
                for row in &mut expected {
                    row.extend([Value::Integer(1), Value::Real(2.0)]);
                }
                expected.insert(
                    1,
                    vec![
                        text("start"),
                        text("end"),
                        Value::Integer(2),
                        Value::Integer(2),
                        Value::Real(3.0),
                    ],
                );
            }
            let columns: Vec<_> = columns.iter().map(String::as_str).collect();
            assert_eq!(
                run(&conn, &query, &columns),
                expected,
                "recursive={recursive}, start={start}"
            );
        }
    }
}

#[test]
fn typed_return_paths_case_distinct_labels_order_by_their_own_values() {
    let conn = fixture();
    for (id, rank) in [("node-a", 100), ("node-b", 30), ("node-z", 10)] {
        conn.execute(
            "UPDATE entities SET properties = ?1 WHERE id = ?2",
            rusqlite::params![format!(r#"{{"rank":{rank}}}"#), id],
        )
        .unwrap();
    }
    for range in ["", "*0..2", "*1..2"] {
        for container in [false, true] {
            let end_projection = if container {
                "A.properties"
            } else {
                "A.properties.rank"
            };
            let projection = if container {
                format!("a.properties, {end_projection}, a.properties.rank")
            } else {
                format!("a.properties.rank, {end_projection}")
            };
            let base =
                format!("MATCH (a {{name: 'Source'}})-[:extends{range}]->(A) RETURN {projection}");
            let mut columns = if container {
                vec!["a_properties", "A_properties", "a_properties_rank"]
            } else {
                vec!["a_properties_rank", "A_properties_rank"]
            };
            let recursive = !range.is_empty();
            if recursive {
                columns.extend(["_depth", "_total_weight"]);
            }
            let row = |rank: i64, depth: i64, weight: f64| {
                let end = if container {
                    text(&format!(r#"{{"rank":{rank}}}"#))
                } else {
                    Value::Integer(rank)
                };
                let mut cells = if container {
                    vec![text(r#"{"rank":100}"#), end, Value::Integer(100)]
                } else {
                    vec![Value::Integer(100), end]
                };
                if recursive {
                    cells.extend([Value::Integer(depth), Value::Real(weight)]);
                }
                cells
            };
            let mut automatic = vec![row(10, 1, 2.0), row(30, 1, 2.0)];
            if recursive {
                automatic.push(row(10, 2, 3.0));
            }
            if range == "*0..2" {
                automatic.insert(0, row(100, 0, 0.0));
            }
            // Fixed paths retain their existing hidden identity order before projected ties.
            if recursive {
                assert_eq!(run(&conn, &gql::parse(&base).unwrap(), &columns), automatic);
            }
            let mut ordered = vec![row(10, 1, 2.0), row(30, 1, 2.0)];
            if recursive {
                ordered.insert(1, row(10, 2, 3.0));
            }
            if range == "*0..2" {
                ordered.push(row(100, 0, 0.0));
            }
            let query = gql::parse(&format!("{base} ORDER BY A.properties.rank")).unwrap();
            assert_eq!(run(&conn, &query, &columns), ordered, "{base}");
        }
    }
}

#[test]
fn typed_return_paths_case_distinct_metric_labels_keep_total_weight_order() {
    let conn = fixture();
    conn.execute_batch(
        "DELETE FROM graph_edges WHERE id = 'edge-z';
         UPDATE graph_edges SET weight = 0.25 WHERE id = 'edge-a';
         UPDATE graph_edges SET weight = 0.5 WHERE id = 'edge-next';
         UPDATE entities SET properties = '{\"label\":\"end\"}' WHERE id = 'node-z';
         INSERT INTO entities VALUES
             ('node-c', 'local', 'concept', NULL, 'Other', NULL, '{}', 25, 26, NULL);
         INSERT INTO graph_edges VALUES
             ('edge-other', 'local', 'node-a', 'node-c', 'extends', 0.75, '{}', 1, 1, NULL),
             ('edge-final', 'local', 'node-c', 'node-z', 'extends', 0.25, '{}', 1, 1, NULL);",
    )
    .unwrap();
    let columns = [
        "_TOTAL_weight",
        "b_properties_label",
        "_depth",
        "_total_weight",
    ];
    let expected = vec![
        vec![
            Value::Real(0.25),
            text("end"),
            Value::Integer(2),
            Value::Real(1.0),
        ],
        vec![
            Value::Real(0.5),
            text("end"),
            Value::Integer(2),
            Value::Real(0.75),
        ],
    ];
    for min in [0, 1] {
        let input = format!("MATCH (a {{name: 'Source'}})-[_TOTAL:extends*{min}..2]->(b {{name: 'Zulu'}}) RETURN _TOTAL.weight, b.properties.label");
        assert_eq!(
            run(&conn, &gql::parse(&input).unwrap(), &columns),
            expected,
            "implicit total weight order must not resolve to the case-distinct edge weight label"
        );
        let page = gql::parse(&format!("{input} SKIP 1 LIMIT 1")).unwrap();
        assert_eq!(run(&conn, &page, &columns), expected[1..]);
    }
}

#[test]
fn grouped_case_distinct_labels_order_by_selected_values() {
    let conn = fixture();
    for (id, rank) in [("node-a", 100), ("node-b", 30), ("node-z", 10)] {
        conn.execute(
            "UPDATE entities SET properties = ?1 WHERE id = ?2",
            rusqlite::params![format!(r#"{{"rank":{rank}}}"#), id],
        )
        .unwrap();
    }
    for range in ["", "*0..2", "*1..2"] {
        for container in [false, true] {
            let returned = if container {
                "a.properties, A.properties, a.properties.rank"
            } else {
                "a.properties.rank, A.properties.rank"
            };
            for grouped in [false, true] {
                let projection = if grouped {
                    format!("{returned}, count(A)")
                } else {
                    format!("DISTINCT {returned}")
                };
                let base = format!(
                    "MATCH (a {{name: 'Source'}})-[:extends{range}]->(A) RETURN {projection}"
                );
                let mut columns = if container {
                    vec!["a_properties", "A_properties", "a_properties_rank"]
                } else {
                    vec!["a_properties_rank", "A_properties_rank"]
                };
                if grouped {
                    columns.push("count_A");
                }
                let make_row = |rank: i64, count: i64| {
                    let end = if container {
                        text(&format!(r#"{{"rank":{rank}}}"#))
                    } else {
                        Value::Integer(rank)
                    };
                    let mut row = if container {
                        vec![text(r#"{"rank":100}"#), end, Value::Integer(100)]
                    } else {
                        vec![Value::Integer(100), end]
                    };
                    if grouped {
                        row.push(Value::Integer(count));
                    }
                    row
                };
                let mut ordered = vec![
                    make_row(10, if range.is_empty() { 1 } else { 2 }),
                    make_row(30, 1),
                ];
                if range == "*0..2" {
                    ordered.push(make_row(100, 1));
                }
                let explicit = gql::parse(&format!("{base} ORDER BY A.properties.rank")).unwrap();
                assert_eq!(run(&conn, &explicit, &columns), ordered, "{base}");
                // Numeric projected values have the same natural tie order; whole JSON text
                // uses its own BINARY value ordering and is tested by explicit derived order.
                if !container {
                    assert_eq!(run(&conn, &gql::parse(&base).unwrap(), &columns), ordered);
                }
                ordered.reverse();
                let desc = gql::parse(&format!("{base} ORDER BY A.properties.rank DESC")).unwrap();
                assert_eq!(run(&conn, &desc, &columns), ordered);
            }
        }
    }
}

#[test]
fn count_public_ast_labels_remain_quoted_and_sparql_defaults_to_bag_results() {
    let conn = fixture();
    for recursive in [false, true] {
        for labels in [
            ["123", "789", "456"],
            [
                "left, AS \"source\"",
                "link AS , \"edge\"",
                "right, AS \"target\"",
            ],
        ] {
            let mut query = with_edge_binding(path_query(recursive));
            assert!(
                !query.distinct,
                "SPARQL constructor must retain ordinary semantics"
            );
            for (element, label) in query.pattern.elements.iter_mut().zip(labels) {
                match element {
                    PatternElement::Node(node) => node.variable = Some(label.into()),
                    PatternElement::Edge(edge) => edge.variable = Some(label.into()),
                }
            }
            query.return_items = labels
                .into_iter()
                .map(|label| ReturnItem::Count(label.into()))
                .collect();
            let owned_columns = labels.map(|label| format!("count_{label}"));
            let columns = owned_columns.iter().map(String::as_str).collect::<Vec<_>>();
            assert_eq!(
                run(&conn, &query, &columns),
                vec![vec![Value::Integer(if recursive { 3 } else { 2 }); 3]]
            );
        }
    }
    conn.execute(
        "UPDATE entities SET name = 'same' WHERE id IN ('node-b', 'node-z')",
        [],
    )
    .unwrap();
    let mut query = path_query(false);
    assert!(!query.distinct);
    query.return_items = vec![ReturnItem::Property("456".into(), "name".into())];
    assert_eq!(
        run(&conn, &query, &["456_name"]),
        vec![vec![text("same")], vec![text("same")]]
    );
    query.distinct = true;
    assert_eq!(run(&conn, &query, &["456_name"]), vec![vec![text("same")]]);
}
