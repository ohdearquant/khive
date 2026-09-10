//! Opt-in diagnostic: full-history replay against existing file-backed issue notes.
//! Run with `cargo test -p khive-pack-git --test acceptance digest_scale -- --ignored --nocapture`.
//! Timing is evidence, not a normal CI assertion or proof about another store.

use super::*;
use std::time::{Duration, Instant};

const ISSUES: usize = 5_000;
const UNRELATED: usize = 50_000;
const VISITS: u64 = 5_005; // Inclusive paging revisits each of five boundary rows.

async fn file_fixture(path: &Path) -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize fixture");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    builder.with_runtime_event_store(&rt).unwrap();
    let registry = builder.build().unwrap();
    rt.install_edge_rules(registry.all_edge_rules());
    registry.call_register_entity_type_validators(&rt);
    registry.apply_schema_plans(rt.backend());
    (rt, token, registry)
}

fn timestamp(index: usize) -> String {
    chrono::DateTime::from_timestamp(1_735_689_600 + index as i64, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Uses the acceptance harness's PATH boundary; no live GitHub process is called.
/// Each response is selected by the exact inclusive search floor, not call order.
fn paged_gh(repo: &Path, bin: &Path, logs: &Path) {
    write_fake_gh(repo, bin, logs, "[]", "[]");
    let rows: Vec<Value> = (0..ISSUES)
        .map(|i| {
            json!({"number":i+1,"title":format!("Fixture issue {}",i+1),
                "author":{"login":"fixture"},"createdAt":timestamp(i),
                "updatedAt":timestamp(i),"closedAt":null,"stateReason":null,
                "labels":[],"body":"Already stored fixture issue."})
        })
        .collect();
    let mut arms = String::new();
    let mut first = 0;
    let mut floor = "sort:updated-asc".to_string();
    loop {
        let end = (first + 1_000).min(rows.len());
        let file = logs.join(format!("page-{first}.json"));
        std::fs::write(&file, serde_json::to_vec(&rows[first..end]).unwrap()).unwrap();
        arms.push_str(&format!(
            "    {}) cat {} ;;\n",
            shell_quote(&floor),
            shell_quote(file.to_str().unwrap())
        ));
        if end == rows.len() {
            break;
        }
        first = end - 1;
        floor = format!("sort:updated-asc updated:>={}", timestamp(first));
    }
    let script = format!(
        r#"#!/bin/sh
set -eu
if [ "$1" = repo ]; then
  [ "$2" = view ] && [ "$3" = fixture/repository ] || exit 2
  printf '%s\n' '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
  exit 0
fi
[ "$1" = issue ] && [ "$2" = list ] || exit 3
search= repo=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --search) search="$2"; shift ;;
    --repo) repo="$2"; shift ;;
  esac
  shift
done
[ "$repo" = fixture/repository ] || exit 4
printf '%s\n' "$search" >> {log}
case "$search" in
{arms}    *) printf '%s\n' 'unexpected fixture floor' >&2; exit 5 ;;
esac
"#,
        log = shell_quote(logs.join("pages.log").to_str().unwrap())
    );
    // write_fake_gh already set the executable bit on this exact file.
    std::fs::write(bin.join("gh"), script).unwrap();
}

fn stmt(label: &str, sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: Some(label.into()),
    }
}

/// Copies the labelled SELECTs on the canonical-project, issues-only path.
/// Keep predicates and bound value types identical to handlers.rs / ingest.rs.
fn reader_statements(project: Uuid) -> Vec<SqlStatement> {
    let ns = SqlValue::Text("local".into());
    let id = SqlValue::Text(project.to_string());
    let slug = SqlValue::Text("github.com/fixture/repository".into());
    vec![
        stmt("git_digest_find_projects_by_slug", "SELECT id FROM entities WHERE kind='project' AND namespace=?1 AND deleted_at IS NULL AND json_extract(properties,'$.repo_slug')=?2 ORDER BY created_at ASC, id ASC", vec![ns.clone(),slug.clone()]),
        stmt("git_digest_find_projects_without_canonical_slug", "SELECT id, json_extract(properties,'$.repo_url') AS repo_url FROM entities WHERE kind='project' AND namespace=?1 AND deleted_at IS NULL AND json_extract(properties,'$.repo_url') IS NOT NULL AND (json_extract(properties,'$.repo_slug') IS NULL OR json_extract(properties,'$.repo_slug')<>?2) ORDER BY created_at ASC, id ASC", vec![ns.clone(),slug]),
        stmt("git_ingest_read_cursor", "SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?2", vec![id.clone(),SqlValue::Text("issues".into())]),
        stmt("git_ingest_find_by_number", "SELECT id FROM notes WHERE kind=?1 AND namespace=?2 AND deleted_at IS NULL AND json_extract(properties,'$.number')=?3 AND json_extract(properties,'$.project_id')=?4 LIMIT 1", vec![SqlValue::Text("issue".into()),ns.clone(),SqlValue::Integer(ISSUES as i64),id.clone()]),
        stmt("git_ingest_count_commit_notes", "SELECT COUNT(*) FROM notes n JOIN graph_edges e ON e.source_id = n.id AND e.namespace = n.namespace WHERE n.kind = 'commit' AND n.namespace = ?1 AND n.deleted_at IS NULL AND e.relation = 'annotates' AND e.target_id = ?2 AND e.deleted_at IS NULL", vec![ns,id]),
    ]
}

