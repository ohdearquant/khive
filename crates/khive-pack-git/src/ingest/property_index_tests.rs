use super::*;
use khive_db::StorageBackend;
use khive_runtime::{PackRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::SqlRow;
use khive_types::Namespace;
use std::sync::Arc;

const SHA_INDEX: &str = "idx_git_notes_live_commit_sha";
const NUMBER_INDEX: &str = "idx_git_notes_live_number_project";
const EXPECT_GIT_PROPERTY_INDEXES: bool = true;
const GIT_CORE_INDEXES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../khive-db/sql/049-git-note-property-indexes.sql"
));

fn configure_property_indexes(runtime: &KhiveRuntime, indexed: bool) {
    if indexed {
        install(runtime);
    } else {
        runtime.backend().pool().try_writer().unwrap().conn().execute_batch(
            "DROP INDEX idx_git_notes_live_commit_sha; DROP INDEX idx_git_notes_live_number_project; DROP INDEX idx_git_notes_history_canonical_sha; DROP INDEX idx_git_notes_history_noncanonical"
        ).unwrap();
    }
}

fn file_runtime(path: &Path, read_only: bool) -> KhiveRuntime {
    let backend = if read_only {
        StorageBackend::sqlite_read_only_with_max_readers(path, Some(2)).unwrap()
    } else {
        StorageBackend::sqlite_with_max_readers(path, Some(2)).unwrap()
    };
    if !read_only {
        // from_backend is assembly only. Core migration 049, including its
        // indexes and the graph tables used by these real queries, must precede
        // pack schema loading. Keep the readonly historical-inspection arm raw.
        backend.prepare_core_schema().unwrap();
    }
    let runtime = KhiveRuntime::from_backend(Arc::new(backend), RuntimeConfig::no_embeddings());
    if !read_only {
        runtime.backend().notes_for_namespace("local").unwrap();
    }
    runtime
}

async fn close_file_runtime(runtime: KhiveRuntime) {
    let writer_join = runtime.backend().pool().take_writer_task_join();
    drop(runtime);
    if let Some(writer_join) = writer_join {
        tokio::time::timeout(std::time::Duration::from_secs(10), writer_join)
            .await
            .expect("writer task must close before read-only inspection")
            .unwrap();
    }
}

fn install(runtime: &KhiveRuntime) {
    let pack = crate::GitPack::new(runtime.clone());
    let plan = pack.schema_plan();
    runtime
        .backend()
        .apply_pack_ddl_statements(plan.statements)
        .unwrap();
}

fn insert(
    runtime: &KhiveRuntime,
    id: &str,
    ns: &str,
    kind: &str,
    properties: Value,
    deleted: bool,
) {
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer.conn().execute(
        "INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,?4,1,1,?5)",
        (id, ns, kind, properties.to_string(), deleted.then_some(1_i64)),
    ).unwrap();
}

fn delete(runtime: &KhiveRuntime, id: &str) {
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer
        .conn()
        .execute("UPDATE notes SET deleted_at=2 WHERE id=?1", [id])
        .unwrap();
}

async fn query(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> Vec<SqlRow> {
    let mut reader = runtime.sql().reader().await.unwrap();
    reader
        .query_all(SqlStatement {
            sql: sql.into(),
            params,
            label: Some("git_property_index_fixture".into()),
        })
        .await
        .unwrap()
}

async fn plan(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) -> Vec<String> {
    // EXPLAIN lists bytecode without executing its schema-cookie checks.
    // Pin one reader and step the exact SELECT before asking for its plan.
    let mut reader = runtime.sql().reader().await.unwrap();
    reader
        .query_all(SqlStatement {
            sql: "BEGIN DEFERRED".into(),
            params: vec![],
            label: Some("git_property_index_plan_begin".into()),
        })
        .await
        .unwrap();
    let observed = reader
        .query_all(SqlStatement {
            sql: sql.into(),
            params: params.clone(),
            label: Some("git_property_index_plan_read".into()),
        })
        .await
        .unwrap();
    assert!(
        !observed.is_empty(),
        "the exact planner fixture SELECT must observe its seeded row"
    );
    let rows = reader
        .query_all(SqlStatement {
            sql: format!("EXPLAIN QUERY PLAN {sql}"),
            params,
            label: Some("git_property_index_plan_explain".into()),
        })
        .await
        .unwrap();
    reader
        .query_all(SqlStatement {
            sql: "ROLLBACK".into(),
            params: vec![],
            label: Some("git_property_index_plan_end".into()),
        })
        .await
        .unwrap();
    rows.into_iter()
        .map(|row| match row.get("detail") {
            Some(SqlValue::Text(detail)) => detail.clone(),
            value => panic!("plan detail absent or wrong type: {value:?}"),
        })
        .collect()
}

fn snapshot(rows: &[SqlRow]) -> Value {
    serde_json::to_value(rows).unwrap()
}

fn annotation_count_sql() -> String {
    let insert = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../khive-db/sql/commit-annotation-insert.sql"
    ));
    let (_, rest) = insert.split_once("AND 1 = (\n").unwrap();
    let (count, _) = rest.split_once("\n)\nAND EXISTS").unwrap();
    format!("SELECT ({count}) AS holders").replace("?9", "?2")
}

