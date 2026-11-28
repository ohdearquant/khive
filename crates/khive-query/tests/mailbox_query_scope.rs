use khive_query::{
    compile, compile_with_mailbox, parse, CompileOptions, CompiledQuery, QueryLanguage,
    QueryMailboxScope, QueryValue,
};
use rusqlite::{params, params_from_iter, types::Value, Connection};

#[test]
fn mixed_segments_filter_every_intermediate_and_preserve_typed_joins() {
    let own_in = r#"{"direction":"inbound","to_actor":"a"}"#;
    let own_out = r#"{"direction":"outbound","from_actor":"a"}"#;
    for variable_first in [true, false] {
        let conn = fixture();
        entity(&conn, "root");
        for case in [
            "own-in",
            "own-out",
            "hidden-in",
            "hidden-out",
            "malformed",
            "deleted-note",
            "foreign-note",
            "deleted-mid",
            "foreign-mid",
            "deleted-first",
            "foreign-first",
            "deleted-second",
            "foreign-second",
            "deleted-last",
            "foreign-last",
        ] {
            let message = format!("message-{case}");
            let mid = format!("mid-{case}");
            let leaf = format!("leaf-{case}");
            let properties = match case {
                "own-out" => own_out,
                "hidden-in" => r#"{"direction":"inbound","to_actor":"b"}"#,
                "hidden-out" => r#"{"direction":"outbound","from_actor":"b"}"#,
                "malformed" => r#"{"direction":"inbound","to_actor":17}"#,
                _ => own_in,
            };
            note(&conn, &message, "message", Some(properties));
            entity(&conn, &mid);
            entity(&conn, &leaf);
            for (position, from, to, relation) in [
                (
                    "first",
                    "root",
                    message.as_str(),
                    if variable_first {
                        "extends"
                    } else {
                        "contains"
                    },
                ),
                ("second", message.as_str(), mid.as_str(), "extends"),
                (
                    "last",
                    mid.as_str(),
                    leaf.as_str(),
                    if variable_first {
                        "contains"
                    } else {
                        "extends"
                    },
                ),
            ] {
                let id = format!("{position}-{case}");
                edge(&conn, &id, from, to);
                conn.execute(
                    "UPDATE graph_edges SET relation = ?1 WHERE id = ?2",
                    params![relation, id],
                )
                .unwrap();
                if case == format!("deleted-{position}") {
                    conn.execute("UPDATE graph_edges SET deleted_at = 1 WHERE id = ?1", [&id])
                        .unwrap();
                }
                if case == format!("foreign-{position}") {
                    conn.execute(
                        "UPDATE graph_edges SET namespace = 'foreign' WHERE id = ?1",
                        [&id],
                    )
                    .unwrap();
                }
            }
            match case {
                "deleted-note" => {
                    conn.execute("UPDATE notes SET deleted_at = 1 WHERE id = ?1", [&message])
                        .unwrap();
                }
                "foreign-note" => {
                    conn.execute(
                        "UPDATE notes SET namespace = 'foreign' WHERE id = ?1",
                        [&message],
                    )
                    .unwrap();
                }
                "deleted-mid" => {
                    conn.execute("UPDATE entities SET deleted_at = 1 WHERE id = ?1", [&mid])
                        .unwrap();
                }
                "foreign-mid" => {
                    conn.execute(
                        "UPDATE entities SET namespace = 'foreign' WHERE id = ?1",
                        [&mid],
                    )
                    .unwrap();
                }
                _ => {}
            }
        }
        let before: i64 = conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap();
        let pattern = if variable_first {
            "MATCH (a:concept {name: 'root'})-[:extends*2..2]->(b:concept)-[:contains]->(c:concept)"
        } else {
            "MATCH (a:concept {name: 'root'})-[:contains]->(b:message)-[:extends*2..2]->(c:concept)"
        };
        let source = format!("{pattern} RETURN c.id, count(c) ORDER BY c.id");
        assert_eq!(
            grouped_rows(
                &conn,
                &compiled(&source, &scope("a", false)),
                &["c_id", "count_c"]
            ),
            vec![
                vec![Value::Text("leaf-own-in".into()), Value::Integer(1)],
                vec![Value::Text("leaf-own-out".into()), Value::Integer(1)],
            ],
            "variable_first={variable_first}"
        );
        let query = parse(QueryLanguage::Gql, &format!("{source} LIMIT 3")).unwrap();
        let page = compile_with_mailbox(&query, &options(2), &scope("a", false)).unwrap();
        assert!(page.truncation_check.is_some());
        assert_eq!(
            grouped_rows(&conn, &page, &["c_id", "count_c"]).len(),
            2,
            "hidden paths cannot become the sentinel"
        );
        assert!(grouped_rows(
            &conn,
            &compiled(&format!("{source} SKIP 2 LIMIT 3"), &scope("a", false)),
            &["c_id", "count_c"]
        )
        .is_empty());
        assert_eq!(
            conn.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            before
        );
    }

    let conn = fixture();
    for id in ["shared", "end"] {
        entity(&conn, id);
        note(&conn, id, "observation", None);
        conn.execute(
            "UPDATE entities SET name = ?1 WHERE id = ?2",
            params![format!("entity {id}"), id],
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET name = ?1 WHERE id = ?2",
            params![format!("note {id}"), id],
        )
        .unwrap();
    }
    edge(&conn, "fixed", "shared", "end");
    conn.execute("UPDATE graph_edges SET relation = 'contains'", [])
        .unwrap();
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    for (start_kind, end_kind, expected) in [
        ("entity", "entity", 1),
        ("note", "note", 1),
        ("entity", "note", 0),
        ("note", "entity", 0),
    ] {
        for pattern in [
            format!("MATCH (a:{start_kind} {{name: '{start_kind} shared'}})-[e:extends*0..0]->(b:{end_kind})-[f:contains]->(c:entity)"),
            format!("MATCH (a:entity {{name: 'entity shared'}})-[f:contains]->(b:{start_kind})-[e:extends*0..0]->(c:{end_kind})"),
        ] {
            let query = compiled(&format!("{pattern} RETURN count(a), count(e), count(f)"), &scope("a", false));
            assert_eq!(grouped_rows(&conn, &query, &["count_a", "count_e", "count_f"]), vec![vec![Value::Integer(expected), Value::Integer(0), Value::Integer(expected)]], "{pattern}");
        }
    }
    let unconstrained_start = compiled(
        "MATCH (a)-[e:extends*0..0]->(b)-[f:contains]->(c:entity) WHERE a.id = 'shared' RETURN count(a), count(b), count(c), count(e), count(f)",
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(
            &conn,
            &unconstrained_start,
            &["count_a", "count_b", "count_c", "count_e", "count_f"]
        ),
        vec![vec![
            Value::Integer(2),
            Value::Integer(2),
            Value::Integer(2),
            Value::Integer(0),
            Value::Integer(2)
        ]],
        "each typed zero seed joins exactly one of the UUID-equal start bindings"
    );
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn mixed_not_exists_correlates_intermediate_nodes_before_counts() {
    let conn = fixture();
    entity(&conn, "root");
    let cases = [
        "00-middle",
        "01-final",
        "02-hidden",
        "03-deleted",
        "04-foreign",
        "05-malformed",
        "06-dead-edge",
        "07-foreign-edge",
        "08-absent",
    ];
    for case in cases {
        let mid = format!("mid-{case}");
        let leaf = format!("leaf-{case}");
        let support = format!("support-{case}");
        entity(&conn, &mid);
        entity(&conn, &leaf);
        note(&conn, &mid, "observation", None);
        edge(&conn, &format!("prefix-{case}"), "root", &mid);
        conn.execute(
            "UPDATE graph_edges SET relation = 'contains' WHERE id = ?1",
            [format!("prefix-{case}")],
        )
        .unwrap();
        edge(&conn, &format!("suffix-{case}"), &mid, &leaf);
        if case == "08-absent" {
            continue;
        }
        let properties = match case {
            "02-hidden" => r#"{"direction":"outbound","from_actor":"b"}"#,
            "05-malformed" => r#"{"direction":"inbound","to_actor":[]}"#,
            _ => r#"{"direction":"inbound","to_actor":"a"}"#,
        };
        note(&conn, &support, "message", Some(properties));
        absence_support(
            &conn,
            &format!("support-edge-{case}"),
            &support,
            if case == "01-final" { &leaf } else { &mid },
        );
        match case {
            "03-deleted" => {
                conn.execute("UPDATE notes SET deleted_at = 1 WHERE id = ?1", [&support])
                    .unwrap();
            }
            "04-foreign" => {
                conn.execute(
                    "UPDATE notes SET namespace = 'foreign' WHERE id = ?1",
                    [&support],
                )
                .unwrap();
            }
            "06-dead-edge" => {
                conn.execute(
                    "UPDATE graph_edges SET deleted_at = 1 WHERE id = ?1",
                    [format!("support-edge-{case}")],
                )
                .unwrap();
            }
            "07-foreign-edge" => {
                conn.execute(
                    "UPDATE graph_edges SET namespace = 'foreign' WHERE id = ?1",
                    [format!("support-edge-{case}")],
                )
                .unwrap();
            }
            _ => {}
        }
    }
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    let pattern =
        "MATCH (a:entity {name: 'root'})-[:contains]->(b:entity)-[:extends*1..2]->(c:entity)";
    let predicate = "WHERE NOT EXISTS { (f:message)-[:supports]->(b:entity) } AND NOT EXISTS { (g:message)-[:supports]->(c:entity) }";
    let source = format!("{pattern} {predicate} RETURN c.id, count(b) ORDER BY c.id");
    let expected: Vec<_> = [
        "leaf-02-hidden",
        "leaf-03-deleted",
        "leaf-04-foreign",
        "leaf-05-malformed",
        "leaf-06-dead-edge",
        "leaf-07-foreign-edge",
        "leaf-08-absent",
    ]
    .into_iter()
    .map(|id| vec![Value::Text(id.into()), Value::Integer(1)])
    .collect();
    assert_eq!(
        grouped_rows(
            &conn,
            &compiled(&source, &scope("a", false)),
            &["c_id", "count_b"]
        ),
        expected
    );
    let count = compiled(
        &format!("{pattern} {predicate} RETURN count(b), count(c)"),
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(&conn, &count, &["count_b", "count_c"]),
        vec![vec![Value::Integer(7), Value::Integer(7)]]
    );
    let query = parse(QueryLanguage::Gql, &format!("{source} SKIP 2 LIMIT 3")).unwrap();
    let page = compile_with_mailbox(&query, &options(2), &scope("a", false)).unwrap();
    assert!(page.truncation_check.is_some());
    assert_eq!(
        grouped_rows(&conn, &page, &["c_id", "count_b"]),
        expected[2..5]
    );
    let distinct = compiled(
        &format!("{pattern} {predicate} RETURN DISTINCT c.id ORDER BY c.id SKIP 6 LIMIT 2"),
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(&conn, &distinct, &["c_id"]),
        vec![vec![Value::Text("leaf-08-absent".into())]]
    );

    let wrong_substrate = compiled(
        &format!(
            "{pattern} WHERE NOT EXISTS {{ (f:message)-[:supports]->(b:note) }} RETURN count(b)"
        ),
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(&conn, &wrong_substrate, &["count_b"]),
        vec![vec![Value::Integer(9)]],
        "a UUID-equal note cannot satisfy an entity correlation"
    );
    let constrained = compiled(&format!("{pattern} WHERE NOT EXISTS {{ (f:message)-[:supports]->(b {{name: 'mid-01-final'}}) }} RETURN count(b)"), &scope("a", false));
    assert_eq!(
        grouped_rows(&conn, &constrained, &["count_b"]),
        vec![vec![Value::Integer(9)]],
        "inner occurrence constraints restrict the actual middle binding"
    );
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE entities (
            id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
            entity_type TEXT, name TEXT, description TEXT, properties TEXT,
            created_at INTEGER, updated_at INTEGER, deleted_at INTEGER
        );
        CREATE TABLE notes (
            id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
            name TEXT, content TEXT, status TEXT, salience REAL, decay_factor REAL,
            properties TEXT, created_at INTEGER, updated_at INTEGER, deleted_at INTEGER
        );
        CREATE TABLE graph_edges (
            id TEXT PRIMARY KEY, namespace TEXT NOT NULL, source_id TEXT NOT NULL,
            target_id TEXT NOT NULL, relation TEXT, weight REAL, metadata TEXT,
            created_at INTEGER, updated_at INTEGER, deleted_at INTEGER
        );
        CREATE TABLE events (
            id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT, verb TEXT,
            substrate TEXT, actor TEXT, outcome TEXT, data TEXT, duration_us INTEGER,
            target_id TEXT, payload TEXT, created_at INTEGER
        );
        CREATE TABLE event_observations (
            event_id TEXT, entity_id TEXT, referent_kind TEXT, role TEXT, position INTEGER,
            PRIMARY KEY (event_id, role, position)
        );",
    )
    .unwrap();
    conn
}

