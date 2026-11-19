// --- Issue #849: substrate node labels (entity/note) must be satisfiable ---
//
// Stored `kind` values are always granular (concept/document/task/...); the
// substrate words entity/note/edge/event name a *table*, not a stored `kind`.
// Compiling a bare substrate label straight into `kind = ?` (as fixed/`event`
// filters did) makes the predicate unsatisfiable by construction. The fix
// filters the union's `substrate_kind` discriminator column instead. These
// tests exercise the compiled SQL shape (fixed-length + variable-length +
// SPARQL) and, for the fixed-length case, execute the compiled SQL against a
// minimal in-memory fixture matching `khive-db`'s substrate schema to prove
// the query is actually satisfiable end-to-end, not just shaped correctly.

mod substrate_labels {
    use super::*;
    use rusqlite::Connection;

    /// Minimal fixture matching `crates/khive-db/sql/schema.sql`'s substrate
    /// tables. All four must exist (even empty) because the compiler always
    /// binds a plain node pattern through the `entities UNION notes UNION
    /// events UNION graph_edges` primary-substrate source.
    fn fixture_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE entities (
                id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
                name TEXT NOT NULL, description TEXT, properties TEXT,
                tags TEXT NOT NULL DEFAULT '[]', created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted_at INTEGER,
                entity_type TEXT, merged_into TEXT, merge_event_id TEXT
            );
            CREATE TABLE notes (
                id TEXT PRIMARY KEY, namespace TEXT NOT NULL, kind TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'active', name TEXT,
                content TEXT NOT NULL DEFAULT '', salience REAL, decay_factor REAL,
                expires_at INTEGER, properties TEXT, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted_at INTEGER
            );
            CREATE TABLE events (
                id TEXT PRIMARY KEY, namespace TEXT NOT NULL, verb TEXT NOT NULL,
                substrate TEXT NOT NULL, actor TEXT NOT NULL, outcome TEXT NOT NULL,
                data TEXT, duration_us INTEGER NOT NULL DEFAULT 0, target_id TEXT,
                created_at INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT 'audit',
                payload TEXT NOT NULL DEFAULT '{}',
                payload_schema_version INTEGER NOT NULL DEFAULT 1,
                profile_state_version INTEGER, session_id TEXT,
                aggregate_kind TEXT, aggregate_id TEXT
            );
            CREATE TABLE event_observations (
                event_id TEXT NOT NULL, entity_id TEXT NOT NULL,
                referent_kind TEXT NOT NULL, role TEXT NOT NULL,
                position INTEGER NOT NULL,
                PRIMARY KEY (event_id, role, position)
            );
            CREATE TABLE graph_edges (
                namespace TEXT NOT NULL, id TEXT NOT NULL, source_id TEXT NOT NULL,
                target_id TEXT NOT NULL, relation TEXT NOT NULL,
                weight REAL NOT NULL DEFAULT 1.0, created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL, deleted_at INTEGER, metadata TEXT,
                target_backend TEXT, PRIMARY KEY (namespace, id)
            );
            INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
            VALUES
                ('e-fixture-1', 'local', 'concept', 'X', NULL, '{}', '[]',
                 0, 0, NULL, NULL),
                ('e-fixture-spaces', 'local', 'project', 'Mixed Case Entity',
                 NULL, '{}', '[]', 0, 0, NULL, NULL),
                ('e-fixture-cjk', 'local', 'document', '知识 图谱', NULL, '{}', '[]',
                 0, 0, NULL, NULL);
            INSERT INTO notes
                (id, namespace, kind, status, name, content, salience,
                 decay_factor, expires_at, properties, created_at, updated_at,
                 deleted_at)
            VALUES
                ('n-fixture-1', 'local', 'observation', 'active', 'X', 'body',
                 NULL, NULL, NULL, '{}', 0, 0, NULL);",
        )
        .unwrap();
        conn
    }

    fn run(conn: &Connection, compiled: &CompiledQuery) -> Vec<String> {
        let db_params: Vec<Box<dyn rusqlite::ToSql>> = compiled
            .params
            .iter()
            .map(|p| -> Box<dyn rusqlite::ToSql> {
                match p {
                    QueryValue::Null => Box::new(Option::<i64>::None),
                    QueryValue::Integer(n) => Box::new(*n),
                    QueryValue::Float(n) => Box::new(*n),
                    QueryValue::Text(s) => Box::new(s.clone()),
                    QueryValue::Blob(b) => Box::new(b.clone()),
                }
            })
            .collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = db_params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&compiled.sql).unwrap();
        stmt.query_map(param_refs.as_slice(), |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn synthetic_candidate_and_selected_queries_follow_entity_and_note_referents() {
        let conn = fixture_db();
        conn.execute_batch(
            "INSERT INTO events
                (id, namespace, verb, substrate, actor, outcome, created_at, kind, payload)
             VALUES
                ('ev-search', 'local', 'search', 'entity', 'agent:test', 'success', 1,
                 'search_executed', '{}'),
                ('ev-recall', 'local', 'memory.recall', 'note', 'agent:test', 'success', 2,
                 'recall_executed', '{}');
             INSERT INTO event_observations
                (event_id, entity_id, referent_kind, role, position)
             VALUES
                ('ev-search', 'e-fixture-1', 'entity', 'candidate', 0),
                ('ev-search', 'e-fixture-1', 'entity', 'selected', 0),
                ('ev-recall', 'n-fixture-1', 'note', 'candidate', 0),
                ('ev-recall', 'n-fixture-1', 'note', 'selected', 0);",
        )
        .unwrap();
        for role in ["candidate", "selected"] {
            for (substrate, expected) in [("entity", "e-fixture-1"), ("note", "n-fixture-1")] {
                let query =
                    format!("MATCH (ev:event)-[:observed_as_{role}]->(t:{substrate}) RETURN t.id");
                let parsed = parse(QueryLanguage::Gql, &query).unwrap();
                let compiled = compile(&parsed, &scoped("local")).unwrap();
                assert_eq!(run(&conn, &compiled), vec![expected], "query: {query}");
            }
        }
    }

    fn insert_entity(conn: &Connection, id: &str, name: &str, properties: &str) {
        conn.execute(
            "INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
             VALUES (?1, 'local', 'concept', ?2, NULL, ?3, '[]', 0, 0, NULL, NULL)",
            rusqlite::params![id, name, properties],
        )
        .unwrap();
    }

    #[test]
    fn contains_escapes_wildcards_backslash_and_quote_end_to_end() {
        let conn = fixture_db();
        insert_entity(&conn, "e-literal", r#"prefix%_\' OR 1=1 --suffix"#, "{}");
        insert_entity(
            &conn,
            "e-wildcard-distractor",
            r#"prefixAB\' OR 1=1 --suffix"#,
            "{}",
        );
        let q = parse(
            QueryLanguage::Gql,
            r#"MATCH (e:entity) WHERE e.name CONTAINS "%_\' OR 1=1 --" RETURN e.id"#,
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert!(!compiled.sql.contains("OR 1=1"), "sql: {}", compiled.sql);
        assert_eq!(run(&conn, &compiled), vec!["e-literal"]);
    }

    #[test]
    fn starts_with_escapes_wildcards_and_backslash_end_to_end() {
        let conn = fixture_db();
        insert_entity(&conn, "e-prefix", r#"pre%_\suffix"#, "{}");
        insert_entity(&conn, "e-prefix-distractor", r#"preAB\suffix"#, "{}");
        let q = parse(
            QueryLanguage::Gql,
            r#"MATCH (e:entity) WHERE e.name STARTS WITH "pre%_\" RETURN e.id"#,
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-prefix"]);
    }

    #[test]
    fn in_list_binds_injection_shaped_strings_end_to_end() {
        let conn = fixture_db();
        insert_entity(&conn, "e-percent", "%", "{}");
        insert_entity(&conn, "e-underscore", "_", "{}");
        insert_entity(&conn, "e-quote", "' OR 1=1 --", "{}");
        insert_entity(&conn, "e-other", "not a match", "{}");
        let q = parse(
            QueryLanguage::Gql,
            r#"MATCH (e:entity) WHERE e.name IN ["%", "_", "' OR 1=1 --"] RETURN e.id"#,
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(!compiled.sql.contains("OR 1=1"), "sql: {}", compiled.sql);

        let mut rows = run(&conn, &compiled);
        rows.sort();
        assert_eq!(rows, vec!["e-percent", "e-quote", "e-underscore"]);
    }

    #[test]
    fn mixed_type_in_preserves_case_insensitive_string_matching_end_to_end() {
        let conn = fixture_db();
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) WHERE e.kind IN ['CONCEPT', 1] RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-fixture-1"]);
    }

    #[test]
    fn is_not_null_filters_json_property_presence_end_to_end() {
        let conn = fixture_db();
        insert_entity(
            &conn,
            "e-with-domain",
            "with domain",
            r#"{"domain":"attention"}"#,
        );
        insert_entity(&conn, "e-without-domain", "without domain", "{}");
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) WHERE e.properties.domain IS NOT NULL RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-with-domain"]);
    }

    #[test]
    fn nested_json_property_path_filters_end_to_end() {
        let conn = fixture_db();
        insert_entity(
            &conn,
            "e-high",
            "high severity",
            r#"{"finding":{"severity":"high"}}"#,
        );
        insert_entity(
            &conn,
            "e-low",
            "low severity",
            r#"{"finding":{"severity":"low"}}"#,
        );
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) \
             WHERE e.properties.finding.severity = 'high' RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();

        assert_eq!(run(&conn, &compiled), vec!["e-high"]);
    }

    /// `fixture_db()` plus a second entity and a `graph_edges` row connecting
    /// it to `e-fixture-1`. SPARQL requires at least one variable-to-variable
    /// relation triple to parse (`no edge patterns found` otherwise), so any
    /// SPARQL regression test needs a connected graph, unlike the single-node
    /// GQL substrate-label tests below.
    fn fixture_db_with_edge() -> Connection {
        let conn = fixture_db();
        conn.execute_batch(
            "INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
            VALUES
                ('e-fixture-2', 'local', 'document', 'Y', NULL, '{}', '[]',
                 0, 0, NULL, NULL);
            INSERT INTO graph_edges
                (namespace, id, source_id, target_id, relation, weight,
                 created_at, updated_at, deleted_at, metadata, target_backend)
            VALUES
                ('local', 'edge-fixture-1', 'e-fixture-1', 'e-fixture-2',
                 'extends', 1.0, 0, 0, NULL, NULL, NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn repeated_name_predicates_with_ascii_case_variants_preserve_matches() {
        let conn = fixture_db_with_edge();
        for relation in ["extends", "extends+"] {
            for (var, expected, upper, lower) in [
                ("a", "e-fixture-1", "X", "x"),
                ("b", "e-fixture-2", "Y", "y"),
            ] {
                for (first, second) in [(upper, lower), (lower, upper)] {
                    let single = format!(
                        "SELECT ?{var} WHERE {{ ?a :{relation} ?b . \
                         ?{var} :name '{first}' . }}"
                    );
                    let control = parse(QueryLanguage::Sparql, &single).unwrap();
                    assert_eq!(
                        run(&conn, &compile(&control, &scoped("local")).unwrap()),
                        vec![expected.to_string()],
                        "single predicate must match: {single}"
                    );
                    let repeated = format!(
                        "SELECT ?{var} WHERE {{ ?a :{relation} ?b . \
                         ?{var} :name '{first}' . ?{var} :name '{second}' . }}"
                    );
                    let query = parse(QueryLanguage::Sparql, &repeated).unwrap_or_else(|e| {
                        panic!("NOCASE-equivalent predicates: {repeated}: {e}")
                    });
                    assert_eq!(
                        run(&conn, &compile(&query, &scoped("local")).unwrap()),
                        vec![expected.to_string()],
                        "a redundant equivalent predicate must preserve matches: {repeated}"
                    );
                }
            }
        }
    }

    #[test]
    fn repeated_json_string_predicates_with_ascii_case_variants_preserve_matches() {
        let conn = fixture_db_with_edge();
        assert_eq!(
            conn.execute(
                "UPDATE entities SET properties = ?1 WHERE id = ?2",
                rusqlite::params![r#"{"domain":"Attention"}"#, "e-fixture-1"],
            )
            .unwrap(),
            1
        );
        for relation in ["extends", "extends+"] {
            let control_text =
                format!("SELECT ?a WHERE {{ ?a :{relation} ?b . ?a :domain 'attention' . }}");
            let control = parse(QueryLanguage::Sparql, &control_text).unwrap();
            assert_eq!(
                run(&conn, &compile(&control, &scoped("local")).unwrap()),
                vec!["e-fixture-1"]
            );
            for constraints in [
                "?a :domain 'Attention' . ?a :domain 'attention' .",
                "?a :domain 'attention' . ?a :domain 'Attention' .",
                "?a :domain :attention . ?a :domain 'ATTENTION' .",
                "?a :domain 'ATTENTION' . ?a :domain :attention .",
            ] {
                let text = format!("SELECT ?a WHERE {{ ?a :{relation} ?b . {constraints} }}");
                let query = parse(QueryLanguage::Sparql, &text)
                    .unwrap_or_else(|e| panic!("equivalent JSON-property predicates: {text}: {e}"));
                assert_eq!(
                    run(&conn, &compile(&query, &scoped("local")).unwrap()),
                    vec!["e-fixture-1"],
                    "query: {text}"
                );
            }
        }
    }

    #[test]
    fn repeated_entity_type_predicates_remain_case_sensitive() {
        let conn = fixture_db_with_edge();
        assert_eq!(
            conn.execute(
                "UPDATE entities SET entity_type = 'Widget' WHERE id = 'e-fixture-1'",
                [],
            )
            .unwrap(),
            1
        );
        for (literal, should_match) in [("Widget", true), ("widget", false)] {
            let text =
                format!("SELECT ?a WHERE {{ ?a :entity_type '{literal}' . ?a :extends ?b . }}");
            let query = parse(QueryLanguage::Sparql, &text).unwrap();
            let expected: Vec<String> = if should_match {
                vec!["e-fixture-1".into()]
            } else {
                Vec::new()
            };
            assert_eq!(
                run(&conn, &compile(&query, &scoped("local")).unwrap()),
                expected
            );
        }
        for (first, second) in [("Widget", "widget"), ("widget", "Widget")] {
            let text = format!(
                "SELECT ?a WHERE {{ ?a :entity_type '{first}' . \
                 ?a :entity_type '{second}' . ?a :extends ?b . }}"
            );
            let err = parse(QueryLanguage::Sparql, &text).unwrap_err();
            assert!(matches!(err, QueryError::Parse { .. }), "{err:?}");
            assert!(err.to_string().contains("conflicting constraints"), "{err}");
        }
    }

    #[test]
    fn non_ascii_case_and_distinct_name_predicates_remain_conflicting() {
        let conn = fixture_db_with_edge();
        assert_eq!(
            conn.execute(
                "UPDATE entities SET name = ?1 WHERE id = ?2",
                rusqlite::params!["É", "e-fixture-1"],
            )
            .unwrap(),
            1
        );
        for (literal, should_match) in [("É", true), ("é", false)] {
            let text = format!("SELECT ?a WHERE {{ ?a :name '{literal}' . ?a :extends ?b . }}");
            let query = parse(QueryLanguage::Sparql, &text).unwrap();
            let expected: Vec<String> = if should_match {
                vec!["e-fixture-1".into()]
            } else {
                Vec::new()
            };
            assert_eq!(
                run(&conn, &compile(&query, &scoped("local")).unwrap()),
                expected
            );
        }
        for (first, second) in [("É", "é"), ("x", "y")] {
            let text = format!(
                "SELECT ?a WHERE {{ ?a :name '{first}' . \
                 ?a :name '{second}' . ?a :extends ?b . }}"
            );
            let err = parse(QueryLanguage::Sparql, &text).unwrap_err();
            assert!(matches!(err, QueryError::Parse { .. }), "{err:?}");
            assert!(err.to_string().contains("conflicting constraints"), "{err}");
        }
    }

    fn insert_path_node(conn: &Connection, id: &str, namespace: &str, deleted_at: Option<i64>) {
        conn.execute(
            "INSERT INTO entities
                (id, namespace, kind, name, description, properties, tags,
                 created_at, updated_at, deleted_at, entity_type)
             VALUES (?1, ?2, 'concept', ?1, NULL, '{}', '[]', 0, 0, ?3, NULL)",
            rusqlite::params![id, namespace, deleted_at],
        )
        .unwrap();
    }

    fn insert_local_path_edge(conn: &Connection, id: &str, source: &str, target: &str) {
        conn.execute(
            "INSERT INTO graph_edges
                (namespace, id, source_id, target_id, relation, weight,
                 created_at, updated_at, deleted_at, metadata, target_backend)
             VALUES ('local', ?1, ?2, ?3, 'extends', 1.0, 0, 0, NULL, NULL, NULL)",
            rusqlite::params![id, source, target],
        )
        .unwrap();
    }

    fn depth_two_from_fixture(conn: &Connection) -> Vec<String> {
        let query = parse(
            QueryLanguage::Gql,
            "MATCH (a)-[:extends*2..2]->(b) \
             WHERE a.id = 'e-fixture-1' RETURN b.id",
        )
        .unwrap();
        run(conn, &compile(&query, &scoped("local")).unwrap())
    }

    #[test]
    fn variable_length_seed_hop_cannot_cross_soft_deleted_node() {
        let conn = fixture_db();
        insert_path_node(&conn, "seed-deleted", "local", Some(1));
        insert_path_node(&conn, "behind-deleted", "local", None);
        insert_path_node(&conn, "seed-live", "local", None);
        insert_path_node(&conn, "behind-live", "local", None);
        insert_local_path_edge(&conn, "edge-deleted-1", "e-fixture-1", "seed-deleted");
        insert_local_path_edge(&conn, "edge-deleted-2", "seed-deleted", "behind-deleted");
        insert_local_path_edge(&conn, "edge-live-1", "e-fixture-1", "seed-live");
        insert_local_path_edge(&conn, "edge-live-2", "seed-live", "behind-live");

        assert_eq!(depth_two_from_fixture(&conn), vec!["behind-live"]);
    }

    #[test]
    fn variable_length_seed_hop_cannot_cross_out_of_scope_node() {
        let conn = fixture_db();
        insert_path_node(&conn, "seed-foreign", "other", None);
        insert_path_node(&conn, "behind-foreign", "local", None);
        insert_path_node(&conn, "seed-live", "local", None);
        insert_path_node(&conn, "behind-live", "local", None);
        insert_local_path_edge(&conn, "edge-foreign-1", "e-fixture-1", "seed-foreign");
        insert_local_path_edge(&conn, "edge-foreign-2", "seed-foreign", "behind-foreign");
        insert_local_path_edge(&conn, "edge-live-1", "e-fixture-1", "seed-live");
        insert_local_path_edge(&conn, "edge-live-2", "seed-live", "behind-live");

        assert_eq!(depth_two_from_fixture(&conn), vec!["behind-live"]);
    }

    #[test]
    fn entity_substrate_label_compiles_without_unsatisfiable_kind_filter() {
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (e:entity) WHERE e.name = 'X' RETURN e.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("substrate_kind = ?"),
            "substrate label 'entity' must filter substrate_kind, not kind; sql: {}",
            compiled.sql
        );
        assert!(
            !compiled.sql.contains("n0.kind = ?"),
            "substrate label 'entity' must not also emit an unsatisfiable kind filter; sql: {}",
            compiled.sql
        );

        let conn = fixture_db();
        let rows = run(&conn, &compiled);
        assert_eq!(
            rows,
            vec!["e-fixture-1".to_string()],
            "MATCH (e:entity) WHERE e.name = 'X' must return the existing entity row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn entity_name_equality_handles_case_spaces_and_cjk() {
        let conn = fixture_db();
        let cases = [
            ("Mixed Case Entity", "e-fixture-spaces"),
            ("mixed case entity", "e-fixture-spaces"),
            ("知识 图谱", "e-fixture-cjk"),
        ];

        for (name, expected_id) in cases {
            let q = parse(
                QueryLanguage::Gql,
                &format!(r#"MATCH (e:entity) WHERE e.name = "{name}" RETURN e.id"#),
            )
            .unwrap();
            let compiled = compile(&q, &opts()).unwrap();
            assert!(
                compiled.sql.contains("COLLATE NOCASE"),
                "GQL string equality must remain case-insensitive; sql: {}",
                compiled.sql
            );
            assert!(
                compiled.sql.contains(".name = ?"),
                "GQL name equality must use a SQL placeholder; sql: {}",
                compiled.sql
            );
            assert!(
                compiled
                    .params
                    .iter()
                    .any(|param| matches!(param, QueryValue::Text(value) if value == name)),
                "name {name:?} must be passed as a bound text parameter; params: {:?}",
                compiled.params
            );
            assert!(
                !compiled.sql.contains(name),
                "name {name:?} must not be interpolated into SQL: {}",
                compiled.sql
            );
            assert_eq!(
                run(&conn, &compiled),
                vec![expected_id.to_string()],
                "name equality must find {name:?}; sql: {}",
                compiled.sql
            );
        }
    }

    #[test]
    fn note_substrate_label_compiles_without_unsatisfiable_kind_filter() {
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (n:note) WHERE n.name = 'X' RETURN n.id",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("substrate_kind = ?"),
            "substrate label 'note' must filter substrate_kind, not kind; sql: {}",
            compiled.sql
        );

        let conn = fixture_db();
        let rows = run(&conn, &compiled);
        assert_eq!(
            rows,
            vec!["n-fixture-1".to_string()],
            "MATCH (n:note) WHERE n.name = 'X' must return the existing note row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn granular_label_still_filters_kind_column() {
        let q = parse(QueryLanguage::Gql, "MATCH (e:concept) RETURN e.id").unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("n0.kind = ?"),
            "granular label 'concept' must still emit kind = ?; sql: {}",
            compiled.sql
        );
        let has_concept_param = compiled
            .params
            .iter()
            .any(|p| matches!(p, QueryValue::Text(s) if s == "concept"));
        assert!(has_concept_param, "params: {:?}", compiled.params);

        let conn = fixture_db();
        let rows = run(&conn, &compiled);
        assert_eq!(
            rows,
            vec!["e-fixture-1".to_string()],
            "MATCH (e:concept) must still return the concept-kind entity row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn sparql_entity_substrate_label_compiles_and_returns_row() {
        let q = parse(
            QueryLanguage::Sparql,
            "SELECT ?e WHERE { ?e a :entity . ?e :name \"X\" . ?e :extends ?c . }",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("substrate_kind = ?"),
            "SPARQL 'a :entity' must filter substrate_kind, not kind (frontend parity); sql: {}",
            compiled.sql
        );

        let conn = fixture_db_with_edge();
        let db_params: Vec<Box<dyn rusqlite::ToSql>> = compiled
            .params
            .iter()
            .map(|p| -> Box<dyn rusqlite::ToSql> {
                match p {
                    QueryValue::Null => Box::new(Option::<i64>::None),
                    QueryValue::Integer(n) => Box::new(*n),
                    QueryValue::Float(n) => Box::new(*n),
                    QueryValue::Text(s) => Box::new(s.clone()),
                    QueryValue::Blob(b) => Box::new(b.clone()),
                }
            })
            .collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> = db_params.iter().map(|b| b.as_ref()).collect();
        let mut stmt = conn.prepare(&compiled.sql).unwrap();
        let ids: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>("e_id"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            ids,
            vec!["e-fixture-1".to_string()],
            "SPARQL entity-substrate query must return the existing entity row; sql: {}",
            compiled.sql
        );
    }

    #[test]
    fn sparql_substrate_and_granular_kinds_filter_together_in_both_orders() {
        let conn = fixture_db_with_edge();
        conn.execute_batch(
            "INSERT INTO graph_edges
                (namespace, id, source_id, target_id, relation, weight,
                 created_at, updated_at, deleted_at, metadata, target_backend)
             VALUES
                ('local', 'edge-project', 'e-fixture-spaces', 'e-fixture-2',
                 'extends', 1.0, 0, 0, NULL, NULL, NULL);",
        )
        .unwrap();

        for relation in ["extends", "extends+"] {
            for kinds in [":entity . ?a a :concept", ":concept . ?a a :entity"] {
                let source = format!("SELECT ?a WHERE {{ ?a a {kinds} . ?a :{relation} ?b . }}");
                let query = parse(QueryLanguage::Sparql, &source).unwrap();
                let compiled = compile(&query, &opts()).unwrap();
                assert!(
                    compiled.sql.contains("substrate_kind = ?"),
                    "missing substrate filter for {source}: {}",
                    compiled.sql
                );
                assert!(
                    compiled.sql.contains(".kind = ?"),
                    "missing granular filter for {source}: {}",
                    compiled.sql
                );
                assert_eq!(
                    run(&conn, &compiled),
                    vec!["e-fixture-1"],
                    "only concept entities should match {source}: {}",
                    compiled.sql
                );
            }
        }
    }

    #[test]
    fn variable_length_entity_substrate_label_filters_substrate_kind() {
        let q = parse(
            QueryLanguage::Gql,
            "MATCH (a:entity)-[:extends*1..2]->(b) RETURN b LIMIT 5",
        )
        .unwrap();
        let compiled = compile(&q, &opts()).unwrap();
        assert!(
            compiled.sql.contains("s.substrate_kind = ?"),
            "variable-length seed with substrate label 'entity' must filter \
             s.substrate_kind, not s.kind; sql: {}",
            compiled.sql
        );
    }
}