async fn annotation_count(runtime: &KhiveRuntime, sha: &str) -> i64 {
    let rows = query(runtime, &annotation_count_sql(), sha_params(sha)).await;
    match rows[0].get("holders") {
        Some(SqlValue::Integer(count)) => *count,
        value => panic!("COUNT holder type: {value:?}"),
    }
}

fn uses(plan: &[String], index: &str) -> bool {
    plan.iter()
        .any(|line| line.contains(index) && line.contains("SEARCH"))
}

fn sha_params(sha: &str) -> Vec<SqlValue> {
    vec![SqlValue::Text("local".into()), SqlValue::Text(sha.into())]
}

fn number_params(kind: &str, number: i64, project: Uuid) -> Vec<SqlValue> {
    vec![
        SqlValue::Text(kind.into()),
        SqlValue::Text("local".into()),
        SqlValue::Integer(number),
        SqlValue::Text(project.to_string()),
    ]
}

fn seed_selectivity(runtime: &KhiveRuntime, project: Uuid, count: usize) {
    let writer = runtime.backend().pool().try_writer().unwrap();
    writer.transaction(|conn| {
        let mut statement = conn.prepare("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at) VALUES(?1,?2,?3,?4,1,1)")?;
        for index in 0..count {
            for (ns,kind) in [("local","commit"),("local","issue"),("local","pull_request"),("other","commit"),("other","issue")] {
                let props = if kind == "commit" { json!({"sha":format!("{index:040x}")}) } else { json!({"number":index as i64,"project_id":project.to_string()}) };
                statement.execute((Uuid::new_v4().to_string(),ns,kind,props.to_string()))?;
            }
        }
        Ok(())
    }).unwrap();
}

#[tokio::test]
async fn bundled_sqlite_uses_property_indexes_for_exact_production_queries() {
    for count in [100, 1000] {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("lookup.db"), false);
        let project = Uuid::new_v4();
        seed_selectivity(&runtime, project, count);
        let sha = format!("{:040x}", count - 1);
        let queries = [
            (
                "sha",
                sql!("commits_by_sha_select"),
                sha_params(&sha),
                SHA_INDEX,
            ),
            (
                "issue",
                sql!("notes_by_number_select"),
                number_params("issue", count as i64 - 1, project),
                NUMBER_INDEX,
            ),
            (
                "pull_request",
                sql!("notes_by_number_select"),
                number_params("pull_request", count as i64 - 1, project),
                NUMBER_INDEX,
            ),
        ];
        for (label, sql, params, index) in &queries {
            let before = plan(&runtime, sql, params.clone()).await;
            assert_eq!(
                uses(&before, index),
                EXPECT_GIT_PROPERTY_INDEXES,
                "core migration must index without a loaded pack: {before:?}"
            );
            println!(
                "PROPERTY_PLAN before {label} count={count} {}",
                json!(before)
            );
        }
        install(&runtime);
        for analyze in [false, true] {
            if analyze {
                runtime
                    .backend()
                    .pool()
                    .try_writer()
                    .unwrap()
                    .conn()
                    .execute_batch("ANALYZE")
                    .unwrap();
            }
            for (label, sql, params, index) in &queries {
                let after = plan(&runtime, sql, params.clone()).await;
                assert_eq!(
                    uses(&after, index),
                    EXPECT_GIT_PROPERTY_INDEXES,
                    "{label}: actual plan {after:?}"
                );
                assert_eq!(query(&runtime, sql, params.clone()).await.len(), 1);
                println!(
                    "PROPERTY_PLAN after {label} count={count} analyze={analyze} {}",
                    json!(after)
                );
            }
        }
    }
}

