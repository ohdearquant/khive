use super::{analyze, analyze_template, Analysis, Target};

fn target(relation: &str, role: &str) -> Target {
    Target {
        relation: relation.to_owned(),
        role: role.to_owned(),
        dynamic: false,
    }
}

fn complete(sql: &str, targets: Vec<Target>) {
    assert_eq!(
        analyze(sql),
        Analysis {
            targets,
            statement: true,
            diagnostics: Vec::new(),
        },
        "{sql}"
    );
}

#[test]
fn dml_roles_and_query_payloads_are_structural_targets() {
    complete(
        "SELECT a.id FROM main.accounts AS a JOIN balances b ON b.id = a.id",
        vec![target("main.accounts", "from"), target("balances", "join")],
    );
    complete(
        "INSERT OR IGNORE INTO inbox(id) SELECT id FROM outgoing",
        vec![target("inbox", "insert"), target("outgoing", "from")],
    );
    complete(
        "INSERT INTO inbox DEFAULT VALUES",
        vec![target("inbox", "insert")],
    );
    complete(
        "UPDATE OR FAIL accounts AS a SET value=(SELECT value FROM balances) \
         FROM owners JOIN flags ON owners.id=flags.id",
        vec![
            target("accounts", "update"),
            target("balances", "from"),
            target("owners", "from"),
            target("flags", "join"),
        ],
    );
    complete(
        "DELETE FROM accounts WHERE id IN (SELECT id FROM revoked)",
        vec![target("accounts", "delete"), target("revoked", "from")],
    );
    complete(
        "REPLACE INTO inbox(id) VALUES(1)",
        vec![target("inbox", "replace")],
    );
    complete(
        "update accounts set value=1",
        vec![target("accounts", "update")],
    );
}

#[test]
fn ddl_objects_and_on_relations_have_distinct_roles() {
    for (sql, expected) in [
        (
            "CREATE TABLE IF NOT EXISTS main.records(id INTEGER)",
            vec![target("main.records", "create_table")],
        ),
        (
            "CREATE TEMP TABLE result AS SELECT id FROM records",
            vec![target("result", "create_table"), target("records", "from")],
        ),
        (
            "CREATE VIEW IF NOT EXISTS summary(id) AS SELECT id FROM records",
            vec![target("summary", "create_view"), target("records", "from")],
        ),
        (
            "CREATE VIRTUAL TABLE IF NOT EXISTS records_fts USING fts5(content)",
            vec![target("records_fts", "create_virtual_table")],
        ),
        (
            "CREATE UNIQUE INDEX IF NOT EXISTS records_id ON records(id)",
            vec![
                target("records_id", "create_index"),
                target("records", "on"),
            ],
        ),
        (
            "CREATE TRIGGER audit_write AFTER UPDATE ON records \
             BEGIN INSERT INTO audit_log VALUES(new.id); \
             UPDATE counters SET n=n+1; END;",
            vec![
                target("audit_write", "create_trigger"),
                target("records", "on"),
                target("audit_log", "insert"),
                target("counters", "update"),
            ],
        ),
        (
            "ALTER TABLE records RENAME COLUMN old_name TO new_name",
            vec![target("records", "alter_table")],
        ),
        (
            "DROP TABLE IF EXISTS records",
            vec![target("records", "drop_table")],
        ),
        ("DROP VIEW summary", vec![target("summary", "drop_view")]),
        (
            "DROP INDEX records_id",
            vec![target("records_id", "drop_index")],
        ),
        (
            "DROP TRIGGER audit_write",
            vec![target("audit_write", "drop_trigger")],
        ),
    ] {
        complete(sql, expected);
    }
}

#[test]
fn cte_and_derived_aliases_do_not_become_physical_targets() {
    complete(
        "WITH RECURSIVE selected(id) AS NOT MATERIALIZED \
         (SELECT id FROM records UNION ALL SELECT id FROM selected), \
         final AS (SELECT id FROM selected JOIN future ON selected.id=future.id), \
         future AS (SELECT id FROM flags) \
         SELECT id FROM final JOIN main.selected ON final.id=main.selected.id",
        vec![
            target("records", "from"),
            target("flags", "from"),
            target("main.selected", "join"),
        ],
    );
    complete(
        "SELECT d.id FROM (SELECT id FROM accounts) AS d \
         JOIN (WITH x AS (SELECT id FROM flags) SELECT id FROM x) e ON d.id=e.id",
        vec![target("accounts", "from"), target("flags", "from")],
    );
    complete(
        "SELECT id FROM accounts, balances WHERE EXISTS \
         (SELECT 1 FROM revoked WHERE revoked.id=accounts.id)",
        vec![
            target("accounts", "from"),
            target("balances", "from"),
            target("revoked", "from"),
        ],
    );
}