fn note(conn: &Connection, id: &str, kind: &str, properties: Option<&str>) {
    conn.execute(
        "INSERT INTO notes VALUES (?1, 'local', ?2, ?1, ?1, 'active', 1, 1, ?3, 0, 0, NULL)",
        params![id, kind, properties],
    )
    .unwrap();
}

fn entity(conn: &Connection, id: &str) {
    conn.execute(
        "INSERT INTO entities VALUES (?1, 'local', 'concept', NULL, ?1, NULL, '{}', 0, 0, NULL)",
        [id],
    )
    .unwrap();
}

fn edge(conn: &Connection, id: &str, from: &str, to: &str) {
    conn.execute(
        "INSERT INTO graph_edges VALUES (?1, 'local', ?2, ?3, 'extends', 1, '{}', 0, 0, NULL)",
        params![id, from, to],
    )
    .unwrap();
}

fn event(conn: &Connection, id: &str) {
    conn.execute(
        "INSERT INTO events (id, namespace, kind, verb, payload, created_at)
         VALUES (?1, 'local', 'search_executed', 'search', '{}', 0)",
        [id],
    )
    .unwrap();
}

fn scope(actor: &str, legacy_local: bool) -> QueryMailboxScope {
    QueryMailboxScope {
        actor_id: actor.to_owned(),
        legacy_local,
        maintainer_actor: None,
    }
}

fn options(max_limit: usize) -> CompileOptions {
    CompileOptions {
        scopes: vec!["local".to_owned()],
        max_limit,
    }
}

fn compiled(source: &str, mailbox: &QueryMailboxScope) -> CompiledQuery {
    let query = parse(QueryLanguage::Gql, source).unwrap();
    compile_with_mailbox(&query, &options(100), mailbox).unwrap()
}