#[tokio::test]
async fn commit_lookup_preserves_live_scope_value_types_and_duplicate_refusals() {
    for indexed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("commit.db"), false);
        let local = runtime.authorize(Namespace::local()).unwrap();
        let foreign = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();
        configure_property_indexes(&runtime, indexed);
        let sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let upper = sha.to_ascii_uppercase();
        let live = Uuid::new_v4();
        let foreign_id = Uuid::new_v4();
        insert(
            &runtime,
            &live.to_string(),
            "local",
            "commit",
            json!({"sha":sha}),
            false,
        );
        insert(
            &runtime,
            &foreign_id.to_string(),
            "other",
            "commit",
            json!({"sha":sha}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"sha":sha}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":sha}),
            true,
        );
        assert_eq!(
            find_commit_by_sha(&runtime, &local, sha).await.unwrap(),
            Some(live)
        );
        assert_eq!(
            find_commit_by_sha(&runtime, &foreign, sha).await.unwrap(),
            Some(foreign_id)
        );
        assert_eq!(
            find_commit_by_sha(&runtime, &local, &upper).await.unwrap(),
            None
        );
        let upper_id = Uuid::new_v4();
        insert(
            &runtime,
            &upper_id.to_string(),
            "local",
            "commit",
            json!({"sha":upper}),
            false,
        );
        assert_eq!(
            find_commit_by_sha(&runtime, &local, &upper).await.unwrap(),
            Some(upper_id)
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":123}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":null}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({}),
            false,
        );
        assert_eq!(
            find_commit_by_sha(&runtime, &local, "123").await.unwrap(),
            None
        );
        assert_eq!(
            find_commit_by_sha(&runtime, &local, "missing")
                .await
                .unwrap(),
            None
        );
        insert(
            &runtime,
            "invalid-commit-id",
            "local",
            "commit",
            json!({"sha":sha}),
            false,
        );
        let duplicate = find_commit_by_sha(&runtime, &local, sha)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(duplicate,format!("multiple live commit notes hold SHA {sha}; reconcile legacy duplicates before advancing an ingest checkpoint"));
        delete(&runtime, &live.to_string());
        assert_eq!(
            find_commit_by_sha(&runtime, &local, sha)
                .await
                .unwrap_err()
                .to_string(),
            format!("stored commit note has an invalid ID for SHA {sha}")
        );
        delete(&runtime, "invalid-commit-id");
        assert_eq!(
            find_commit_by_sha(&runtime, &local, sha).await.unwrap(),
            None
        );
    }
}

