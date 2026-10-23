use khive_query::{
    compile, compile_with_mailbox, parse, CompileOptions, CompiledQuery, QueryLanguage,
    QueryMailboxScope, QueryValue,
};
use rusqlite::{params, params_from_iter, types::Value, Connection};

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
    for (offset, expected, sentinel) in [
        (0, vec!["row-1", "row-3", "row-5"], true),
        (2, vec!["row-5"], false),
        (3, vec![], false),
    ] {
        let query = parse(
            QueryLanguage::Gql,
            &format!("MATCH (n:message) RETURN n.id SKIP {offset} LIMIT 3"),
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
                let query = parse(
                    QueryLanguage::Gql,
                    &format!("MATCH (n:message) RETURN n.id SKIP {offset} LIMIT 1"),
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
