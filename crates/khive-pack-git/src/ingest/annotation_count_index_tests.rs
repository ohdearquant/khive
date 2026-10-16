use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

const CANONICAL: &str = "idx_git_notes_history_canonical_sha";
const NONCANONICAL: &str = "idx_git_notes_history_noncanonical";
const OLD_COUNT: &str = "SELECT COUNT(*) AS holders FROM notes WHERE namespace=?1 AND kind='commit' AND json_type(properties,'$.sha')='text' AND json_extract(properties,'$.sha')=?2 COLLATE BINARY";
const INSERT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../khive-db/sql/commit-annotation-insert.sql"
));
const SOURCE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../khive-db/sql/commit-annotation-source-live-select.sql"
));
const OLD_SOURCE: &str = "SELECT EXISTS(SELECT 1 FROM notes WHERE id=?1 AND namespace=?2 AND kind='commit' AND deleted_at IS NULL AND json_type(properties,'$.sha')='text' AND json_extract(properties,'$.sha')=?3 COLLATE BINARY AND (SELECT COUNT(*) FROM notes WHERE namespace=?2 AND kind='commit' AND json_type(properties,'$.sha')='text' AND json_extract(properties,'$.sha')=?3 COLLATE BINARY)=1)";

#[derive(Clone, Copy, Debug)]
enum Schema {
    Absent,
    Both,
    CanonicalOnly,
    NoncanonicalOnly,
    Aliases,
    WrongTable,
    WrongDefinitions,
}
const SCHEMAS: [Schema; 7] = [
    Schema::Absent,
    Schema::Both,
    Schema::CanonicalOnly,
    Schema::NoncanonicalOnly,
    Schema::Aliases,
    Schema::WrongTable,
    Schema::WrongDefinitions,
];

fn history_ddl(name: &str) -> &str {
    GIT_CORE_INDEXES
        .split(';')
        .find(|sql| sql.contains(name))
        .unwrap()
}

fn configure(runtime: &KhiveRuntime, schema: Schema) {
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer
        .conn()
        .execute_batch(&format!(
            "DROP INDEX IF EXISTS {CANONICAL}; DROP INDEX IF EXISTS {NONCANONICAL}"
        ))
        .unwrap();
    if matches!(schema, Schema::Absent) {
        return;
    }
    writer.conn().execute_batch(GIT_CORE_INDEXES).unwrap();
    match schema {
        Schema::CanonicalOnly => writer
            .conn()
            .execute_batch(&format!("DROP INDEX {NONCANONICAL}"))
            .unwrap(),
        Schema::NoncanonicalOnly => writer
            .conn()
            .execute_batch(&format!("DROP INDEX {CANONICAL}"))
            .unwrap(),
        Schema::Aliases | Schema::WrongTable | Schema::WrongDefinitions => {
            writer
                .conn()
                .execute_batch(&format!(
                    "DROP INDEX {CANONICAL}; DROP INDEX {NONCANONICAL}"
                ))
                .unwrap();
            match schema {
                Schema::Aliases => {
                    for ddl in [
                        history_ddl(CANONICAL),
                        history_ddl(NONCANONICAL),
                    ] {
                        writer.conn().execute_batch(&ddl.replace("idx_git_notes_history_", "idx_fixture_alias_")).unwrap();
                    }
                }
                Schema::WrongTable => writer.conn().execute_batch(&format!(
                    "CREATE INDEX {CANONICAL} ON entities(namespace); CREATE INDEX {NONCANONICAL} ON entities(kind)"
                )).unwrap(),
                Schema::WrongDefinitions => writer.conn().execute_batch(&format!(
                    "CREATE INDEX {CANONICAL} ON notes(namespace); CREATE INDEX {NONCANONICAL} ON notes(kind)"
                )).unwrap(),
                _ => unreachable!(),
            }
        }
        Schema::Absent | Schema::Both => {}
    }
}