#[test]
fn lower_case_cte_sql_retains_its_physical_targets() {
    complete(
        "with chosen as (select id from records) select id from chosen",
        vec![target("records", "from")],
    );
}

#[test]
fn quoted_schema_names_values_and_comments_are_not_confused() {
    complete(
        r#"SELECT 'FROM decoy; JOIN ignored', "value" FROM "ma""in"."记录"
            JOIN `odd``name` ON 1 JOIN [brack]]et] ON 1 -- FROM hidden
            /* UPDATE hidden SET value=0 */"#,
        vec![
            target("ma\"in.记录", "from"),
            target("odd`name", "join"),
            target("brack]et", "join"),
        ],
    );
    complete(
        r#"SELECT id FROM "SELECT" JOIN "123" ON 1"#,
        vec![target("SELECT", "from"), target("123", "join")],
    );
    complete(
        "SELECT * FROM json_each('[1,2]') JOIN records ON 1",
        vec![target("json_each", "from"), target("records", "join")],
    );
}

#[test]
fn known_dynamic_relations_are_explicit_and_assembled_fragments_are_partial() {
    complete(
        "SELECT id FROM main.{table} JOIN {table_name} ON 1",
        vec![
            Target {
                relation: "main.{table}".to_owned(),
                role: "from".to_owned(),
                dynamic: true,
            },
            Target {
                relation: "{table_name}".to_owned(),
                role: "join".to_owned(),
                dynamic: true,
            },
        ],
    );
    for sql in [
        "SELECT id FROM {opaque}",
        "SELECT id FROM prefix_{table}",
        "SELECT id FROM {table}_suffix",
        "{verb} id FROM records",
        "{prefix} SELECT 1",
    ] {
        let analysis = analyze(sql);
        assert!(analysis.targets.is_empty(), "{sql}: {analysis:?}");
        assert!(!analysis.diagnostics.is_empty(), "{sql}: {analysis:?}");
        assert!(
            analysis
                .diagnostics
                .iter()
                .all(|error| error.starts_with("at byte ")),
            "{sql}: {analysis:?}"
        );
    }
}

#[test]
fn malformed_or_unsupported_sql_does_not_certify_a_zero_target_result() {
    for sql in [
        "SELECT id FROM 123",
        "SELECT id FROM $parameter",
        "SELECT id FROM ?1",
        "SELECT id FROM WHERE",
        "SELECT FROM records",
        "SELECT 1 JOIN records ON 1",
        "UPDATE SET ready",
        "update SET ready",
        "INSERT INTO records",
        "INSERT INTO records VALUES",
        "INSERT INTO records DEFAULT",
        "INSERT OR nonsense INTO records VALUES(1)",
        "DELETE records",
        "CREATE TABLE records",
        "CREATE VIEW summary AS prose",
        "CREATE INDEX idx ON records",
        "CREATE TRIGGER audit AFTER UPDATE ON records",
        "ALTER TABLE records",
        "WITH chosen AS (SELECT 1)",
        "with chosen as select 1",
        "WITH chosen AS (prose) SELECT 1",
        "SELECT id FROM (accounts JOIN flags ON 1)",
        "SELECT id FROM 'records'",
        "SELECT id FROM records AS WHERE",
        "SELECT id FROM \"records",
        "SELECT id FROM records /* unfinished",
        "SELECT (id FROM records",
        "SELECT id FROM records)",
        "; SELECT id FROM records /* unfinished",
        "PRAGMA 123",
        "EXPLAIN QUERY PLAN SELECT id FROM records",
        "ATTACH DATABASE 'archive.db' AS archive",
    ] {
        let analysis = analyze(sql);
        assert!(!analysis.diagnostics.is_empty(), "{sql}: {analysis:?}");
    }
    // Diagnostic incompleteness does not erase independently recognized targets.
    let partial = analyze("SELECT id FROM records; ATTACH DATABASE 'x' AS x");
    assert_eq!(partial.targets, vec![target("records", "from")]);
    assert!(partial.statement);
    assert!(!partial.diagnostics.is_empty());
}