#[tokio::test]
async fn number_lookup_preserves_namespace_project_kind_type_and_cast_contracts() {
    for indexed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("numbers.db"), false);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let foreign = runtime
            .authorize(Namespace::parse("other").unwrap())
            .unwrap();
        let project = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
        let other_project = Uuid::new_v4();
        configure_property_indexes(&runtime, indexed);
        let issue = Uuid::new_v4();
        let pr = Uuid::new_v4();
        let other_ns = Uuid::new_v4();
        for (id, ns, kind) in [
            (issue, "local", "issue"),
            (pr, "local", "pull_request"),
            (other_ns, "other", "issue"),
        ] {
            insert(
                &runtime,
                &id.to_string(),
                ns,
                kind,
                json!({"number":42,"project_id":project.to_string()}),
                false,
            );
        }
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"number":42,"project_id":project.to_string()}),
            true,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"number":"42","project_id":other_project.to_string()}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"number":42}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"number":null,"project_id":project.to_string()}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"number":42,"project_id":null}),
            false,
        );
        assert_eq!(
            find_by_number(&runtime, &token, "issue", project, 42)
                .await
                .unwrap(),
            Some(issue)
        );
        assert_eq!(
            find_by_number(&runtime, &token, "pull_request", project, 42)
                .await
                .unwrap(),
            Some(pr)
        );
        assert_eq!(
            find_by_number(&runtime, &foreign, "issue", project, 42)
                .await
                .unwrap(),
            Some(other_ns)
        );
        assert_eq!(
            find_by_number(&runtime, &token, "issue", other_project, 42)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            find_by_number(&runtime, &token, "issue", project, 43)
                .await
                .unwrap(),
            None
        );
        insert(
            &runtime,
            "invalid-number-id",
            "local",
            "issue",
            json!({"number":66,"project_id":project.to_string()}),
            false,
        );
        assert_eq!(
            find_by_number(&runtime, &token, "issue", project, 66)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            query(
                &runtime,
                sql!("notes_by_number_select"),
                number_params("issue", 66, project)
            )
            .await
            .len(),
            1
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"number":77,"project_id":project.to_string().to_ascii_uppercase()}),
            false,
        );
        assert_eq!(
            find_by_number(&runtime, &token, "issue", project, 77)
                .await
                .unwrap(),
            None
        );
        let cast_id = Uuid::new_v4();
        insert(
            &runtime,
            &cast_id.to_string(),
            "local",
            "issue",
            json!({"number":-1,"project_id":project.to_string()}),
            false,
        );
        assert_eq!(
            find_by_number(&runtime, &token, "issue", project, u64::MAX)
                .await
                .unwrap(),
            Some(cast_id)
        );
    }
}

fn project_update_fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_db::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_config: None,
        volume_lock_dir: None,
        actor_id: None,
        brain_profile: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        events_split: None,
        mounts: Vec::new(),
        packs: vec!["kg".into(), "git".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::GitPack::new(runtime.clone()));
    builder
        .with_runtime_event_store(&runtime)
        .expect("runtime audit store");
    let registry = builder.build().expect("registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(runtime.backend());
    (runtime, token, registry)
}

#[tokio::test]
async fn dispatched_project_id_spellings_remain_findable_by_ingest() {
    let (runtime, token, registry) = project_update_fixture();
    let project = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
    let other = Uuid::parse_str("bbbbbbbb-cccc-4ddd-8eee-ffffffffffff").unwrap();
    let canonical = project.to_string();
    for kind in ["issue", "pull_request"] {
        for (index, spelling) in [
            canonical.clone(),
            canonical.to_ascii_uppercase(),
            project.simple().to_string(),
            format!("{{{project}}}"),
            format!("urn:uuid:{project}"),
        ]
        .into_iter()
        .enumerate()
        {
            let number = index as u64 + 1;
            let created = registry
                .dispatch(
                    "create",
                    json!({
                        "kind": kind, "content": "project spelling fixture",
                        "properties": {"number": number, "project_id": canonical, "kept": true},
                    }),
                )
                .await
                .expect("create a canonical note");
            let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
            assert_eq!(
                find_by_number(&runtime, &token, kind, project, number)
                    .await
                    .unwrap(),
                Some(id),
                "the canonical note must be visible before the update"
            );
            let updated = registry
                .dispatch(
                    "update",
                    json!({"id": id, "kind": kind, "properties": {"project_id": spelling}}),
                )
                .await
                .expect("a complete spelling of the same project is accepted");
            let stored = registry.dispatch("get", json!({"id": id})).await.unwrap();
            assert_eq!(
                find_by_number(&runtime, &token, kind, project, number)
                    .await
                    .unwrap(),
                Some(id),
                "the actual ingest lookup must retain the dispatched note for {kind} {spelling}"
            );
            assert_eq!(updated["properties"]["project_id"], json!(canonical));
            assert_eq!(stored["properties"]["project_id"], json!(canonical));
            assert_eq!(stored["properties"]["kept"], json!(true));

            registry
                .dispatch(
                    "update",
                    json!({"id": id, "kind": kind, "properties": {"project_id": other}}),
                )
                .await
                .expect("a different canonical project remains a legal update");
            assert_eq!(
                find_by_number(&runtime, &token, kind, project, number)
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                find_by_number(&runtime, &token, kind, other, number)
                    .await
                    .unwrap(),
                Some(id)
            );
        }
    }
}