fn raw_note(
    runtime: &KhiveRuntime,
    id: &str,
    ns: &str,
    kind: &str,
    value: SqlValue,
    deleted: bool,
) {
    let writer = runtime.backend().pool().try_writer().unwrap();
    let sql = "INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,?4,1,1,?5)";
    let deleted = deleted.then_some(1_i64);
    match value {
        SqlValue::Text(value) => writer.conn().execute(sql, (id, ns, kind, value, deleted)),
        SqlValue::Blob(value) => writer.conn().execute(sql, (id, ns, kind, value, deleted)),
        SqlValue::Null => writer
            .conn()
            .execute(sql, (id, ns, kind, None::<&str>, deleted)),
        other => panic!("fixture property representation: {other:?}"),
    }
    .unwrap();
}

fn jsonb(runtime: &KhiveRuntime, json: &str) -> SqlValue {
    let writer = runtime.backend().pool().try_writer().unwrap();
    let bytes: Vec<u8> = writer
        .conn()
        .query_row("SELECT jsonb(?1)", [json], |row| row.get(0))
        .unwrap();
    SqlValue::Blob(bytes)
}

async fn observed(
    runtime: &KhiveRuntime,
    sql: &str,
    params: Vec<SqlValue>,
) -> Result<Value, String> {
    let mut reader = runtime.sql().reader().await.unwrap();
    reader
        .query_all(SqlStatement {
            sql: sql.into(),
            params,
            label: Some("annotation_count_exact_parity".into()),
        })
        .await
        .map(|rows| snapshot(&rows))
        .map_err(|error| error.to_string())
}

// EXISTS returns one typed integer; expression-derived column labels are not
// semantic results and differ between the current and original SQL text.
async fn observed_source_scalar(
    runtime: &KhiveRuntime,
    sql: &str,
    params: Vec<SqlValue>,
) -> Result<i64, String> {
    let mut reader = runtime.sql().reader().await.unwrap();
    let rows = reader
        .query_all(SqlStatement {
            sql: sql.into(),
            params,
            label: Some("annotation_source_scalar_parity".into()),
        })
        .await
        .map_err(|error| error.to_string())?;
    assert_eq!(rows.len(), 1, "source predicate row count: {rows:?}");
    assert_eq!(
        rows[0].columns.len(),
        1,
        "source predicate columns: {rows:?}"
    );
    match &rows[0].columns[0].value {
        SqlValue::Integer(value) => Ok(*value),
        other => panic!("source predicate must return an SQL integer: {other:?}"),
    }
}

fn old_insert() -> String {
    let (before, rest) = INSERT.split_once("AND 1 = (\n").unwrap();
    let (_, after) = rest.split_once("\n)\nAND EXISTS").unwrap();
    format!(
        "{before}AND 1 = (\n{}\n)\nAND EXISTS{after}",
        OLD_COUNT.replace(" AS holders", "").replace("?2", "?9")
    )
}

fn insert_edge(
    runtime: &KhiveRuntime,
    sql: &str,
    source: &str,
    target: &str,
    cursor: bool,
) -> Result<usize, String> {
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer
        .conn()
        .execute(
            sql,
            (
                "local",
                "candidate-edge",
                source,
                target,
                1.0,
                1_i64,
                1_i64,
                "{}",
                "match",
                "owner/repo",
                cursor,
            ),
        )
        .map_err(|error| error.to_string())
}

fn seed_history(runtime: &KhiveRuntime, from: usize, to: usize) {
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer.transaction(|conn| {
        for i in from..to {
            conn.execute("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at,deleted_at) VALUES(?1,'local','commit',?2,1,1,?3)", (format!("ordinary-{i}"), format!("{{\"sha\":\"ordinary-{i}\"}}"), (i % 2 == 0).then_some(1_i64)))?;
        }
        Ok(())
    }).unwrap();
}