async fn query_plans(rt: &KhiveRuntime, project: Uuid) {
    for query in reader_statements(project) {
        let mut explain = query.clone();
        explain.sql = format!("EXPLAIN QUERY PLAN {}", query.sql);
        let mut reader = rt.sql().reader().await.unwrap();
        let plan = reader.query_all(explain).await.expect("read query plan");
        let started = Instant::now();
        let result = reader.query_all(query.clone()).await;
        println!(
            "DIGEST_SCALE_PLAN {}",
            json!({"label":query.label,"sql":query.sql,
            "params":query.params,"plan":plan,"probe_us":started.elapsed().as_micros(),
            "probe_error":result.err().map(|e|e.to_string())})
        );
    }
    // SqlReader (and queue-backed SqlWriter::explain) correctly refuses an
    // INSERT capability, even under EXPLAIN. Use only this fixture's raw
    // connection for its write plan. EXPLAIN never executes the cursor write.
    let query = stmt("git_ingest_write_cursor", "INSERT INTO git_mirror_cursor(project_id, kind, cursor_value, updated_at) VALUES(?1, ?2, ?3, ?4) ON CONFLICT(project_id, kind) DO UPDATE SET cursor_value=excluded.cursor_value, updated_at=excluded.updated_at", vec![SqlValue::Text(project.to_string()),SqlValue::Text("issues".into()),SqlValue::Text(timestamp(ISSUES-1)),SqlValue::Integer(0)]);
    let writer = rt.backend().pool().try_writer().unwrap();
    let mut prepared = writer
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {}", query.sql))
        .unwrap();
    let plan: Vec<Value> = prepared
        .query_map(
            (project.to_string(), "issues", timestamp(ISSUES - 1), 0_i64),
            |row| {
                Ok(
                    json!({"id": row.get::<_, i64>(0)?, "parent": row.get::<_, i64>(1)?,
            "aux": row.get::<_, i64>(2)?, "detail": row.get::<_, String>(3)?}),
                )
            },
        )
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    println!(
        "DIGEST_SCALE_PLAN {}",
        json!({"label":query.label,"sql":query.sql,
        "params":query.params,"plan":plan,"probe_us":null,"probe_error":null,
        "write_executed":false,"plan_connection":"fixture_raw_writer"})
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
#[ignore = "file-backed ingest scale measurement; run explicitly with --ignored --nocapture"]
async fn digest_scale_existing_tracker() {
    let _env_lock = ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tracker.db");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo(&repo);
    let bin = dir.path().join("bin");
    let logs = dir.path().join("logs");
    std::fs::create_dir(&bin).unwrap();
    std::fs::create_dir(&logs).unwrap();
    paged_gh(&repo, &bin, &logs);
    let _path = PathGuard::install(&bin);

    println!(
        "DIGEST_SCALE_CONTROLS {}",
        json!({
        "issues":ISSUES,"unrelated_notes":UNRELATED,"unrelated_kind":"observation",
        "namespace":"local","expected_visits":VISITS,"expected_pages":6,
        "max_items":[10,200,20000],"new_creates_expected":0,
        "deadline_seconds":30,"timing_assertions":false,
        "cursor":"absent/full-history-replay; cleared before each arm",
        "seed_shape":"minimal issue properties number/project_id plus annotates edges; small content; no live-store import",
        "background_index_scope":"observation notes share namespace but not the issue kind index range",
        "failure_observability":"terminal error only; first failing SQL and visited count unavailable on error",
        "hold_observability":"maximum since each fresh pool opened, including disclosed bootstrap maximum",
        "expectation":"unscoped core completes with all existing; scoped verb either completes or reports actual deadline failure; no production fix inferred from source alone"})
    );

    let project;
    {
        let (rt, _token, registry) = file_fixture(&db).await;
        project = create(&registry, json!({"kind":"project","name":"fixture/repository",
            "properties":{"repo_slug":"github.com/fixture/repository","repo_url":"https://github.com/fixture/repository"}})).await;
        let writer = rt.backend().pool().try_writer().unwrap();
        writer.conn().execute_batch(&format!(
            "BEGIN; \
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<{UNRELATED}) \
             INSERT INTO notes(id,namespace,kind,name,content,properties,created_at,updated_at) \
             SELECT printf('20000000-0000-4000-8000-%012x',i),'local','observation','Unrelated fixture note','Background.', '{{}}',i,i FROM n; \
             WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<{ISSUES}) \
             INSERT INTO notes(id,namespace,kind,name,content,properties,created_at,updated_at) \
             SELECT printf('10000000-0000-4000-8000-%012x',i),'local','issue','Fixture issue','Already stored fixture issue.',json_object('number',i,'project_id','{project}'),i,i FROM n; \
             INSERT INTO graph_edges(namespace,id,source_id,target_id,relation,weight,created_at,updated_at) \
             SELECT namespace,replace(id,'10000000-','30000000-'),id,'{project}','annotates',1,created_at,updated_at FROM notes WHERE kind='issue'; COMMIT;"
        )).expect("seed exact fixture population");
        let count: i64 = writer
            .conn()
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        // Project creation is an entity; its audit records are in events, not notes.
        assert_eq!(count, (ISSUES + UNRELATED) as i64);
        drop(writer);
        query_plans(&rt, project).await;
    }

    let mut failures = Vec::new();
    for max_items in [10_u64, 200, 20_000] {
        for scoped in [false, true] {
            // Reset outside the measured interval, then reconstruct the entire pool.
            {
                let (rt, _, _) = file_fixture(&db).await;
                rt.backend()
                    .pool()
                    .try_writer()
                    .unwrap()
                    .conn()
                    .execute("DELETE FROM git_mirror_cursor", [])
                    .unwrap();
            }
            std::fs::write(logs.join("pages.log"), "").unwrap();
            let (rt, token, registry) = file_fixture(&db).await;
            let before = rt.backend().pool().reader_acquisition_snapshot();
            let started = Instant::now();
            let result: Result<Value, String> = if scoped {
                khive_storage::scope_request_read_deadline(
                    Duration::from_secs(30),
                    registry.dispatch(
                        "git.digest",
                        json!({"source":"https://github.com/fixture/repository",
                        "include":["issues"],"max_items":max_items}),
                    ),
                )
                .await
                .map_err(|e| e.to_string())
            } else {
                run_ingest(
                    &rt,
                    &token,
                    &registry,
                    IngestOptions {
                        repo: repo.clone(),
                        expected_github_repo: Some("fixture/repository".into()),
                        project: project.to_string(),
                        max_items: Some(max_items),
                        include: IngestInclude {
                            commits: false,
                            issues: true,
                            pull_requests: false,
                        },
                    },
                )
                .await
                .map(|r| serde_json::to_value(r).unwrap())
                .map_err(|e| e.to_string())
            };
            let elapsed = started.elapsed();
            let after = rt.backend().pool().reader_acquisition_snapshot();
            let pages = std::fs::read_to_string(logs.join("pages.log"))
                .unwrap()
                .lines()
                .count();
            let visited = result
                .as_ref()
                .ok()
                .and_then(|r| r["issues_skipped_existing"].as_u64());
            println!(
                "DIGEST_SCALE_RESULT {}",
                json!({"max_items":max_items,
                "scope":if scoped {"verb_30s_deadline"} else {"core_unscoped"},
                "elapsed_ms":elapsed.as_secs_f64()*1000.0,"visited":visited,"pages":pages,
                "first_failing_sql_observed":false,"first_failing_sql":null,
                "visited_observation":if visited.is_some() {"successful report"} else {"unobserved on error"},
                "per_item_us":visited.filter(|n|*n>0).map(|n|elapsed.as_secs_f64()*1e6/n as f64),
                "completed_hold_max_us_since_pool_open":after.max_completed_hold_micros,
                "bootstrap_hold_max_us":before.max_completed_hold_micros,
                "acquisitions":after.acquisitions-before.acquisitions,
                "checkout_timeouts":after.checkout_timeouts-before.checkout_timeouts,
                "active_checkouts":after.active_pooled_checkouts,
                "free_reader_slots":after.available_reader_admission_slots,
                "reader_capacity":after.reader_admission_capacity,"result":result})
            );
            assert_eq!(after.active_pooled_checkouts, 0);
            assert_eq!(
                after.available_reader_admission_slots,
                after.reader_admission_capacity
            );
            match result {
                Ok(report) => {
                    assert_eq!(report["issues_ingested"], 0);
                    assert_eq!(report["issues_skipped_existing"], VISITS);
                    assert_eq!(report["sources"]["issues"]["state"], "completed");
                    assert_eq!(pages, 6);
                }
                Err(error) => {
                    failures.push(format!("max_items={max_items} scoped={scoped}: {error}"))
                }
            }
        }
    }
    assert!(failures.is_empty(), "digest scale failures: {failures:#?}");
}