#[tokio::test]
async fn dispatched_project_id_normalization_preserves_validation_and_omission() {
    let (runtime, token, registry) = project_update_fixture();
    let project = Uuid::parse_str("aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee").unwrap();
    for kind in ["issue", "pull_request"] {
        let created = registry
            .dispatch(
                "create",
                json!({
                    "kind": kind, "content": "project validation fixture",
                    "properties": {"number": 9, "project_id": project},
                }),
            )
            .await
            .unwrap();
        let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
        registry
            .dispatch(
                "update",
                json!({"id": id, "properties": {"unrelated": "retained"}}),
            )
            .await
            .expect("omitting project_id must preserve it");
        let before = registry.dispatch("get", json!({"id": id})).await.unwrap();
        assert_eq!(before["properties"]["project_id"], json!(project));
        assert_eq!(before["properties"]["unrelated"], json!("retained"));
        for invalid in [
            Value::Null,
            json!(false),
            json!(7),
            json!([]),
            json!({}),
            json!("aaaaaaaa"),
            json!("not-a-uuid"),
        ] {
            let error = registry
                .dispatch(
                    "update",
                    json!({"id": id, "kind": kind, "properties": {"project_id": invalid}}),
                )
                .await
                .expect_err("the existing project_id validator must still refuse");
            assert!(
                matches!(error, RuntimeError::InvalidInput(ref text) if text.contains("project_id"))
            );
            assert_eq!(
                registry.dispatch("get", json!({"id": id})).await.unwrap(),
                before
            );
            assert_eq!(
                find_by_number(&runtime, &token, kind, project, 9)
                    .await
                    .unwrap(),
                Some(id)
            );
        }
        let error = registry
            .dispatch(
                "update",
                json!({
                    "id": id, "kind": kind,
                    "properties": {"project_id": format!("{{{project}}}"), "number": "nine"},
                }),
            )
            .await
            .expect_err("normalization must not skip the remaining kind validation");
        assert!(
            matches!(error, RuntimeError::InvalidInput(ref text) if text.contains("number must be an integer"))
        );
        assert_eq!(
            registry.dispatch("get", json!({"id": id})).await.unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn number_duplicates_retain_unspecified_holder_including_invalid_ids() {
    for invalid_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("duplicates.db"), false);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let project = Uuid::new_v4();
        let valid = Uuid::new_v4();
        let ids = if invalid_first {
            vec!["invalid-issue-id".to_string(), valid.to_string()]
        } else {
            vec![valid.to_string(), "invalid-issue-id".to_string()]
        };
        for id in &ids {
            insert(
                &runtime,
                id,
                "local",
                "issue",
                json!({"number":55,"project_id":project.to_string()}),
                false,
            );
        }
        for indexed in [false, true] {
            if indexed {
                install(&runtime);
            }
            let rows = query(
                &runtime,
                sql!("notes_by_number_select"),
                number_params("issue", 55, project),
            )
            .await;
            assert_eq!(rows.len(), 1);
            let selected = rows[0].get("id");
            assert!(matches!(selected,Some(SqlValue::Text(id)) if ids.contains(id)));
            let actual = find_by_number(&runtime, &token, "issue", project, 55)
                .await
                .unwrap();
            assert!(actual.is_none() || actual == Some(valid));
            println!("UNSPECIFIED_NUMBER_WINNER invalid_first={invalid_first} indexed={indexed} selected={selected:?} resolved={actual:?}");
        }
    }
}

#[tokio::test]
async fn pack_indexes_reopen_idempotently_and_read_only_store_uses_installed_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen.db");
    let runtime = file_runtime(&path, false);
    let project = Uuid::new_v4();
    let id = Uuid::new_v4();
    insert(
        &runtime,
        &id.to_string(),
        "local",
        "issue",
        json!({"number":7,"project_id":project.to_string()}),
        false,
    );
    install(&runtime);
    let sql="SELECT name,sql FROM sqlite_master WHERE type='index' AND name IN ('idx_git_notes_live_commit_sha','idx_git_notes_live_number_project') ORDER BY name";
    let first = query(&runtime, sql, vec![]).await;
    assert_eq!(first.len(), if EXPECT_GIT_PROPERTY_INDEXES { 2 } else { 0 });
    install(&runtime);
    assert_eq!(
        snapshot(&query(&runtime, sql, vec![]).await),
        snapshot(&first)
    );
    close_file_runtime(runtime).await;
    let runtime = file_runtime(&path, false);
    install(&runtime);
    assert_eq!(
        snapshot(&query(&runtime, sql, vec![]).await),
        snapshot(&first)
    );
    close_file_runtime(runtime).await;
    assert!(
        !path.with_extension("db-wal").exists(),
        "writer drain must settle WAL"
    );
    assert!(
        !path.with_extension("db-shm").exists(),
        "no writable SHM before inspection"
    );
    let readonly = file_runtime(&path, true);
    let token = readonly.authorize(Namespace::local()).unwrap();
    assert_eq!(
        snapshot(&query(&readonly, sql, vec![]).await),
        snapshot(&first)
    );
    assert_eq!(
        find_by_number(&readonly, &token, "issue", project, 7)
            .await
            .unwrap(),
        Some(id)
    );
    let actual = plan(
        &readonly,
        sql!("notes_by_number_select"),
        number_params("issue", 7, project),
    )
    .await;
    assert_eq!(uses(&actual, NUMBER_INDEX), EXPECT_GIT_PROPERTY_INDEXES);
    assert!(readonly.backend().is_read_only());
}