fn run(conn: &Connection, query: &CompiledQuery) -> Vec<String> {
    let values = query.params.iter().map(|value| match value {
        QueryValue::Null => Value::Null,
        QueryValue::Integer(value) => Value::Integer(*value),
        QueryValue::Float(value) => Value::Real(*value),
        QueryValue::Text(value) => Value::Text(value.clone()),
        QueryValue::Blob(value) => Value::Blob(value.clone()),
    });
    let mut stmt = conn.prepare(&query.sql).unwrap();
    assert_eq!(stmt.parameter_count(), query.params.len(), "{}", query.sql);
    stmt.query_map(params_from_iter(values), |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn caller_partition_filters_granular_bare_and_predicate_note_queries() {
    let conn = fixture();
    for (id, properties) in [
        ("a-in", r#"{"direction":"inbound","to_actor":"a"}"#),
        ("a-out", r#"{"direction":"outbound","from_actor":"a"}"#),
        ("b-in", r#"{"direction":"inbound","to_actor":"b"}"#),
        ("b-out", r#"{"direction":"outbound","from_actor":"b"}"#),
        (
            "quarantine",
            r#"{"direction":"inbound","to_actor":"maintainer"}"#,
        ),
    ] {
        note(&conn, id, "message", Some(properties));
    }
    note(&conn, "ordinary", "observation", None);
    note(
        &conn,
        "foreign",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"a"}"#),
    );
    conn.execute(
        "UPDATE notes SET namespace = 'foreign' WHERE id = 'foreign'",
        [],
    )
    .unwrap();
    for source in [
        "MATCH (n:message) RETURN n.id",
        "MATCH (n:note) WHERE n.kind = 'message' RETURN n.id",
    ] {
        assert_eq!(
            run(&conn, &compiled(source, &scope("a", false))),
            ["a-in", "a-out"]
        );
        assert_eq!(
            run(&conn, &compiled(source, &scope("b", false))),
            ["b-in", "b-out"]
        );
    }
    assert_eq!(
        run(
            &conn,
            &compiled("MATCH (n:note) RETURN n.id", &scope("a", false))
        ),
        ["a-in", "a-out", "ordinary"]
    );
    let query = parse(QueryLanguage::Gql, "MATCH (n:message) RETURN n.id").unwrap();
    assert_eq!(
        run(&conn, &compile(&query, &options(100)).unwrap()),
        ["a-in", "a-out", "b-in", "b-out", "quarantine"]
    );
}

#[test]
fn legacy_partition_preserves_absent_null_and_nontext_distinctions() {
    let conn = fixture();
    let rows = [
        ("00-sql-null", None, true, false),
        ("01-json-null", Some("null"), true, false),
        ("02-missing", Some("{}"), true, false),
        (
            "03-null",
            Some(r#"{"direction":null,"to_actor":null,"from_actor":null}"#),
            true,
            false,
        ),
        (
            "04-in-missing",
            Some(r#"{"direction":"inbound"}"#),
            true,
            false,
        ),
        (
            "05-in-null",
            Some(r#"{"direction":"inbound","to_actor":null}"#),
            true,
            false,
        ),
        (
            "06-out-missing",
            Some(r#"{"direction":"outbound"}"#),
            true,
            false,
        ),
        (
            "07-out-null",
            Some(r#"{"direction":"outbound","from_actor":null}"#),
            true,
            false,
        ),
        (
            "08-nontext-unrouted",
            Some(r#"{"from_actor":17,"to_actor":[]}"#),
            true,
            false,
        ),
        (
            "09-local-in",
            Some(r#"{"direction":"inbound","to_actor":"local"}"#),
            true,
            true,
        ),
        (
            "10-local-out",
            Some(r#"{"direction":"outbound","from_actor":"local"}"#),
            true,
            true,
        ),
        (
            "11-empty-actor",
            Some(r#"{"direction":"inbound","to_actor":""}"#),
            false,
            false,
        ),
        (
            "12-wrong-actor",
            Some(r#"{"direction":"outbound","from_actor":"a"}"#),
            false,
            false,
        ),
        (
            "13-number-actor",
            Some(r#"{"direction":"inbound","to_actor":17}"#),
            false,
            false,
        ),
        (
            "14-bool-actor",
            Some(r#"{"direction":"outbound","from_actor":true}"#),
            false,
            false,
        ),
        (
            "15-array-actor",
            Some(r#"{"direction":"inbound","to_actor":["local"]}"#),
            false,
            false,
        ),
        (
            "16-object-actor",
            Some(r#"{"direction":"outbound","from_actor":{"x":1}}"#),
            false,
            false,
        ),
        (
            "17-text-unrouted",
            Some(r#"{"to_actor":"local"}"#),
            false,
            false,
        ),
        (
            "18-empty-direction",
            Some(r#"{"direction":""}"#),
            false,
            false,
        ),
        (
            "19-number-direction",
            Some(r#"{"direction":17}"#),
            false,
            false,
        ),
        (
            "20-bool-direction",
            Some(r#"{"direction":true}"#),
            false,
            false,
        ),
        (
            "21-array-direction",
            Some(r#"{"direction":[]}"#),
            false,
            false,
        ),
        (
            "22-object-direction",
            Some(r#"{"direction":{}}"#),
            false,
            false,
        ),
    ];
    for (id, properties, _, _) in rows {
        note(&conn, id, "message", properties);
    }
    for (legacy, expected) in [
        (
            true,
            rows.iter()
                .filter(|row| row.2)
                .map(|row| row.0)
                .collect::<Vec<_>>(),
        ),
        (
            false,
            rows.iter()
                .filter(|row| row.3)
                .map(|row| row.0)
                .collect::<Vec<_>>(),
        ),
    ] {
        assert_eq!(
            run(
                &conn,
                &compiled("MATCH (n:message) RETURN n.id", &scope("local", legacy))
            ),
            expected
        );
    }
}

#[test]
fn actors_are_bound_text_and_json_containers_cannot_impersonate_them() {
    let conn = fixture();
    for (id, properties) in [
        ("array", r#"{"direction":"inbound","to_actor":["a"]}"#),
        (
            "array-text",
            r#"{"direction":"inbound","to_actor":"[\"a\"]"}"#,
        ),
        ("object", r#"{"direction":"outbound","from_actor":{"x":1}}"#),
        (
            "object-text",
            r#"{"direction":"outbound","from_actor":"{\"x\":1}"}"#,
        ),
        (
            "quoted",
            r#"{"direction":"inbound","to_actor":"x' OR 1=1 --"}"#,
        ),
    ] {
        note(&conn, id, "message", Some(properties));
    }
    for (actor, expected) in [
        (r#"["a"]"#, "array-text"),
        (r#"{"x":1}"#, "object-text"),
        ("x' OR 1=1 --", "quoted"),
    ] {
        let result = compiled("MATCH (n:message) RETURN n.id", &scope(actor, false));
        assert!(!result.sql.contains(actor));
        assert_eq!(
            result
                .params
                .iter()
                .filter(|value| matches!(value, QueryValue::Text(text) if text == actor))
                .count(),
            1
        );
        assert_eq!(run(&conn, &result), [expected]);
    }
}

#[test]
fn synthetic_targets_filter_notes_without_changing_typed_entity_referents() {
    let conn = fixture();
    note(
        &conn,
        "hidden",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"b"}"#),
    );
    note(
        &conn,
        "visible",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"a"}"#),
    );
    entity(&conn, "hidden");
    conn.execute("INSERT INTO events (id, namespace, kind, verb, payload, created_at) VALUES ('event', 'local', 'search_executed', 'search', '{}', 0)", []).unwrap();
    for role in ["candidate", "selected", "target", "signal"] {
        for (position, id, kind) in [
            (0, "hidden", "note"),
            (1, "visible", "note"),
            (2, "hidden", "entity"),
        ] {
            conn.execute(
                "INSERT INTO event_observations VALUES ('event', ?1, ?2, ?3, ?4)",
                params![id, kind, role, position],
            )
            .unwrap();
        }
        let result = compiled(
            &format!("MATCH (ev)-[:observed_as_{role}]->(n) RETURN n.id"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &result), ["visible", "hidden"]);
        let notes = compiled(
            &format!("MATCH (ev)-[:observed_as_{role}]->(n:message) RETURN n.id"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &notes), ["visible"]);
    }
}

#[test]
fn fixed_paging_counts_only_visible_rows_before_offset_and_limit() {
    let conn = fixture();
    for index in 0..8 {
        let properties = if index % 2 == 0 {
            r#"{"direction":"inbound","to_actor":"b"}"#
        } else {
            r#"{"direction":"inbound","to_actor":"a"}"#
        };
        note(&conn, &format!("row-{index}"), "message", Some(properties));
    }
    for (offset, limit, expected, sentinel) in [
        (0, 3, vec!["row-1", "row-3", "row-5"], true),
        (2, 3, vec!["row-5", "row-7"], true),
        (3, 3, vec!["row-7"], true),
        (4, 3, vec![], true),
        (1, 2, vec!["row-3", "row-5"], false),
        (3, 1, vec!["row-7"], false),
        (1, 0, vec![], false),
    ] {
        let query = parse(
            QueryLanguage::Gql,
            &format!("MATCH (n:message) RETURN n.id SKIP {offset} LIMIT {limit}"),
        )
        .unwrap();
        let result = compile_with_mailbox(&query, &options(2), &scope("a", false)).unwrap();
        assert_eq!(run(&conn, &result), expected);
        assert_eq!(result.truncation_check.is_some(), sentinel);
    }
    let query = parse(QueryLanguage::Gql, "MATCH (n:message) RETURN n.id SKIP 4").unwrap();
    assert!(run(
        &conn,
        &compile_with_mailbox(&query, &options(2), &scope("a", false)).unwrap()
    )
    .is_empty());
}

#[test]
fn edge_nodes_use_entity_precedence_and_screen_both_hidden_endpoints() {
    let conn = fixture();
    entity(&conn, "root");
    for id in [
        "live-entity",
        "dead-entity",
        "foreign-entity",
        "hidden-only",
        "dead-note",
    ] {
        note(
            &conn,
            id,
            "message",
            Some(r#"{"direction":"inbound","to_actor":"b"}"#),
        );
    }
    for id in ["live-entity", "dead-entity", "foreign-entity"] {
        entity(&conn, id);
    }
    conn.execute(
        "UPDATE entities SET deleted_at = 1 WHERE id = 'dead-entity'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE entities SET namespace = 'foreign' WHERE id = 'foreign-entity'",
        [],
    )
    .unwrap();
    conn.execute("UPDATE notes SET deleted_at = 1 WHERE id = 'dead-note'", [])
        .unwrap();
    note(
        &conn,
        "visible",
        "message",
        Some(r#"{"direction":"outbound","from_actor":"a"}"#),
    );
    note(&conn, "null-routing", "message", None);
    note(
        &conn,
        "missing-routing",
        "message",
        Some(r#"{"direction":"inbound"}"#),
    );
    note(&conn, "ordinary", "observation", None);
    for (id, target) in [
        ("allow-dead", "dead-entity"),
        ("allow-foreign", "foreign-entity"),
        ("allow-live", "live-entity"),
        ("allow-missing", "missing-id"),
        ("allow-note", "visible"),
        ("allow-ordinary", "ordinary"),
        ("deny-dead-note", "dead-note"),
        ("deny-hidden", "hidden-only"),
        ("deny-missing-route", "missing-routing"),
        ("deny-null-route", "null-routing"),
    ] {
        edge(&conn, id, "root", target);
    }
    edge(&conn, "deny-hidden-source", "hidden-only", "root");
    edge(&conn, "allow-visible-source", "visible", "root");
    assert_eq!(
        run(
            &conn,
            &compiled("MATCH (e:edge) RETURN e.id", &scope("a", false))
        ),
        [
            "allow-dead",
            "allow-foreign",
            "allow-live",
            "allow-missing",
            "allow-note",
            "allow-ordinary",
            "allow-visible-source",
        ]
    );
    assert_eq!(
        run(
            &conn,
            &compiled("MATCH (n:message) RETURN n.id", &scope("a", false))
        ),
        ["visible"]
    );
}

#[test]
fn canonical_joins_screen_hidden_note_references_even_when_an_event_shares_the_id() {
    let conn = fixture();
    entity(&conn, "root");
    entity(&conn, "visible");
    event(&conn, "hidden");
    note(&conn, "hidden", "message", None);
    edge(&conn, "bad-out", "root", "hidden");
    edge(&conn, "bad-in", "hidden", "root");
    edge(&conn, "good", "root", "visible");
    for (pattern, expected) in [
        ("(a)-[:extends]->(b)", vec!["visible"]),
        ("(a)<-[:extends]-(b)", vec![]),
        ("(a)-[:extends]-(b)", vec!["visible"]),
    ] {
        let result = compiled(
            &format!("MATCH {pattern} WHERE a.id = 'root' RETURN b.id"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &result), expected, "{pattern}");
    }
    let query = parse(
        QueryLanguage::Sparql,
        "SELECT ?b WHERE { ?a :name \"root\" . ?a :extends ?b . }",
    )
    .unwrap();
    let result = compile_with_mailbox(&query, &options(100), &scope("a", false)).unwrap();
    assert_eq!(run(&conn, &result), ["visible"]);
}

#[test]
fn recursive_seed_step_and_intermediate_message_nodes_are_filtered() {
    let conn = fixture();
    for id in ["root", "middle", "end", "hidden-path-end"] {
        entity(&conn, id);
    }
    for id in ["hidden-seed", "hidden-step", "hidden-intermediate"] {
        note(&conn, id, "message", None);
    }
    // Event aliases keep the primary-node source nonempty, independently testing
    // the seed and recursive edge sources rather than only note-row filtering.
    event(&conn, "hidden-seed");
    event(&conn, "hidden-step");
    // Tombstoned entities admit the edges, but only hidden live note rows could
    // satisfy the recursive node joins if their mailbox filter were removed.
    for id in ["hidden-seed-collision", "hidden-step-collision"] {
        entity(&conn, id);
        conn.execute("UPDATE entities SET deleted_at = 1 WHERE id = ?1", [id])
            .unwrap();
        note(
            &conn,
            id,
            "message",
            Some(r#"{"direction":"inbound","to_actor":"b"}"#),
        );
    }
    edge(&conn, "seed-bad", "root", "hidden-seed");
    edge(&conn, "seed-good", "root", "middle");
    edge(&conn, "seed-node-bad", "root", "hidden-seed-collision");
    edge(&conn, "step-bad", "middle", "hidden-step");
    edge(&conn, "step-good", "middle", "end");
    edge(&conn, "step-node-bad", "middle", "hidden-step-collision");
    edge(&conn, "intermediate-in", "root", "hidden-intermediate");
    edge(
        &conn,
        "intermediate-out",
        "hidden-intermediate",
        "hidden-path-end",
    );
    assert_eq!(
        run(
            &conn,
            &compiled("MATCH (e:edge) RETURN e.id", &scope("a", false))
        ),
        ["seed-good", "seed-node-bad", "step-good", "step-node-bad"]
    );
    for (depth, expected) in [(1, vec!["middle"]), (2, vec!["end"])] {
        let result = compiled(
            &format!("MATCH (a)-[:extends*{depth}..{depth}]->(b) WHERE a.id = 'root' RETURN b.id"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &result), expected);
    }
    // A one-hop maximum uses fixed-length compilation; this range exercises
    // the recursive seed and step node joins together.
    let result = compiled(
        "MATCH (a)-[:extends*1..2]->(b) WHERE a.id = 'root' RETURN b.id",
        &scope("a", false),
    );
    assert_eq!(run(&conn, &result), ["middle", "end"]);
    let query = parse(
        QueryLanguage::Sparql,
        "SELECT ?b WHERE { ?a :name \"root\" . ?a :extends+ ?b . }",
    )
    .unwrap();
    let result = compile_with_mailbox(&query, &options(100), &scope("a", false)).unwrap();
    assert_eq!(run(&conn, &result), ["middle", "end"]);
}

#[test]
fn recursive_pages_remain_distinct_after_visibility_filtering() {
    let conn = fixture();
    for id in ["root", "left", "right", "end"] {
        entity(&conn, id);
    }
    note(&conn, "hidden", "message", None);
    event(&conn, "hidden");
    for (id, from, to) in [
        ("a", "root", "left"),
        ("b", "root", "right"),
        ("c", "left", "end"),
        ("d", "right", "end"),
        ("e", "root", "hidden"),
    ] {
        edge(&conn, id, from, to);
    }
    for (offset, expected) in [
        (0, vec!["left", "right"]),
        (1, vec!["right", "end"]),
        (2, vec!["end"]),
        (3, vec![]),
    ] {
        let query = parse(
            QueryLanguage::Gql,
            &format!(
                "MATCH (a)-[:extends*1..2]->(b) WHERE a.id = 'root' RETURN b.id SKIP {offset}"
            ),
        )
        .unwrap();
        let result = compile_with_mailbox(&query, &options(1), &scope("a", false)).unwrap();
        assert_eq!(run(&conn, &result), expected);
    }
    for (offset, limit, expected, sentinel) in [
        (1, 2, vec!["right", "end"], true),
        (2, 2, vec!["end"], true),
        (2, 1, vec!["end"], false),
        (2, 0, vec![], false),
    ] {
        let query = parse(
            QueryLanguage::Gql,
            &format!(
                "MATCH (a)-[:extends*1..2]->(b) WHERE a.id = 'root' RETURN b.id SKIP {offset} LIMIT {limit}"
            ),
        )
        .unwrap();
        let result = compile_with_mailbox(&query, &options(1), &scope("a", false)).unwrap();
        assert_eq!(run(&conn, &result), expected);
        assert_eq!(result.truncation_check.is_some(), sentinel);
    }
}

#[test]
fn zero_hop_preserves_typed_start_and_end_identity() {
    let conn = fixture();
    entity(&conn, "shared");
    note(&conn, "shared", "observation", None);
    conn.execute(
        "UPDATE entities SET name = 'entity label' WHERE id = 'shared'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET name = 'note label' WHERE id = 'shared'",
        [],
    )
    .unwrap();
    entity(&conn, "visible-twin");
    note(
        &conn,
        "visible-twin",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"other"}"#),
    );
    for max in [0, 1] {
        for (start_kind, end_kind, expected) in [
            ("entity", "entity", vec!["entity label"]),
            ("note", "note", vec!["note label"]),
            ("entity", "note", vec![]),
            ("note", "entity", vec![]),
        ] {
            let result = compiled(&format!(
                "MATCH (a:{start_kind})-[:extends*0..{max}]->(b:{end_kind}) WHERE a.id = 'shared' RETURN a.name, b.name"
            ), &scope("a", false));
            assert_eq!(
                run(&conn, &result),
                expected,
                "{start_kind} -> {end_kind}, 0..{max}"
            );
        }
        let result = compiled(
            &format!(
                "MATCH (a)-[:extends*0..{max}]->(b) WHERE a.id = 'shared' RETURN a.name, b.name"
            ),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &result), ["entity label", "note label"]);
        let cross_names = compiled(&format!(
            "MATCH (a)-[:extends*0..{max}]->(b) WHERE a.name = 'entity label' AND b.name = 'note label' RETURN a.name"
        ), &scope("a", false));
        assert!(run(&conn, &cross_names).is_empty());
        let twin = compiled(
            &format!(
            "MATCH (a)-[:extends*0..{max}]->(b) WHERE a.id = 'visible-twin' RETURN a.name, b.name"
        ),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &twin), ["visible-twin"]);
    }
}

#[test]
fn zero_hop_respects_mailbox_and_intermediate_visibility() {
    let conn = fixture();
    for (id, properties) in [
        ("own-in", Some(r#"{"direction":"inbound","to_actor":"a"}"#)),
        (
            "own-out",
            Some(r#"{"direction":"outbound","from_actor":"a"}"#),
        ),
        (
            "local-in",
            Some(r#"{"direction":"inbound","to_actor":"local"}"#),
        ),
        (
            "foreign-actor",
            Some(r#"{"direction":"inbound","to_actor":"b"}"#),
        ),
        (
            "malformed",
            Some(r#"{"direction":"inbound","to_actor":["a"]}"#),
        ),
        ("deleted", Some(r#"{"direction":"inbound","to_actor":"a"}"#)),
        (
            "foreign-namespace",
            Some(r#"{"direction":"inbound","to_actor":"a"}"#),
        ),
        ("legacy", None),
    ] {
        note(&conn, id, "message", properties);
    }
    conn.execute("UPDATE notes SET deleted_at = 1 WHERE id = 'deleted'", [])
        .unwrap();
    conn.execute(
        "UPDATE notes SET namespace = 'other' WHERE id = 'foreign-namespace'",
        [],
    )
    .unwrap();
    for max in [0, 1] {
        for (actor, legacy_local, expected) in [
            ("a", false, vec!["own-in", "own-out"]),
            ("local", false, vec!["local-in"]),
            ("local", true, vec!["legacy", "local-in"]),
        ] {
            let result = compiled(
                &format!("MATCH (a:message)-[:extends*0..{max}]->(b:message) RETURN b.id"),
                &scope(actor, legacy_local),
            );
            assert_eq!(
                run(&conn, &result),
                expected,
                "{actor}, legacy={legacy_local}, 0..{max}"
            );
        }
    }

    entity(&conn, "root");
    for id in [
        "own-in",
        "own-out",
        "foreign-actor",
        "malformed",
        "deleted",
        "foreign-namespace",
    ] {
        let end = format!("end-{id}");
        entity(&conn, &end);
        edge(&conn, &format!("root-{id}"), "root", id);
        edge(&conn, &format!("leaf-{id}"), id, &end);
    }
    let result = compiled(
        "MATCH (a:concept)-[:extends*0..2]->(b) WHERE a.id = 'root' RETURN b.id",
        &scope("a", false),
    );
    assert_eq!(
        run(&conn, &result),
        ["root", "own-in", "own-out", "end-own-in", "end-own-out"]
    );
}

#[test]
fn explicit_sparql_zero_ranges_match_gql_results() {
    let conn = fixture();
    for id in ["root", "child", "end"] {
        entity(&conn, id);
    }
    edge(&conn, "first", "root", "child");
    edge(&conn, "second", "child", "end");
    for (max, expected) in [
        (0, vec!["root"]),
        (1, vec!["root", "child"]),
        (2, vec!["root", "child", "end"]),
    ] {
        let query = parse(
            QueryLanguage::Sparql,
            &format!("SELECT ?b WHERE {{ ?a :name \"root\" . ?a :extends{{0,{max}}} ?b . }}"),
        )
        .unwrap();
        let result = compile_with_mailbox(&query, &options(100), &scope("a", false)).unwrap();
        assert_eq!(run(&conn, &result), expected);
        let gql_result = compiled(
            &format!("MATCH (a)-[:extends*0..{max}]->(b) WHERE a.id = 'root' RETURN b.id"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &gql_result), expected);
    }
    assert!(matches!(
        parse(
            QueryLanguage::Sparql,
            "SELECT ?b WHERE { ?a :extends* ?b . }"
        ),
        Err(khive_query::QueryError::Unsupported(_))
    ));
}

#[test]
fn zero_hop_first_expansion_preserves_self_loop_and_later_cycle_guard() {
    let conn = fixture();
    entity(&conn, "root");
    entity(&conn, "child");
    // The compiler must still read an existing self-loop; runtime writes refuse new ones.
    edge(&conn, "self", "root", "root");
    edge(&conn, "forward", "root", "child");
    edge(&conn, "back", "child", "root");
    for (range, expected) in [
        ("0..0", vec!["root"]),
        ("0..1", vec!["root", "child", "root"]),
        ("0..3", vec!["root", "child", "root", "child"]),
        ("1..3", vec!["child", "root", "child"]),
    ] {
        let result = compiled(
            &format!("MATCH (a)-[:extends*{range}]->(b) WHERE a.id = 'root' RETURN b.id"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &result), expected, "{range}");
    }
}

fn grouped_rows(conn: &Connection, query: &CompiledQuery, columns: &[&str]) -> Vec<Vec<Value>> {
    let values = query.params.iter().map(|value| match value {
        QueryValue::Null => Value::Null,
        QueryValue::Integer(value) => Value::Integer(*value),
        QueryValue::Float(value) => Value::Real(*value),
        QueryValue::Text(value) => Value::Text(value.clone()),
        QueryValue::Blob(value) => Value::Blob(value.clone()),
    });
    let mut stmt = conn
        .prepare(&query.sql)
        .unwrap_or_else(|error| panic!("{error}: {}", query.sql));
    assert_eq!(stmt.parameter_count(), query.params.len());
    assert_eq!(stmt.column_names().collect::<Vec<_>>(), columns);
    let width = stmt.column_count();
    let rows = stmt
        .query_map(params_from_iter(values), |row| {
            (0..width)
                .map(|index| row.get::<_, Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows
}

#[test]
fn recursive_counts_preserve_typed_start_and_end_joins() {
    let conn = fixture();
    entity(&conn, "root");
    entity(&conn, "shared");
    note(&conn, "shared", "observation", None);
    entity(&conn, "leaf");
    conn.execute(
        "UPDATE entities SET name = 'entity start' WHERE id = 'shared'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET name = 'note start' WHERE id = 'shared'",
        [],
    )
    .unwrap();
    edge(&conn, "first", "root", "shared");
    edge(&conn, "second", "shared", "leaf");
    for min in [0, 1] {
        for (kind, expected) in [(":entity", 1), (":note", 1), ("", 2)] {
            let query = compiled(&format!("MATCH (a:entity)-[e:extends*{min}..2]->(b{kind}) WHERE a.id = 'root' AND b.id = 'shared' RETURN count(a), count(b), count(e)"), &scope("a", false));
            assert_eq!(
                grouped_rows(&conn, &query, &["count_a", "count_b", "count_e"]),
                vec![vec![Value::Integer(expected); 3]],
                "endpoint {kind}, minimum {min}"
            );
        }
        let query = compiled(&format!("MATCH (a)-[e:extends*{min}..2]->(b:entity) WHERE a.id = 'shared' AND b.id = 'leaf' RETURN a.name, count(a), count(b) ORDER BY a.name"), &scope("a", false));
        assert_eq!(
            grouped_rows(&conn, &query, &["a_name", "count_a", "count_b"]),
            vec![
                vec![
                    Value::Text("entity start".into()),
                    Value::Integer(1),
                    Value::Integer(1)
                ],
                vec![
                    Value::Text("note start".into()),
                    Value::Integer(1),
                    Value::Integer(1)
                ],
            ],
            "start joins must preserve substrate at minimum {min}"
        );
    }
}

#[test]
fn synthetic_counts_use_observation_positions_not_null_public_edge_ids() {
    let conn = fixture();
    note(
        &conn,
        "target",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"a"}"#),
    );
    entity(&conn, "target");
    note(
        &conn,
        "hidden",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"b"}"#),
    );
    event(&conn, "event");
    note(
        &conn,
        "deleted-target",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"a"}"#),
    );
    conn.execute(
        "UPDATE notes SET deleted_at = 1 WHERE id = 'deleted-target'",
        [],
    )
    .unwrap();
    event(&conn, "foreign-event");
    conn.execute(
        "UPDATE events SET namespace = 'foreign' WHERE id = 'foreign-event'",
        [],
    )
    .unwrap();
    for (event_id, id, kind, role, position) in [
        ("event", "target", "note", "selected", 0),
        ("event", "target", "note", "selected", 1),
        ("event", "target", "entity", "selected", 2),
        ("event", "hidden", "note", "selected", 3),
        ("event", "deleted-target", "note", "selected", 4),
        ("event", "target", "note", "candidate", 0),
        ("foreign-event", "target", "note", "selected", 0),
    ] {
        conn.execute(
            "INSERT INTO event_observations VALUES (?1, ?2, ?3, ?4, ?5)",
            params![event_id, id, kind, role, position],
        )
        .unwrap();
    }
    let base = "MATCH (ev)-[e:observed_as_selected]->(n:message)";
    let ordinary = compiled(&format!("{base} RETURN e.id"), &scope("a", false));
    assert_eq!(
        grouped_rows(&conn, &ordinary, &["e_id"]),
        vec![vec![Value::Null], vec![Value::Null]]
    );
    let count = compiled(
        &format!("{base} RETURN count(ev), count(e), count(n)"),
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(&conn, &count, &["count_ev", "count_e", "count_n"]),
        vec![vec![Value::Integer(2); 3]]
    );
    let grouped = compiled(&format!("{base} RETURN n.id, count(e)"), &scope("a", false));
    assert_eq!(
        grouped_rows(&conn, &grouped, &["n_id", "count_e"]),
        vec![vec![Value::Text("target".into()), Value::Integer(2)]]
    );
}

#[test]
fn visibility_precedes_counts_distinct_groups_and_the_sentinel() {
    let conn = fixture();
    for (id, properties) in [
        (
            "own-in",
            r#"{"direction":"inbound","to_actor":"a","bucket":"visible"}"#,
        ),
        (
            "own-out",
            r#"{"direction":"outbound","from_actor":"a","bucket":"visible"}"#,
        ),
        (
            "00-foreign",
            r#"{"direction":"inbound","to_actor":"b","bucket":"hidden"}"#,
        ),
        (
            "01-malformed",
            r#"{"direction":"inbound","to_actor":["a"],"bucket":"malformed"}"#,
        ),
        (
            "02-deleted",
            r#"{"direction":"inbound","to_actor":"a","bucket":"deleted"}"#,
        ),
        (
            "03-namespace",
            r#"{"direction":"inbound","to_actor":"a","bucket":"namespace"}"#,
        ),
    ] {
        note(&conn, id, "message", Some(properties));
    }
    note(&conn, "04-legacy", "message", None);
    conn.execute(
        "UPDATE notes SET deleted_at = 1 WHERE id = '02-deleted'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET namespace = 'foreign' WHERE id = '03-namespace'",
        [],
    )
    .unwrap();
    entity(&conn, "root");
    for id in [
        "own-in",
        "own-out",
        "00-foreign",
        "01-malformed",
        "02-deleted",
        "03-namespace",
        "04-legacy",
    ] {
        edge(&conn, &format!("edge-{id}"), "root", id);
    }
    let changes: i64 = conn
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    for pattern in [
        "MATCH (n:message)",
        "MATCH (a)-[:extends*0..2]->(n:message) WHERE a.id = 'root'",
        "MATCH (a)-[:extends*1..2]->(n:message) WHERE a.id = 'root'",
    ] {
        for (actor, expected) in [("a", 2), ("b", 1)] {
            let query = compiled(&format!("{pattern} RETURN count(n)"), &scope(actor, false));
            assert_eq!(
                grouped_rows(&conn, &query, &["count_n"]),
                vec![vec![Value::Integer(expected)]]
            );
        }
        for projection in [
            "DISTINCT n.properties.bucket",
            "n.properties.bucket, count(n)",
        ] {
            let source = format!("{pattern} RETURN {projection} LIMIT 3");
            let query = parse(QueryLanguage::Gql, &source).unwrap();
            let page = compile_with_mailbox(&query, &options(1), &scope("a", false)).unwrap();
            assert!(page.truncation_check.is_some());
            let (columns, expected) = if projection.contains("count") {
                (
                    vec!["n_properties_bucket", "count_n"],
                    vec![Value::Text("visible".into()), Value::Integer(2)],
                )
            } else {
                (
                    vec!["n_properties_bucket"],
                    vec![Value::Text("visible".into())],
                )
            };
            assert_eq!(
                grouped_rows(&conn, &page, &columns),
                vec![expected],
                "hidden rows must not create a sentinel"
            );
            let exhausted = compiled(
                &format!("{pattern} RETURN {projection} SKIP 1 LIMIT 3"),
                &scope("a", false),
            );
            assert!(grouped_rows(&conn, &exhausted, &columns).is_empty());
        }
    }
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        changes
    );
}

#[test]
fn recursive_counts_keep_first_step_self_loop_and_later_cycle_guard() {
    let conn = fixture();
    entity(&conn, "root");
    entity(&conn, "child");
    edge(&conn, "self", "root", "root");
    edge(&conn, "forward", "root", "child");
    edge(&conn, "back", "child", "root");
    for (range, nodes, edges) in [
        ("0..0", 1, 0),
        ("0..1", 3, 2),
        ("0..3", 4, 3),
        ("1..3", 3, 3),
    ] {
        let query = compiled(
            &format!(
                "MATCH (a)-[e:extends*{range}]->(b) WHERE a.id = 'root' RETURN count(b), count(e)"
            ),
            &scope("a", false),
        );
        assert_eq!(
            grouped_rows(&conn, &query, &["count_b", "count_e"]),
            vec![vec![Value::Integer(nodes), Value::Integer(edges)]],
            "{range}"
        );
    }
}

fn absence_support(conn: &Connection, id: &str, from: &str, to: &str) {
    edge(conn, id, from, to);
    conn.execute(
        "UPDATE graph_edges SET relation = 'supports' WHERE id = ?1",
        [id],
    )
    .unwrap();
}

#[test]
fn not_exists_correlates_substrate_and_uuid_and_keeps_inner_constraints() {
    let conn = fixture();
    entity(&conn, "shared");
    note(&conn, "shared", "observation", Some(r#"{"flag":"note"}"#));
    entity(&conn, "evidence");
    entity(&conn, "unrelated");
    absence_support(&conn, "support", "evidence", "shared");
    conn.execute(
        "UPDATE entities SET name = 'entity name' WHERE id = 'shared'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET name = 'note name' WHERE id = 'shared'",
        [],
    )
    .unwrap();
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    for inner in [
        "(r:note)",
        "(f:concept)-[:supports]->(r:note)",
        "(r:note {flag: 'note'})",
    ] {
        let query = compiled(
            &format!("MATCH (r) WHERE r.id = 'shared' AND NOT EXISTS {{ {inner} }} RETURN r.name"),
            &scope("a", false),
        );
        assert_eq!(run(&conn, &query), ["entity name"], "{inner}");
    }
    for inner in [
        "(r:note {flag: 'different'})",
        "(f:concept {name: 'missing'})-[:supports]->(r)",
        "(r:document)",
    ] {
        let query = compiled(&format!("MATCH (r) WHERE r.id = 'shared' AND NOT EXISTS {{ {inner} }} RETURN r.name ORDER BY r.name"), &scope("a", false));
        assert_eq!(run(&conn, &query), ["entity name", "note name"], "{inner}");
    }
    assert_eq!(run(&conn, &compiled("MATCH (r:concept) WHERE r.id = 'unrelated' AND NOT EXISTS { (f)-[:supports]->(r) } RETURN r.id", &scope("a", false))), ["unrelated"], "unrelated support must not become an uncorrelated EXISTS");
    // Public AST labels are data, even when they contain SQL delimiters.
    let mut query = parse(
        QueryLanguage::Gql,
        "MATCH (r) WHERE r.id = 'shared' AND NOT EXISTS { (r:note) } RETURN r.name",
    )
    .unwrap();
    let label = "odd\";--";
    if let khive_query::ast::PatternElement::Node(node) = &mut query.pattern.elements[0] {
        node.variable = Some(label.into());
    }
    query
        .where_clause
        .for_each_condition_mut(&mut |condition| condition.variable = label.into());
    let khive_query::WhereExpr::And(_, right) = &mut query.where_clause else {
        panic!("AND")
    };
    let khive_query::WhereExpr::NotExists(pattern) = right.as_mut() else {
        panic!("absence")
    };
    if let khive_query::ast::PatternElement::Node(node) = &mut pattern.elements[0] {
        node.variable = Some(label.into());
    }
    query.return_items = vec![khive_query::ReturnItem::Property(
        label.into(),
        "name".into(),
    )];
    let query = compile_with_mailbox(&query, &options(100), &scope("a", false)).unwrap();
    assert_eq!(
        grouped_rows(&conn, &query, &["odd\";--_name"]),
        vec![vec![Value::Text("entity name".into())]]
    );
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn not_exists_visibility_is_checked_on_each_inner_node_and_edge() {
    let conn = fixture();
    let own = r#"{"direction":"inbound","to_actor":"a"}"#;
    for (id, properties) in [
        ("own", Some(own)),
        ("foreign", Some(r#"{"direction":"inbound","to_actor":"b"}"#)),
        (
            "malformed",
            Some(r#"{"direction":"inbound","to_actor":["a"]}"#),
        ),
        ("deleted-source", Some(own)),
        ("namespace-source", Some(own)),
        ("deleted-edge", Some(own)),
        ("namespace-edge", Some(own)),
        ("legacy", None),
    ] {
        let requirement = format!("req-{id}");
        entity(&conn, &requirement);
        note(&conn, id, "message", properties);
        absence_support(&conn, &format!("edge-{id}"), id, &requirement);
    }
    conn.execute(
        "UPDATE notes SET deleted_at = 1 WHERE id = 'deleted-source'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET namespace = 'foreign' WHERE id = 'namespace-source'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE graph_edges SET deleted_at = 1 WHERE id = 'edge-deleted-edge'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE graph_edges SET namespace = 'foreign' WHERE id = 'edge-namespace-edge'",
        [],
    )
    .unwrap();
    let source = "MATCH (r:concept) WHERE r.name STARTS WITH 'req-' AND NOT EXISTS { (f:message)-[:supports]->(r) } RETURN r.id ORDER BY r.id";
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        run(&conn, &compiled(source, &scope("a", false))),
        [
            "req-deleted-edge",
            "req-deleted-source",
            "req-foreign",
            "req-legacy",
            "req-malformed",
            "req-namespace-edge",
            "req-namespace-source"
        ]
    );
    assert_eq!(
        run(&conn, &compiled(source, &scope("local", true))),
        [
            "req-deleted-edge",
            "req-deleted-source",
            "req-foreign",
            "req-malformed",
            "req-namespace-edge",
            "req-namespace-source",
            "req-own"
        ]
    );
    let counted = source.replace("RETURN r.id ORDER BY r.id", "RETURN count(r)");
    assert_eq!(
        grouped_rows(&conn, &compiled(&counted, &scope("a", false)), &["count_r"]),
        vec![vec![Value::Integer(7)]]
    );
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );

    for id in ["visible", "hidden", "deleted", "foreign"] {
        let requirement = format!("two-{id}");
        let mid = format!("mid-{id}");
        entity(&conn, &requirement);
        note(
            &conn,
            &mid,
            "message",
            Some(if id == "hidden" {
                r#"{"direction":"inbound","to_actor":"b"}"#
            } else {
                own
            }),
        );
        absence_support(&conn, &format!("first-{id}"), "own", &mid);
        absence_support(&conn, &format!("last-{id}"), &mid, &requirement);
    }
    conn.execute(
        "UPDATE notes SET deleted_at = 1 WHERE id = 'mid-deleted'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET namespace = 'foreign' WHERE id = 'mid-foreign'",
        [],
    )
    .unwrap();
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    let source = "MATCH (r:concept) WHERE r.name STARTS WITH 'two-' AND NOT EXISTS { (f:message {name: 'own'})-[:supports]->(m:message)-[:supports]->(r) } RETURN r.id ORDER BY r.id";
    assert_eq!(
        run(&conn, &compiled(source, &scope("a", false))),
        ["two-deleted", "two-foreign", "two-hidden"]
    );
    // The same hidden node used as a non-correlated endpoint is still invisible.
    let source = "MATCH (r:concept) WHERE r.name STARTS WITH 'two-' AND NOT EXISTS { (r)<-[:supports]-(m:message) } RETURN r.id ORDER BY r.id";
    assert_eq!(
        run(&conn, &compiled(source, &scope("a", false))),
        ["two-deleted", "two-foreign", "two-hidden"]
    );
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn not_exists_outer_synthetic_node_correlations_keep_typed_referents() {
    let conn = fixture();
    entity(&conn, "target");
    note(
        &conn,
        "target",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"a"}"#),
    );
    entity(&conn, "sink");
    absence_support(&conn, "support", "target", "sink");
    event(&conn, "event");
    entity(&conn, "event"); // A UUID-equal entity is not the observed event node.
    for (kind, position) in [("note", 0), ("note", 1), ("entity", 2)] {
        conn.execute(
            "INSERT INTO event_observations VALUES ('event', 'target', ?1, 'selected', ?2)",
            params![kind, position],
        )
        .unwrap();
    }
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    let base =
        "MATCH (ev)-[e:observed_as_selected]->(n) WHERE NOT EXISTS { (n:entity)-[:supports]->(s) }";
    let query = compiled(&format!("{base} RETURN e.id"), &scope("a", false));
    assert_eq!(
        grouped_rows(&conn, &query, &["e_id"]),
        vec![vec![Value::Null], vec![Value::Null]]
    );
    let query = compiled(
        &format!("{base} RETURN count(n), count(e)"),
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(&conn, &query, &["count_n", "count_e"]),
        vec![vec![Value::Integer(2), Value::Integer(2)]]
    );
    let query = compiled(
        "MATCH (ev)-[e:observed_as_selected]->(n) WHERE NOT EXISTS { (ev:entity) } RETURN count(e)",
        &scope("a", false),
    );
    assert_eq!(
        grouped_rows(&conn, &query, &["count_e"]),
        vec![vec![Value::Integer(3)]]
    );
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn recursive_not_exists_uses_typed_positive_minimum_joins() {
    let conn = fixture();
    for id in ["start", "end"] {
        entity(&conn, id);
        note(&conn, id, "observation", None);
        conn.execute(
            "UPDATE entities SET name = ?1 WHERE id = ?2",
            params![format!("entity {id}"), id],
        )
        .unwrap();
        conn.execute(
            "UPDATE notes SET name = ?1 WHERE id = ?2",
            params![format!("note {id}"), id],
        )
        .unwrap();
    }
    edge(&conn, "path", "start", "end");
    absence_support(&conn, "support", "start", "end");
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    for min in [0, 1] {
        let base = format!("MATCH (a)-[:extends*{min}..2]->(b) WHERE a.id = 'start' AND b.id = 'end' AND NOT EXISTS {{ (a:note)-[:supports]->(b:note) }}");
        for (kind, expected) in [
            (
                "",
                vec![
                    ("entity start", "entity end"),
                    ("entity start", "note end"),
                    ("note start", "entity end"),
                ],
            ),
            (
                ":entity",
                vec![("entity start", "entity end"), ("entity start", "note end")],
            ),
            (":note", vec![("note start", "entity end")]),
        ] {
            // A UUID-only final start join would reintroduce the other substrate
            // after the seed's kind filter; DISTINCT cannot hide that wrong pair.
            let source = base.replacen("MATCH (a)", &format!("MATCH (a{kind})"), 1);
            let query = compiled(
                &format!("{source} RETURN a.name, b.name ORDER BY a.name, b.name"),
                &scope("a", false),
            );
            let rows = grouped_rows(
                &conn,
                &query,
                &["a_name", "b_name", "_depth", "_total_weight"],
            );
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(a, b)| {
                    vec![
                        Value::Text(a.into()),
                        Value::Text(b.into()),
                        Value::Integer(1),
                        Value::Real(1.0),
                    ]
                })
                .collect();
            assert_eq!(
                rows, expected,
                "ordinary typed traversal at minimum {min}, start {kind}"
            );
        }
        let query = compiled(
            &format!("{base} RETURN b.name, count(b) ORDER BY b.name"),
            &scope("a", false),
        );
        assert_eq!(
            grouped_rows(&conn, &query, &["b_name", "count_b"]),
            vec![
                vec![Value::Text("entity end".into()), Value::Integer(2)],
                vec![Value::Text("note end".into()), Value::Integer(1)]
            ],
            "mixed correlation must join the unreturned typed start at minimum {min}"
        );
    }
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn not_exists_singleton_and_true_or_follow_recorded_contract() {
    use khive_query::WhereExpr;
    let conn = fixture();
    entity(&conn, "root");
    entity(&conn, "child");
    edge(&conn, "path", "root", "child");
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    for (pattern, predicate, projection, expected) in [
        ("(r:concept)", "(r:concept)", "r.id", vec!["child", "root"]),
        (
            "(a:concept {name: 'root'})-[:extends*1..2]->(r)",
            "(r:concept)",
            "r.id",
            vec!["child"],
        ),
        (
            "(a:concept {name: 'root'})-[:extends*1..2]->(r)",
            "(a:concept)",
            "r.id",
            vec!["child"],
        ),
    ] {
        let source =
            format!("MATCH {pattern} WHERE NOT EXISTS {{ {predicate} }} RETURN {projection}");
        let base = parse(QueryLanguage::Gql, &source).unwrap();
        assert!(
            run(
                &conn,
                &compile_with_mailbox(&base, &options(100), &scope("a", false)).unwrap()
            )
            .is_empty(),
            "absence is false before adding True: {source}"
        );
        for true_first in [true, false] {
            for nested in [false, true] {
                let mut query = base.clone();
                let absence = query.where_clause.clone();
                let either = if true_first {
                    WhereExpr::Or(Box::new(WhereExpr::True), Box::new(absence.clone()))
                } else {
                    WhereExpr::Or(Box::new(absence.clone()), Box::new(WhereExpr::True))
                };
                // Nested OR also reaches the recursive expression-to-SQL walker.
                query.where_clause = if nested {
                    WhereExpr::Or(Box::new(either), Box::new(absence))
                } else {
                    either
                };
                let query =
                    compile_with_mailbox(&query, &options(100), &scope("a", false)).unwrap();
                assert_eq!(
                    run(&conn, &query),
                    expected,
                    "True first={true_first}, nested={nested}: {source}"
                );
            }
        }
    }
    let query = compiled(
        "MATCH (r:concept) WHERE NOT EXISTS { (r {name: 'child'}) } RETURN r.id",
        &scope("a", false),
    );
    assert_eq!(run(&conn, &query), ["root"]);
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

fn synthetic_id_rows_fixture() -> Connection {
    let conn = fixture();
    note(
        &conn,
        "target",
        "message",
        Some(r#"{"direction":"inbound","to_actor":"a"}"#),
    );
    event(&conn, "event");
    for position in [0, 1] {
        conn.execute(
            "INSERT INTO event_observations VALUES ('event', 'target', 'note', 'selected', ?1)",
            [position],
        )
        .unwrap();
    }
    conn
}

#[test]
fn synthetic_edge_id_projection_preserves_null_groups_and_binding_counts() {
    let conn = synthetic_id_rows_fixture();
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    let pattern = "MATCH (ev)-[e:observed_as_selected]->(n:message)";
    for (projection, order, expected) in [
        ("e.id", "", vec![vec![Value::Null], vec![Value::Null]]),
        ("DISTINCT e.id", " ORDER BY e.id", vec![vec![Value::Null]]),
        (
            "e.id, count(e)",
            " ORDER BY e.id",
            vec![vec![Value::Null, Value::Integer(2)]],
        ),
        (
            "DISTINCT e.id, count(e)",
            " ORDER BY e.id DESC",
            vec![vec![Value::Null, Value::Integer(2)]],
        ),
    ] {
        let source = format!("{pattern} RETURN {projection}{order}");
        let query = compiled(&source, &scope("a", false));
        let preparation = conn.prepare(&query.sql);
        assert!(
            preparation.is_ok(),
            "synthetic edge ID projection must prepare against the stored observation schema: {source}; {:?}",
            preparation.as_ref().err()
        );
        drop(preparation);
        let columns = if projection.contains("count") {
            vec!["e_id", "count_e"]
        } else {
            vec!["e_id"]
        };
        assert_eq!(
            grouped_rows(&conn, &query, &columns),
            expected,
            "synthetic edge IDs must remain null without collapsing observation bindings: {source}"
        );
    }
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn synthetic_edge_id_ordering_preserves_observation_rows() {
    let conn = synthetic_id_rows_fixture();
    let before: i64 = conn
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    for direction in ["ASC", "DESC"] {
        let source = format!(
            "MATCH (ev)-[e:observed_as_selected]->(n:message) RETURN n.id, ev.id ORDER BY e.id {direction}"
        );
        let query = compiled(&source, &scope("a", false));
        let preparation = conn.prepare(&query.sql);
        assert!(
            preparation.is_ok(),
            "synthetic edge ID ordering must prepare against the stored observation schema: {source}; {:?}",
            preparation.as_ref().err()
        );
        drop(preparation);
        assert_eq!(
            grouped_rows(&conn, &query, &["n_id", "ev_id"]),
            vec![
                vec![Value::Text("target".into()), Value::Text("event".into())],
                vec![Value::Text("target".into()), Value::Text("event".into())],
            ],
            "ordering by a null synthetic edge ID must preserve both observation positions"
        );
    }
    assert_eq!(
        conn.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn synthetic_edge_id_where_remains_refused() {
    for predicate in ["e.id IS NULL", "e.id = 'event'"] {
        let source = format!(
            "MATCH (ev)-[e:observed_as_selected]->(n:message) WHERE {predicate} RETURN n.id"
        );
        let query = parse(QueryLanguage::Gql, &source).unwrap();
        let result = compile_with_mailbox(&query, &options(100), &scope("a", false));
        assert!(
            matches!(
                result,
                Err(khive_query::QueryError::Compile(message))
                    if message.contains("unknown edge field 'id'") && message.contains("WHERE")
            ),
            "synthetic edge IDs are not admitted in WHERE: {source}"
        );
    }
}

fn quarantine_scope(
    actor: &str,
    legacy_local: bool,
    maintainer: Option<&str>,
) -> QueryMailboxScope {
    let mut mailbox = scope(actor, legacy_local);
    mailbox.maintainer_actor = maintainer.map(str::to_owned);
    mailbox
}

#[test]
fn quarantine_partition_precedes_note_limits_and_query_endpoint_hydration() {
    let conn = fixture();
    let rows = [
        (
            "00-boolean",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":true}"#,
            true,
            true,
            false,
            false,
        ),
        (
            "01-string",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":"true"}"#,
            true,
            true,
            false,
            false,
        ),
        (
            "02-outbound",
            r#"{"direction":"outbound","from_actor":"landing","to_actor":"landing","quarantined":true}"#,
            true,
            true,
            false,
            false,
        ),
        (
            "03-directionless",
            r#"{"to_actor":"landing","quarantined":"true"}"#,
            true,
            false,
            false,
            false,
        ),
        (
            "04-malformed",
            r#"{"direction":"other","to_actor":17,"quarantined":true}"#,
            true,
            false,
            false,
            false,
        ),
        (
            "05-null-routing",
            r#"{"direction":null,"quarantined":true}"#,
            true,
            false,
            false,
            true,
        ),
        (
            "06-number",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":1}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "07-real",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":1.0}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "08-missing",
            r#"{"direction":"inbound","to_actor":"landing"}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "09-null",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":null}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "10-false",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":false}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "11-uppercase",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":"True"}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "12-array",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":[true]}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "13-object",
            r#"{"direction":"inbound","to_actor":"landing","quarantined":{"value":true}}"#,
            false,
            true,
            false,
            false,
        ),
        (
            "14-maintainer",
            r#"{"direction":"inbound","to_actor":"maintainer","quarantined":true}"#,
            true,
            false,
            true,
            false,
        ),
        ("15-legacy", "{}", false, false, false, true),
    ];
    entity(&conn, "root");
    for (id, properties, _, _, _, _) in rows {
        note(&conn, id, "message", Some(properties));
        edge(&conn, &format!("edge-{id}"), "root", id);
    }
    note(
        &conn,
        "16-ordinary",
        "observation",
        Some(r#"{"quarantined":true}"#),
    );
    edge(&conn, "edge-16-ordinary", "root", "16-ordinary");
    for id in ["foreign-quarantine", "deleted-quarantine"] {
        note(&conn, id, "message", Some(rows[0].1));
    }
    conn.execute(
        "UPDATE notes SET namespace = 'foreign' WHERE id = 'foreign-quarantine'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE notes SET deleted_at = 1 WHERE id = 'deleted-quarantine'",
        [],
    )
    .unwrap();

    for maintainer in [None, Some(""), Some("maintainer")] {
        for (actor, legacy_local) in [
            ("landing", false),
            ("maintainer", false),
            ("other", false),
            ("local", false),
            ("local", true),
        ] {
            let mailbox = quarantine_scope(actor, legacy_local, maintainer);
            let mut expected = rows
                .iter()
                .filter(|row| {
                    if maintainer.is_some_and(|value| !value.is_empty()) && row.2 {
                        actor == maintainer.unwrap()
                    } else {
                        (actor == "landing" && row.3)
                            || (actor == "maintainer" && row.4)
                            || (actor == "local" && legacy_local && row.5)
                    }
                })
                .map(|row| row.0.to_string())
                .collect::<Vec<_>>();
            let messages = expected.clone();
            expected.push("16-ordinary".into());
            assert_eq!(
                run(&conn, &compiled("MATCH (n:message) RETURN n.id", &mailbox)),
                messages,
                "{mailbox:?}"
            );
            assert_eq!(
                run(&conn, &compiled("MATCH (n:note) RETURN n.id", &mailbox)),
                expected,
                "{mailbox:?}"
            );
            let query = parse(
                QueryLanguage::Sparql,
                "SELECT ?b WHERE { ?a :name \"root\" . ?a :extends ?b . }",
            )
            .unwrap();
            assert_eq!(
                run(
                    &conn,
                    &compile_with_mailbox(&query, &options(100), &mailbox).unwrap()
                ),
                expected,
                "{mailbox:?}"
            );
            let expected_edges = expected
                .iter()
                .map(|id| format!("edge-{id}"))
                .collect::<Vec<_>>();
            assert_eq!(
                run(&conn, &compiled("MATCH (e:edge) RETURN e.id", &mailbox)),
                expected_edges,
                "{mailbox:?}"
            );
            for offset in 0..=messages.len() {
                let total_limit = offset + 1;
                let query = parse(
                    QueryLanguage::Gql,
                    &format!("MATCH (n:message) RETURN n.id SKIP {offset} LIMIT {total_limit}"),
                )
                .unwrap();
                let actual = compile_with_mailbox(&query, &options(100), &mailbox).unwrap();
                assert_eq!(
                    run(&conn, &actual),
                    messages
                        .iter()
                        .skip(offset)
                        .take(1)
                        .cloned()
                        .collect::<Vec<_>>()
                );
            }
            if maintainer == Some("") {
                let empty = compiled("MATCH (n:message) RETURN n.id", &mailbox);
                let original =
                    compiled("MATCH (n:message) RETURN n.id", &scope(actor, legacy_local));
                assert_eq!(empty.sql, original.sql);
                let values = |query: &CompiledQuery| {
                    query
                        .params
                        .iter()
                        .map(|value| match value {
                            QueryValue::Null => Value::Null,
                            QueryValue::Integer(value) => Value::Integer(*value),
                            QueryValue::Float(value) => Value::Real(*value),
                            QueryValue::Text(value) => Value::Text(value.clone()),
                            QueryValue::Blob(value) => Value::Blob(value.clone()),
                        })
                        .collect::<Vec<_>>()
                };
                assert_eq!(values(&empty), values(&original));
            }
        }
    }
    let actor = "maintainer' OR 1=1 --";
    let mailbox = quarantine_scope(actor, false, Some(actor));
    let query = compiled("MATCH (n:message) RETURN n.id", &mailbox);
    assert!(!query.sql.contains(actor));
    assert_eq!(
        query
            .params
            .iter()
            .filter(|value| matches!(value, QueryValue::Text(text) if text == actor))
            .count(),
        2
    );
    assert_eq!(
        run(&conn, &query),
        rows.iter()
            .filter(|row| row.2)
            .map(|row| row.0.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn quarantine_edge_and_recursive_scopes_preserve_entity_precedence() {
    let conn = fixture();
    entity(&conn, "root");
    entity(&conn, "end");
    for (id, value) in [
        ("q-boolean", "true"),
        ("q-string", "\"true\""),
        ("dead-q", "true"),
        ("shared-q", "true"),
        ("numeric", "1"),
    ] {
        let properties =
            format!(r#"{{"direction":"inbound","to_actor":"landing","quarantined":{value}}}"#);
        note(&conn, id, "message", Some(&properties));
        edge(&conn, &format!("edge-{id}"), "root", id);
    }
    conn.execute("UPDATE notes SET deleted_at = 1 WHERE id = 'dead-q'", [])
        .unwrap();
    entity(&conn, "shared-q");
    edge(&conn, "edge-q-source", "q-boolean", "end");
    let landing = quarantine_scope("landing", false, Some("maintainer"));
    let maintainer = quarantine_scope("maintainer", false, Some("maintainer"));
    assert_eq!(
        run(&conn, &compiled("MATCH (e:edge) RETURN e.id", &landing)),
        ["edge-numeric", "edge-shared-q"]
    );
    assert_eq!(
        run(&conn, &compiled("MATCH (e:edge) RETURN e.id", &maintainer)),
        [
            "edge-dead-q",
            "edge-q-boolean",
            "edge-q-source",
            "edge-q-string",
            "edge-shared-q"
        ]
    );
    for mailbox in [&landing, &maintainer] {
        let query = compiled(
            "MATCH (a)-[:extends*1..2]->(b) WHERE a.id = 'root' RETURN b.id",
            mailbox,
        );
        let mut actual = run(&conn, &query);
        actual.sort();
        let expected = if mailbox.actor_id == "maintainer" {
            vec!["end", "q-boolean", "q-string", "shared-q"]
        } else {
            vec!["numeric", "shared-q"]
        };
        assert_eq!(actual, expected);
    }
}