#[test]
fn format_context_distinguishes_quoted_placeholders_from_literal_braces() {
    let quoted = r#"SELECT id FROM "{table}" JOIN "main"."{table_name}" ON 1"#;
    complete(
        quoted,
        vec![
            target("{table}", "from"),
            target("main.{table_name}", "join"),
        ],
    );
    assert_eq!(
        analyze_template(quoted),
        Analysis {
            targets: vec![
                Target {
                    relation: "{table}".to_owned(),
                    role: "from".to_owned(),
                    dynamic: true,
                },
                Target {
                    relation: "main.{table_name}".to_owned(),
                    role: "join".to_owned(),
                    dynamic: true,
                },
            ],
            statement: true,
            diagnostics: Vec::new(),
        }
    );
    assert_eq!(
        analyze_template(r#"SELECT id FROM "{{table}}""#),
        Analysis {
            targets: vec![target("{table}", "from")],
            statement: true,
            diagnostics: Vec::new(),
        }
    );
    complete(
        r#"SELECT id FROM "{{table}}""#,
        vec![target("{{table}}", "from")],
    );
    for sql in [
        r#"SELECT id FROM "prefix_{table}""#,
        r#"SELECT id FROM "{unknown}""#,
        r#"SELECT id FROM "{table:?}""#,
        r#"SELECT id FROM "{table}}""#,
        "SELECT id FROM {{table}}",
    ] {
        let analysis = analyze_template(sql);
        assert!(analysis.targets.is_empty(), "{sql}: {analysis:?}");
        assert!(!analysis.diagnostics.is_empty(), "{sql}: {analysis:?}");
    }
}

#[test]
fn tableless_sql_and_non_sql_populations_remain_separate() {
    for sql in ["SELECT 1", "PRAGMA main.user_version", "VALUES(1)"] {
        complete(sql, Vec::new());
    }
    for text in [
        "",
        "can't open database",
        "reader query failed",
        "update the release note",
        "with reason for the change",
        "-- SELECT id FROM hidden",
        "/* UPDATE hidden SET n=0 */",
        "// SELECT id FROM hidden",
        "macro_rules! sql { ($name:ident) => {} }",
    ] {
        assert_eq!(analyze(text), Analysis::default(), "{text}");
    }
    complete(
        "SELECT '; FROM hidden'; PRAGMA user_version; \
         SELECT id FROM records; DELETE FROM expired;",
        vec![target("records", "from"), target("expired", "delete")],
    );
}

#[test]
fn nesting_limit_is_a_diagnostic_instead_of_an_unbounded_recursive_walk() {
    let sql = format!("SELECT {}1{}", "(".repeat(70), ")".repeat(70));
    let analysis = analyze(&sql);
    assert!(analysis.statement);
    assert!(analysis.targets.is_empty());
    assert!(analysis
        .diagnostics
        .iter()
        .any(|error| error.contains("nesting")));
}

#[test]
fn opaque_template_segments_preserve_known_targets_and_report_uncertainty() {
    for sql in [
        "{prefix}; SELECT * FROM notes",
        "{}; SELECT * FROM notes",
        "SELECT * FROM notes; {suffix}",
        "SELECT * FROM notes; {}",
    ] {
        let analysis = analyze_template(sql);
        assert!(analysis.statement, "{sql}: {analysis:?}");
        assert_eq!(
            analysis.targets,
            vec![target("notes", "from")],
            "{sql}: {analysis:?}"
        );
        assert_eq!(analysis.diagnostics.len(), 1, "{sql}: {analysis:?}");
        assert_eq!(
            analysis.diagnostics[0],
            format!(
                "at byte {}: opaque assembled SQL fragment",
                sql.find('{').unwrap()
            )
        );
    }
    for text in [
        "{prefix} ordinary label",
        "{}; ordinary label",
        "ordinary label {suffix}",
    ] {
        assert_eq!(analyze_template(text), Analysis::default(), "{text}");
    }
}