#[tokio::test]
async fn malformed_tombstone_history_is_not_indexed_or_repaired_and_count_error_remains() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = file_runtime(&dir.path().join("malformed.db"), false);
    // Malformed properties cannot coexist with the current core JSON indexes:
    // their unread predicates evaluate json_type even for tombstones. This legacy
    // fixture removes that class in its test DB; restoring it would fail too.
    let indexes = query(
        &runtime,
        "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='notes' AND (sql LIKE '%json_extract%' OR sql LIKE '%json_type%') ORDER BY name",
        vec![],
    )
    .await;
    assert!(
        !indexes.is_empty(),
        "JSON index class enumeration must be nonempty"
    );
    {
        let writer = runtime.backend().pool().try_writer().unwrap();
        writer.transaction(|conn| {
            for row in &indexes {
                let Some(SqlValue::Text(name)) = row.get("name") else {
                    panic!("sqlite_master.name must be text");
                };
                conn.execute_batch(&format!("DROP INDEX \"{}\"", name.replace('"', "\"\"")))?;
            }
            for (id, ns, kind) in [
                ("deleted-local", "local", "commit"),
                ("deleted-foreign", "other", "commit"),
                ("deleted-issue", "local", "issue"),
            ] {
                conn.execute("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,'{',1,1,1)",(id,ns,kind))?;
            }
            Ok(())
        }).unwrap();
    }
    assert!(query(
        &runtime,
        "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='notes' AND (sql LIKE '%json_extract%' OR sql LIKE '%json_type%')",
        vec![],
    ).await.is_empty(), "legacy fixture intentionally has no core JSON indexes");
    let annotation = sql!("annotation_repair_commit_notes_select");
    let mut reader = runtime.sql().reader().await.unwrap();
    let statement = SqlStatement {
        sql: annotation.into(),
        params: sha_params("missing"),
        label: Some("legacy_annotation_refusal".into()),
    };
    let before = reader
        .query_all(statement.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(before.contains("malformed JSON"), "{before}");
    drop(reader);
    let count_statement = SqlStatement {
        sql: annotation_count_sql(),
        params: sha_params("missing"),
        label: Some("legacy_annotation_count_refusal".into()),
    };
    let mut reader = runtime.sql().reader().await.unwrap();
    let count_before = reader
        .query_all(count_statement.clone())
        .await
        .unwrap_err()
        .to_string();
    assert!(count_before.contains("malformed JSON"), "{count_before}");
    drop(reader);
    runtime
        .backend()
        .pool()
        .try_writer()
        .unwrap()
        .conn()
        .execute_batch(GIT_CORE_INDEXES)
        .unwrap();
    install(&runtime);
    let pack_indexes = query(
        &runtime,
        "SELECT name FROM sqlite_master WHERE type='index' AND name IN ('idx_git_notes_live_commit_sha','idx_git_notes_live_number_project')",
        vec![],
    ).await;
    assert_eq!(
        pack_indexes.len(),
        if EXPECT_GIT_PROPERTY_INDEXES { 2 } else { 0 }
    );
    let mut reader = runtime.sql().reader().await.unwrap();
    let count_after = reader
        .query_all(count_statement)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(count_before, count_after);
    let after = reader.query_all(statement).await.unwrap_err().to_string();
    assert_eq!(before, after);
    drop(reader);
    let rows = query(
        &runtime,
        "SELECT id,properties,deleted_at FROM notes WHERE id LIKE 'deleted-%' ORDER BY id",
        vec![],
    )
    .await;
    assert_eq!(rows.len(), 3);
    for row in rows {
        assert!(matches!(row.get("properties"), Some(SqlValue::Text(value)) if value == "{"));
        assert!(matches!(row.get("deleted_at"), Some(SqlValue::Integer(1))));
    }
    let writer = runtime.backend().pool().try_writer().unwrap();
    let invalid_live = writer.conn().execute("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at) VALUES('invalid-live-after','local','commit','{',1,1)",[]);
    assert_eq!(invalid_live.is_err(), EXPECT_GIT_PROPERTY_INDEXES);
}