fn instruction_work(runtime: &KhiveRuntime, sql: &str) -> u64 {
    let work = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&work);
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer
        .conn()
        .progress_handler(
            1,
            Some(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
    let result: Result<i64, _> = writer
        .conn()
        .query_row(sql, ["local", "match"], |row| row.get(0));
    writer
        .conn()
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert_eq!(result.unwrap(), 1);
    work.load(Ordering::Relaxed)
}

#[tokio::test]
async fn actual_file_count_and_source_plans_search_both_history_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = file_runtime(&dir.path().join("plans.db"), false);
    raw_note(
        &runtime,
        "source",
        "local",
        "commit",
        SqlValue::Text(r#"{"sha":"match"}"#.into()),
        false,
    );
    raw_note(
        &runtime,
        "legacy",
        "local",
        "commit",
        SqlValue::Text("{sha:'legacy'}".into()),
        true,
    );
    seed_history(&runtime, 0, 500);
    for analyzed in [false, true] {
        if analyzed {
            runtime
                .backend()
                .pool()
                .try_writer()
                .unwrap()
                .conn()
                .execute_batch("ANALYZE")
                .unwrap();
        }
        for sha in ["match", "legacy"] {
            assert_eq!(annotation_count(&runtime, sha).await, 1);
            let actual = plan(&runtime, &annotation_count_sql(), sha_params(sha)).await;
            assert!(uses(&actual, CANONICAL), "{actual:?}");
            assert!(
                actual
                    .iter()
                    .any(|line| line.contains(CANONICAL) && line.contains("<expr>=?")),
                "SHA must constrain the expression key: {actual:?}"
            );
            assert!(
                uses(&actual, NONCANONICAL),
                "namespace/kind legacy bucket: {actual:?}"
            );
            println!("HISTORY_PLAN analyzed={analyzed} sha={sha} {actual:?}");
        }
        let source = plan(
            &runtime,
            SOURCE,
            vec![
                SqlValue::Text("source".into()),
                SqlValue::Text("local".into()),
                SqlValue::Text("match".into()),
            ],
        )
        .await;
        assert!(
            uses(&source, CANONICAL) && uses(&source, NONCANONICAL),
            "failure classification must use both partitions: {source:?}"
        );
    }
}

#[tokio::test]
async fn partition_preserves_types_namespaces_tombstones_json5_jsonb_and_bindings() {
    for schema in SCHEMAS {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("types.db"), false);
        for (id, ns, kind, value, deleted) in [
            (
                "text",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":"match"}"#.into()),
                false,
            ),
            (
                "deleted",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":"match"}"#.into()),
                true,
            ),
            (
                "json5",
                "local",
                "commit",
                SqlValue::Text("{sha:'match',}".into()),
                true,
            ),
            (
                "jsonb",
                "local",
                "commit",
                jsonb(&runtime, r#"{"sha":"match"}"#),
                true,
            ),
            (
                "blob-text",
                "local",
                "commit",
                SqlValue::Blob(br#"{"sha":"match"}"#.to_vec()),
                true,
            ),
            (
                "upper",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":"MATCH"}"#.into()),
                true,
            ),
            (
                "upper-json5",
                "local",
                "commit",
                SqlValue::Text("{sha:'MATCH'}".into()),
                true,
            ),
            (
                "integer",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":123}"#.into()),
                true,
            ),
            (
                "float",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":123.0}"#.into()),
                true,
            ),
            (
                "text-number",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":"123"}"#.into()),
                true,
            ),
            (
                "missing",
                "local",
                "commit",
                SqlValue::Text("{}".into()),
                true,
            ),
            ("null", "local", "commit", SqlValue::Null, true),
            (
                "sha-null",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":null}"#.into()),
                true,
            ),
            (
                "array",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":["match"]}"#.into()),
                true,
            ),
            (
                "bool",
                "local",
                "commit",
                SqlValue::Text(r#"{"sha":true}"#.into()),
                true,
            ),
            (
                "nan",
                "local",
                "commit",
                SqlValue::Text("{sha:NaN}".into()),
                true,
            ),
            (
                "foreign",
                "other",
                "commit",
                SqlValue::Text(r#"{"sha":"match"}"#.into()),
                true,
            ),
            (
                "issue",
                "local",
                "issue",
                SqlValue::Text(r#"{"sha":"match"}"#.into()),
                true,
            ),
        ] {
            raw_note(&runtime, id, ns, kind, value, deleted);
        }
        configure(&runtime, schema);
        assert_eq!(annotation_count(&runtime, "match").await, 5, "{schema:?}");
        assert_eq!(annotation_count(&runtime, "123").await, 1, "{schema:?}");
        for namespace in [
            SqlValue::Text("local".into()),
            SqlValue::Text("other".into()),
            SqlValue::Text("LOCAL".into()),
            SqlValue::Null,
            SqlValue::Integer(123),
        ] {
            for sha in [
                SqlValue::Text("match".into()),
                SqlValue::Text("MATCH".into()),
                SqlValue::Text("123".into()),
                SqlValue::Integer(123),
                SqlValue::Float(123.0),
                SqlValue::Bool(true),
                SqlValue::Null,
                SqlValue::Blob(b"match".to_vec()),
            ] {
                let params = vec![namespace.clone(), sha];
                assert_eq!(
                    observed(&runtime, &annotation_count_sql(), params.clone()).await,
                    observed(&runtime, OLD_COUNT, params.clone()).await,
                    "{schema:?} {params:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn whole_insert_and_failure_classification_preserve_holder_and_guard_outcomes() {
    for schema in SCHEMAS {
        for history in [
            "unique",
            "duplicate",
            "json5",
            "jsonb",
            "deleted-source",
            "wrong-type",
            "existing-live",
            "existing-deleted",
        ] {
            for cursor in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let runtime = file_runtime(&dir.path().join("insert.db"), false);
                raw_note(
                    &runtime,
                    "source",
                    "local",
                    "commit",
                    SqlValue::Text(
                        if history == "wrong-type" {
                            r#"{"sha":123}"#
                        } else {
                            r#"{"sha":"match"}"#
                        }
                        .into(),
                    ),
                    history == "deleted-source",
                );
                if matches!(history, "duplicate" | "json5" | "jsonb") {
                    let value = match history {
                        "json5" => SqlValue::Text("{sha:'match'}".into()),
                        "jsonb" => jsonb(&runtime, r#"{"sha":"match"}"#),
                        _ => SqlValue::Text(r#"{"sha":"match"}"#.into()),
                    };
                    raw_note(&runtime, "duplicate", "local", "commit", value, true);
                }
                runtime.backend().pool().try_writer().unwrap().conn().execute("INSERT INTO entities(id,namespace,kind,name,properties,created_at,updated_at) VALUES('project','local','project','p','{\"repo_slug\":\"owner/repo\"}',1,1)", []).unwrap();
                if matches!(history, "existing-live" | "existing-deleted") {
                    runtime.backend().pool().try_writer().unwrap().conn().execute("INSERT INTO graph_edges(id,namespace,source_id,target_id,relation,weight,created_at,updated_at,deleted_at,metadata) VALUES('curated','local','source','project','annotates',0.42,7,8,?1,'{\"curated\":true}')", [if history == "existing-deleted" { Some(9_i64) } else { None }]).unwrap();
                }
                let curated = snapshot(
                    &query(
                        &runtime,
                        "SELECT * FROM graph_edges WHERE id='curated'",
                        vec![],
                    )
                    .await,
                );
                configure(&runtime, schema);
                let expected = usize::from(history == "unique" && cursor);
                let old =
                    insert_edge(&runtime, &old_insert(), "source", "project", cursor).unwrap();
                assert_eq!(old, expected, "old {schema:?} {history} {cursor}");
                runtime
                    .backend()
                    .pool()
                    .try_writer()
                    .unwrap()
                    .conn()
                    .execute("DELETE FROM graph_edges WHERE id='candidate-edge'", [])
                    .unwrap();
                let new = insert_edge(&runtime, INSERT, "source", "project", cursor).unwrap();
                assert_eq!(new, old, "whole INSERT {schema:?} {history} {cursor}");
                assert_eq!(
                    snapshot(
                        &query(
                            &runtime,
                            "SELECT * FROM graph_edges WHERE id='curated'",
                            vec![]
                        )
                        .await
                    ),
                    curated
                );
                let params = vec![
                    SqlValue::Text("source".into()),
                    SqlValue::Text("local".into()),
                    SqlValue::Text("match".into()),
                ];
                let actual = observed_source_scalar(&runtime, SOURCE, params.clone()).await;
                assert_eq!(
                    actual,
                    observed_source_scalar(&runtime, OLD_SOURCE, params.clone()).await,
                    "source refusal {schema:?} {history}"
                );
                let rows = query(&runtime, SOURCE, params).await;
                assert!(
                    matches!(&rows[0].columns[0].value, SqlValue::Integer(value) if *value == i64::from(matches!(history, "unique" | "existing-live" | "existing-deleted"))),
                    "source count must reject deleted duplicates: {rows:?}"
                );
                for (source, target) in [("missing", "project"), ("source", "missing")] {
                    assert_eq!(
                        insert_edge(&runtime, INSERT, source, target, true).unwrap(),
                        0
                    );
                }
                let params = vec![
                    SqlValue::Text("source".into()),
                    SqlValue::Text("other".into()),
                    SqlValue::Text("match".into()),
                ];
                assert_eq!(
                    observed_source_scalar(&runtime, SOURCE, params.clone()).await,
                    observed_source_scalar(&runtime, OLD_SOURCE, params).await
                );
                let events = query(
                    &runtime,
                    "SELECT COUNT(*) AS n FROM events WHERE kind='link_created'",
                    vec![],
                )
                .await;
                assert_eq!(events[0].i64("n").unwrap(), 0, "raw SQL must not fabricate audit events; retained runtime fixtures check Created emits once");
            }
        }
    }
}

#[tokio::test]
async fn malformed_deleted_history_installs_safely_and_keeps_exact_count_and_write_errors() {
    for schema in SCHEMAS {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("malformed.db"), false);
        // Model legacy history unavailable under current core JSON indexes.
        let indexes = query(&runtime, "SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='notes' AND (sql LIKE '%json_extract%' OR sql LIKE '%json_type%')", vec![]).await;
        assert!(!indexes.is_empty());
        {
            let writer = runtime.backend().pool().try_writer().unwrap();
            for row in indexes {
                writer
                    .conn()
                    .execute_batch(&format!(
                        "DROP INDEX \"{}\"",
                        row.text("name").unwrap().replace('"', "\"\"")
                    ))
                    .unwrap();
            }
        }
        raw_note(
            &runtime,
            "source",
            "local",
            "commit",
            SqlValue::Text(r#"{"sha":"match"}"#.into()),
            false,
        );
        for (id, ns, kind) in [
            ("bad-local", "local", "commit"),
            ("bad-other", "other", "commit"),
            ("bad-issue", "local", "issue"),
        ] {
            raw_note(&runtime, id, ns, kind, SqlValue::Text("{".into()), true);
        }
        runtime.backend().pool().try_writer().unwrap().conn().execute("INSERT INTO entities(id,namespace,kind,name,properties,created_at,updated_at) VALUES('project','local','project','p','{\"repo_slug\":\"owner/repo\"}',1,1)", []).unwrap();
        let before = snapshot(
            &query(
                &runtime,
                "SELECT id,properties,deleted_at FROM notes ORDER BY id",
                vec![],
            )
            .await,
        );
        configure(&runtime, schema);
        for namespace in ["local", "other", "absent"] {
            let params = vec![
                SqlValue::Text(namespace.into()),
                SqlValue::Text("match".into()),
            ];
            let old = observed(&runtime, OLD_COUNT, params.clone()).await;
            let new = observed(&runtime, &annotation_count_sql(), params).await;
            assert_eq!(new, old, "{schema:?} {namespace}");
            if namespace != "absent" {
                assert!(new.unwrap_err().contains("malformed JSON"));
            }
        }
        let source_params = vec![
            SqlValue::Text("source".into()),
            SqlValue::Text("local".into()),
            SqlValue::Text("match".into()),
        ];
        let old = observed_source_scalar(&runtime, OLD_SOURCE, source_params.clone()).await;
        assert!(old.as_ref().unwrap_err().contains("malformed JSON"));
        assert_eq!(
            observed_source_scalar(&runtime, SOURCE, source_params).await,
            old
        );
        let old = insert_edge(&runtime, &old_insert(), "source", "project", true).unwrap_err();
        assert!(old.contains("malformed JSON"));
        assert_eq!(
            insert_edge(&runtime, INSERT, "source", "project", true).unwrap_err(),
            old
        );
        assert_eq!(
            snapshot(
                &query(
                    &runtime,
                    "SELECT id,properties,deleted_at FROM notes ORDER BY id",
                    vec![]
                )
                .await
            ),
            before
        );
        assert_eq!(
            query(&runtime, "SELECT id FROM graph_edges", vec![])
                .await
                .len(),
            0
        );
        runtime
            .backend()
            .pool()
            .try_writer()
            .unwrap()
            .conn()
            .execute("DELETE FROM notes WHERE id='bad-local'", [])
            .unwrap();
        assert_eq!(
            annotation_count(&runtime, "match").await,
            1,
            "other namespace/kind malformed rows must not poison local count"
        );
    }
}

#[tokio::test]
async fn instruction_growth_avoids_ordinary_history_and_absent_pack_double_scan() {
    for schema in SCHEMAS {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("work.db"), false);
        raw_note(
            &runtime,
            "source",
            "local",
            "commit",
            SqlValue::Text(r#"{"sha":"match"}"#.into()),
            false,
        );
        // Keep a real noncanonical bucket of unrelated history in each fixture.
        raw_note(
            &runtime,
            "legacy",
            "local",
            "commit",
            SqlValue::Text("{sha:'legacy'}".into()),
            true,
        );
        configure(&runtime, schema);
        let mut work = Vec::new();
        for (from, to) in [(0, 500), (500, 5_000)] {
            seed_history(&runtime, from, to);
            work.push((
                instruction_work(&runtime, OLD_COUNT),
                instruction_work(&runtime, &annotation_count_sql()),
            ));
        }
        let old_growth = work[1].0 - work[0].0;
        assert!(
            work[1].0 > work[0].0 * 5,
            "baseline must expose history scan: {schema:?} {work:?}"
        );
        match schema {
            Schema::Both => assert!(
                work[1].1 <= work[0].1 * 2,
                "ordinary history must not scale indexed work: {work:?}"
            ),
            Schema::WrongDefinitions => {} // Names establish readiness, not valid plans or a work bound.
            _ => {
                let gated_growth = work[1].1 - work[0].1;
                assert!(gated_growth >= old_growth * 4 / 5 && gated_growth <= old_growth * 6 / 5, "fallback must retain one original history scan plus fixed catalog cost: {schema:?} {work:?}");
            }
        }
        println!("HISTORY_WORK schema={schema:?} counts=500,5000 work={work:?}");
        if matches!(schema, Schema::Both) {
            runtime
                .backend()
                .pool()
                .try_writer()
                .unwrap()
                .conn()
                .execute_batch("ANALYZE")
                .unwrap();
            let analyzed = instruction_work(&runtime, &annotation_count_sql());
            assert!(
                analyzed <= work[0].1 * 2,
                "ANALYZE must retain bounded ordinary-history work: {work:?} {analyzed}"
            );
        }
    }
}

#[tokio::test]
async fn core_indexes_reopen_read_only_idempotently_and_auxiliary_batch_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.db");
    let runtime = file_runtime(&path, false);
    raw_note(
        &runtime,
        "source",
        "local",
        "commit",
        SqlValue::Text(r#"{"sha":"match"}"#.into()),
        true,
    );
    let mut statements = crate::GitPack::new(runtime.clone())
        .schema_plan()
        .statements
        .to_vec();
    statements.push("CREATE INDEX idx_fixture_late_refusal ON notes(missing_fixture_column)");
    let error = runtime
        .backend()
        .apply_pack_ddl_statements(&statements)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no such column"), "{error}");
    let catalog = "SELECT name,sql FROM sqlite_schema WHERE name IN ('idx_git_notes_live_commit_sha','idx_git_notes_live_number_project','idx_git_notes_history_canonical_sha','idx_git_notes_history_noncanonical','git_mirror_cursor','git_receipts') ORDER BY name";
    let core_before = query(&runtime, catalog, vec![]).await;
    assert_eq!(
        core_before.len(),
        4,
        "all core indexes precede pack loading"
    );
    assert!(core_before.iter().all(|row| matches!(row.get("name"), Some(SqlValue::Text(name)) if name.starts_with("idx_git_notes_"))), "pack auxiliary tables must roll back: {core_before:?}");
    install(&runtime);
    let first = snapshot(&query(&runtime, catalog, vec![]).await);
    assert_eq!(first.as_array().unwrap().len(), 6);
    install(&runtime);
    assert_eq!(snapshot(&query(&runtime, catalog, vec![]).await), first);
    close_file_runtime(runtime).await;
    let runtime = file_runtime(&path, false);
    install(&runtime);
    assert_eq!(annotation_count(&runtime, "match").await, 1);
    assert_eq!(snapshot(&query(&runtime, catalog, vec![]).await), first);
    close_file_runtime(runtime).await;
    assert!(!path.with_extension("db-wal").exists() && !path.with_extension("db-shm").exists());
    let readonly = file_runtime(&path, true);
    assert!(readonly.backend().is_read_only());
    assert_eq!(annotation_count(&readonly, "match").await, 1);
    assert_eq!(snapshot(&query(&readonly, catalog, vec![]).await), first);
    let actual = plan(&readonly, &annotation_count_sql(), sha_params("match")).await;
    assert!(
        uses(&actual, CANONICAL) && uses(&actual, NONCANONICAL),
        "{actual:?}"
    );
}

#[test]
fn git_schema_plan_contains_only_auxiliary_table_statements() {
    fn names_notes(sql: &str) -> bool {
        sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|token| token.eq_ignore_ascii_case("notes"))
    }
    // This is exactly the former live-commit statement, now in core migration 049.
    let known_core_statement = history_ddl(SHA_INDEX);
    assert!(
        names_notes(known_core_statement),
        "detector must match the old control row"
    );
    let plan = &crate::vocab::GIT_SCHEMA_PLAN_STMTS;
    assert_eq!(plan.len(), 5);
    assert!(
        plan.iter().all(|sql| !names_notes(sql)),
        "core notes DDL escaped into a pack plan: {plan:?}"
    );
    assert!(plan
        .iter()
        .any(|sql| sql.contains("CREATE TABLE IF NOT EXISTS git_receipts")));
    assert!(plan
        .iter()
        .any(|sql| sql.contains("CREATE TABLE IF NOT EXISTS git_mirror_cursor")));
}

#[tokio::test]
async fn low_level_readonly_old_snapshot_reaches_original_count_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old-snapshot.db");
    let runtime = file_runtime(&path, false);
    raw_note(
        &runtime,
        "source",
        "local",
        "commit",
        SqlValue::Text(r#"{"sha":"match"}"#.into()),
        false,
    );
    configure_property_indexes(&runtime, false);
    runtime
        .backend()
        .pool()
        .try_writer()
        .unwrap()
        .conn()
        .execute("DELETE FROM _schema_migrations WHERE version>=49", [])
        .unwrap();
    close_file_runtime(runtime).await;
    // The helper calls the real read-only backend constructor and from_backend;
    // neither validates or migrates core schema, unlike serving boot.
    let readonly = file_runtime(&path, true);
    assert!(readonly.backend().is_read_only());
    assert!(
        readonly.backend().prepare_core_schema().is_err(),
        "serving preparation must refuse this older schema"
    );
    let ledger = query(
        &readonly,
        "SELECT MAX(version) AS version FROM _schema_migrations",
        vec![],
    )
    .await;
    assert!(matches!(
        ledger[0].get("version"),
        Some(SqlValue::Integer(48))
    ));
    let catalog = query(
        &readonly,
        "SELECT name FROM sqlite_master WHERE name LIKE 'idx_git_notes_%'",
        vec![],
    )
    .await;
    assert!(
        catalog.is_empty(),
        "read-only low-level assembly must not create indexes"
    );
    assert_eq!(annotation_count(&readonly, "match").await, 1);
    assert_eq!(
        observed(&readonly, &annotation_count_sql(), sha_params("match")).await,
        observed(&readonly, OLD_COUNT, sha_params("match")).await
    );
    let actual = plan(&readonly, &annotation_count_sql(), sha_params("match")).await;
    assert!(actual
        .iter()
        .all(|line| !line.contains(CANONICAL) && !line.contains(NONCANONICAL)));
    assert!(query(
        &readonly,
        "SELECT name FROM sqlite_master WHERE name LIKE 'idx_git_notes_%'",
        vec![]
    )
    .await
    .is_empty());
}