#[tokio::test]
async fn current_core_notes_schema_retains_live_json_guards() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = file_runtime(&dir.path().join("full-core-guards.db"), false);
    for installed in [false, true] {
        if installed {
            install(&runtime);
        }
        let writer = runtime.backend().pool().try_writer().unwrap();
        let error = writer.conn().execute("INSERT INTO notes(id,namespace,kind,properties,created_at,updated_at) VALUES('invalid-current-live','local','commit','{',1,1)",[]).unwrap_err().to_string();
        assert!(error.contains("malformed JSON"), "full core guard: {error}");
    }
}

#[tokio::test]
async fn pack_schema_batch_rolls_back_actual_late_sqlite_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = file_runtime(&dir.path().join("rollback.db"), false);
    let pack = crate::GitPack::new(runtime.clone());
    let plan = pack.schema_plan();
    let mut statements = plan.statements.to_vec();
    statements.push("CREATE INDEX idx_fixture_invalid_column ON notes(missing_fixture_column)");
    let error = runtime
        .backend()
        .apply_pack_ddl_statements(&statements)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no such column"), "{error}");
    let created=query(&runtime,"SELECT name FROM sqlite_master WHERE name IN ('idx_git_notes_live_commit_sha','idx_git_notes_live_number_project','git_mirror_cursor','git_receipts')",vec![]).await;
    assert_eq!(
        created.len(),
        2,
        "preexisting core live indexes survive: {created:?}"
    );
    assert!(created.iter().all(|row| matches!(row.get("name"), Some(SqlValue::Text(name)) if name.starts_with("idx_git_notes_live_"))), "no partial auxiliary pack install after real SQLite refusal: {created:?}");
    install(&runtime);
}

#[tokio::test]
async fn annotation_count_keeps_zero_one_many_and_deleted_history() {
    for indexed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let runtime = file_runtime(&dir.path().join("count.db"), false);
        configure_property_indexes(&runtime, indexed);
        let sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert_eq!(annotation_count(&runtime, sha).await, 0);
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":sha}),
            false,
        );
        assert_eq!(annotation_count(&runtime, sha).await, 1);
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":sha}),
            true,
        );
        assert_eq!(annotation_count(&runtime, sha).await, 2);
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "other",
            "commit",
            json!({"sha":sha}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "issue",
            json!({"sha":sha}),
            false,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":sha.to_ascii_uppercase()}),
            false,
        );
        assert_eq!(annotation_count(&runtime, sha).await, 2);
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":123}),
            true,
        );
        insert(
            &runtime,
            &Uuid::new_v4().to_string(),
            "local",
            "commit",
            json!({"sha":"123"}),
            true,
        );
        assert_eq!(annotation_count(&runtime, "123").await, 1);
    }
}

#[path = "annotation_count_index_tests.rs"]
mod annotation_count_index_tests;
