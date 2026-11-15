async fn project_annotation_checkpoint_snapshot(
    runtime: &KhiveRuntime,
    project_id: Uuid,
) -> (String, String) {
    let sql = runtime.sql();
    let row = sql
        .reader()
        .await
        .expect("checkpoint reader")
        .query_row(SqlStatement {
            sql: include_str!("../../sql/page_checkpoint_select.sql").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text("issues".into()),
                SqlValue::Text("issues_checkpoint".into()),
            ],
            label: Some("test_project_annotation_checkpoint_snapshot".into()),
        })
        .await
        .expect("read checkpoint snapshot")
        .expect("checkpoint projection row");
    let text = |field| match row.get(field) {
        Some(SqlValue::Text(value)) => value.clone(),
        _ => panic!("expected seeded checkpoint text field {field}"),
    };
    (text("floor"), text("progress"))
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn project_annotation_census_reaches_notes_outside_the_remote_window() {
    let (runtime, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "census-project"}),
    )
    .await;
    let other_project = create(
        &registry,
        json!({"kind": "project", "name": "other-project"}),
    )
    .await;
    let floor = "2026-01-02T00:00:00Z";

    let mut missing_ids = std::collections::BTreeSet::new();
    let mut acknowledged_id = None;
    for (number, kind, updated_at) in [
        (1, "issue", "2026-01-01T00:00:00Z"),
        (2, "pull_request", "2026-01-03T00:00:00Z"),
        (3, "issue", floor),
    ] {
        let id = create(
            &registry,
            json!({
                "kind": kind,
                "content": "",
                "properties": {
                    "number": number,
                    "title": format!("unlinked {kind} {number}"),
                    "updated_at": updated_at,
                    "project_id": project_id.to_string()
                }
            }),
        )
        .await;
        missing_ids.insert(id.to_string());
        if number == 3 {
            acknowledged_id = Some(id);
        }
    }
    let linked = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 4, "title": "linked", "project_id": project_id.to_string()},
            "annotates": [project_id.to_string()]
        }),
    )
    .await;
    create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 5, "title": "other project", "project_id": other_project.to_string()}
        }),
    )
    .await;

    let at_floor = std::collections::BTreeMap::from([(
        "3".to_string(),
        acknowledged_id.expect("at-floor fixture ID").to_string(),
    )]);
    let progress = json!({
        "version": 1,
        "namespace": token.namespace().as_str(),
        "floor": floor,
        "at_floor": at_floor,
        "undated": {}
    })
    .to_string();
    runtime
        .sql()
        .writer()
        .await
        .expect("checkpoint writer")
        .execute(SqlStatement {
            sql: include_str!("../../sql/page_checkpoint_with_floor_upsert.sql").into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text("issues_checkpoint".into()),
                SqlValue::Text(progress),
                SqlValue::Integer(khive_storage::now_micros()),
                SqlValue::Text("issues".into()),
                SqlValue::Text(floor.to_string()),
            ],
            label: Some("test_seed_project_annotation_checkpoint".into()),
        })
        .await
        .expect("seed acknowledged issue checkpoint");
    let before = project_annotation_checkpoint_snapshot(&runtime, project_id).await;

    // No remote or local history producer is needed: the census must run
    // even when this pass does not request any source walker.
    let report = run_ingest(
        &runtime,
        &token,
        &registry,
        IngestOptions {
            repo: PathBuf::from("."),
            expected_github_repo: None,
            project: project_id.to_string(),
            max_items: None,
            include: IngestInclude {
                commits: false,
                issues: false,
                pull_requests: false,
            },
        },
    )
    .await
    .expect("local-only ingest pass");
    let report = serde_json::to_value(report).expect("serialized ingest report");
    assert_eq!(
        report["missing_project_annotation_count"],
        json!(3),
        "the census must reach old, above-floor and acknowledged-at-floor notes"
    );
    let reported_ids = report["missing_project_annotation_note_ids"]
        .as_array()
        .expect("missing annotation IDs")
        .iter()
        .map(|id| id.as_str().expect("note ID string").to_string())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(reported_ids, missing_ids);
    assert_eq!(report["project_annotation_notes_scanned"], json!(4));
    assert_eq!(report["project_annotation_census_complete"], json!(true));
    assert_eq!(report["done"], json!(true));
    assert_eq!(report["cursor_stalled"], json!(false));
    assert_eq!(report["issues_skipped_existing"], json!(0));
    assert_eq!(
        project_annotation_checkpoint_snapshot(&runtime, project_id).await,
        before,
        "the local census must not rewrite the remote floor or acknowledgement payload"
    );

    let neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": project_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("project annotations");
    let neighbors = neighbors.as_array().expect("annotation neighbor array");
    assert_eq!(neighbors.len(), 1, "the census must not repair any edge");
    assert_eq!(neighbors[0]["id"], json!(linked.to_string()));
}

const ANNOTATION_ENCOUNTER_TIME: &str = "2026-01-02T00:00:00Z";

fn annotation_encounter_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

// These fixtures have one timestamp, so both initial and inclusive-resume
// searches select the same ordered rows. Unlike write_fake_gh, honor --limit.
fn annotation_encounter_remote(
    prs: bool,
    numbers: &[u64],
) -> (tempfile::TempDir, PathBuf, PathBuf, PathGuard) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let bin = dir.path().join("bin");
    let logs = dir.path().join("logs");
    for path in [&repo, &bin, &logs] {
        std::fs::create_dir(path).unwrap();
    }
    init_repo(&repo);
    write_fake_gh(&repo, &bin, &logs, "[]", "[]");
    let make = if prs { pr_fixture } else { issue_fixture };
    let rows = numbers
        .iter()
        .map(|number| {
            format!(
                "{}\n",
                make(*number, "existing note", ANNOTATION_ENCOUNTER_TIME)
            )
        })
        .collect::<String>();
    let rows_path = logs.join("rows.jsonl");
    std::fs::write(&rows_path, rows).unwrap();
    let operation = if prs { "pr" } else { "issue" };
    let script = format!(
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> {args_log}
if [ "$1" = repo ]; then
  [ "$2" = view ] && [ "$3" = fixture/repository ] || exit 2
  printf '%s\n' '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
  exit 0
fi
[ "$1" = {operation} ] && [ "$2" = list ] || exit 3
search= repo= limit=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --search) search="$2"; shift ;;
    --repo) repo="$2"; shift ;;
    --limit) limit="$2"; shift ;;
  esac
  shift
done
[ "$repo" = fixture/repository ] || exit 4
[ "$limit" -ge 1 ] && [ "$limit" -le 1000 ] || exit 5
case "$search" in
  'sort:updated-asc'|'sort:updated-asc updated:>={time}') ;;
  *) exit 6 ;;
esac
printf '%s\n' "$limit" >> {limits_log}
printf '['
sed -n "1,${{limit}}p" {rows} | paste -sd, -
printf ']\n'
"#,
        args_log = annotation_encounter_quote(logs.join("encounter-args.log").to_str().unwrap()),
        limits_log = annotation_encounter_quote(logs.join("limits.log").to_str().unwrap()),
        rows = annotation_encounter_quote(rows_path.to_str().unwrap()),
        time = ANNOTATION_ENCOUNTER_TIME,
    );
    // write_fake_gh already made this same file executable.
    std::fs::write(bin.join("gh"), script).unwrap();
    let path = PathGuard::install(&bin);
    (dir, repo, logs, path)
}

async fn annotation_encounter_seed(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project: Uuid,
    prs: bool,
    total: u64,
    linked: u64,
) -> Vec<Uuid> {
    assert!(linked <= total);
    let kind = if prs { "pull_request" } else { "issue" };
    let prefix = if prs { "6349" } else { "6348" };
    let mut notes = Vec::new();
    let mut edges = Vec::new();
    let mut ids = Vec::new();
    let at = chrono::DateTime::parse_from_rfc3339(ANNOTATION_ENCOUNTER_TIME)
        .unwrap()
        .with_timezone(&chrono::Utc);
    for number in 1..=total {
        let id = Uuid::parse_str(&format!("{prefix}0000-0000-4000-8000-{number:012x}")).unwrap();
        let mut note =
            khive_storage::Note::new(token.namespace().as_str(), kind, "stored original");
        note.id = id;
        note.name = Some(format!("stored {kind} #{number}"));
        note.created_at = at.timestamp_micros();
        note.updated_at = at.timestamp_micros();
        note.properties = Some(json!({
            "number": number,
            "title": format!("stored {kind} {number}"),
            "updated_at": ANNOTATION_ENCOUNTER_TIME,
            "project_id": project.to_string()
        }));
        notes.push(note);
        ids.push(id);
        if number <= linked {
            edges.push(khive_storage::types::Edge {
                id: Uuid::parse_str(&format!("{prefix}0001-0000-4000-8000-{number:012x}"))
                    .unwrap()
                    .into(),
                namespace: token.namespace().as_str().to_owned(),
                source_id: id,
                target_id: project,
                relation: khive_types::EdgeRelation::Annotates,
                weight: 1.0,
                created_at: at,
                updated_at: at,
                deleted_at: None,
                metadata: Some(json!({"fixture": "existing project annotation"})),
                target_backend: None,
            });
        }
    }
    let written = runtime
        .notes(token)
        .unwrap()
        .upsert_notes(notes)
        .await
        .unwrap();
    assert_eq!(written.attempted, total);
    assert_eq!(written.affected, total);
    assert_eq!(written.failed, 0, "{}", written.first_error);
    if !edges.is_empty() {
        let written = runtime
            .graph(token)
            .unwrap()
            .upsert_edges(edges)
            .await
            .unwrap();
        assert_eq!(written.attempted, linked);
        assert_eq!(written.affected, linked);
        assert_eq!(written.failed, 0, "{}", written.first_error);
    }
    ids
}

async fn annotation_encounter_rows(runtime: &KhiveRuntime) -> (Value, Value) {
    let sql = runtime.sql();
    let mut reader = sql.reader().await.unwrap();
    let notes = reader
        .query_all(SqlStatement::new(
            "SELECT * FROM notes ORDER BY namespace, id",
            vec![],
        ))
        .await
        .unwrap();
    let edges = reader
        .query_all(SqlStatement::new(
            "SELECT * FROM graph_edges ORDER BY namespace, id",
            vec![],
        ))
        .await
        .unwrap();
    (
        serde_json::to_value(notes).unwrap(),
        serde_json::to_value(edges).unwrap(),
    )
}

async fn annotation_encounter_checkpoint(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project: Uuid,
    prs: bool,
    acknowledged: &[(u64, Uuid)],
) -> (String, Value) {
    let kind = if prs { "prs" } else { "issues" };
    let sql = runtime.sql();
    let row = sql
        .reader()
        .await
        .unwrap()
        .query_row(SqlStatement {
            sql: include_str!("../../sql/page_checkpoint_select.sql").into(),
            params: vec![
                SqlValue::Text(project.to_string()),
                SqlValue::Text(kind.into()),
                SqlValue::Text(format!("{kind}_checkpoint")),
            ],
            label: Some("test_annotation_encounter_checkpoint".into()),
        })
        .await
        .unwrap()
        .expect("durable checkpoint pair");
    let Some(SqlValue::Text(floor)) = row.get("floor") else {
        panic!("missing floor")
    };
    let Some(SqlValue::Text(raw)) = row.get("progress") else {
        panic!("missing checkpoint")
    };
    let progress: Value = serde_json::from_str(raw).unwrap();
    let at_floor = acknowledged
        .iter()
        .map(|(number, id)| (number.to_string(), json!(id.to_string())))
        .collect::<serde_json::Map<_, _>>();
    assert_eq!(floor, ANNOTATION_ENCOUNTER_TIME);
    assert_eq!(
        progress,
        json!({
            "version": 1, "namespace": token.namespace().as_str(),
            "floor": ANNOTATION_ENCOUNTER_TIME, "at_floor": at_floor, "undated": {}
        })
    );
    (floor.clone(), progress)
}

fn annotation_encounter_params(repo: &Path, project: Uuid, prs: bool, max_items: u64) -> Value {
    json!({
        "source": repo.to_str().unwrap(), "project": project.to_string(),
        "include": [if prs { "pull_requests" } else { "issues" }], "max_items": max_items
    })
}

fn annotation_encounter_missing(prs: bool, number: u64, note: Uuid, project: Uuid) -> String {
    let kind = if prs { "pull_request" } else { "issue" };
    format!(
        "existing {kind} #{number} note {note} is missing a live project annotation to {project}"
    )
}

fn annotation_encounter_warnings(report: &Value) -> Vec<&str> {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning.as_str().unwrap())
        .filter(|warning| warning.starts_with("existing "))
        .collect()
}

async fn annotation_encounter_receipt(registry: &VerbRegistry, report: &Value) -> Value {
    let receipt_id = report["receipt_id"]
        .as_str()
        .expect("durable digest receipt");
    assert_eq!(Uuid::parse_str(receipt_id).unwrap().to_string(), receipt_id);
    let event = registry
        .dispatch("get", json!({"id": receipt_id}))
        .await
        .unwrap();
    assert_eq!(event["kind"], "event");
    assert_eq!(&event["payload"]["result"], report);
    event
}

fn annotation_encounter_state(report: &Value, prs: bool, skipped: u64, done: bool) {
    assert_eq!(report["prs_ingested"], 0);
    assert_eq!(report["issues_ingested"], 0);
    assert_eq!(report["commits_ingested"], 0);
    assert_eq!(
        report[if prs {
            "prs_skipped_existing"
        } else {
            "issues_skipped_existing"
        }],
        skipped
    );
    assert_eq!(
        report[if prs {
            "issues_skipped_existing"
        } else {
            "prs_skipped_existing"
        }],
        0
    );
    assert_eq!(report["done"], done);
    assert_eq!(report["history_exhausted"], done);
    assert_eq!(report["cursor_stalled"], false);
    assert_eq!(
        report["sources"][if prs { "pull_requests" } else { "issues" }]["state"],
        if done { "completed" } else { "stopped_early" }
    );
}

fn annotation_encounter_fetches(logs: &Path, prs: bool, expected: &[u64]) {
    let limits = std::fs::read_to_string(logs.join("limits.log"))
        .unwrap()
        .lines()
        .map(|line| line.parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(limits, expected);
    let operation = if prs { "pr" } else { "issue" };
    let args = std::fs::read_to_string(logs.join("encounter-args.log")).unwrap();
    let fetches = args
        .lines()
        .filter(|line| line.starts_with(&format!("{operation} list ")))
        .collect::<Vec<_>>();
    assert_eq!(fetches.len(), expected.len());
    for (line, limit) in fetches.iter().zip(expected) {
        assert_eq!(repo_flag_value(line), Some("fixture/repository"));
        let words = line.split_whitespace().collect::<Vec<_>>();
        let position = words.iter().position(|word| *word == "--limit").unwrap();
        assert_eq!(words[position + 1].parse::<u64>().unwrap(), *limit);
        assert!(line.contains("--search sort:updated-asc"));
    }
}

async fn annotation_encounter_above_prefix(prs: bool) {
    let _lock = ENV_MUTEX.lock().await;
    let (runtime, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind":"project", "name":"annotation prefix"}),
    )
    .await;
    let ids = annotation_encounter_seed(&runtime, &token, project, prs, 1002, 1001).await;
    assert!(ids
        .windows(2)
        .all(|pair| pair[0].to_string() < pair[1].to_string()));
    let target = ids[1001];
    let before = annotation_encounter_rows(&runtime).await;
    assert_eq!(before.0.as_array().unwrap().len(), 1002);
    assert_eq!(before.1.as_array().unwrap().len(), 1001);
    let (_dir, repo, logs, _path) = annotation_encounter_remote(prs, &[1002]);
    let report = registry
        .dispatch(
            "git.digest",
            annotation_encounter_params(&repo, project, prs, 10),
        )
        .await
        .expect("digest existing high-ID note");
    annotation_encounter_state(&report, prs, 1, true);
    assert_eq!(
        annotation_encounter_warnings(&report),
        vec![annotation_encounter_missing(prs, 1002, target, project)]
    );
    assert_eq!(report["project_annotation_notes_scanned"], 1000);
    assert_eq!(report["missing_project_annotation_count"], 0);
    assert_eq!(report["missing_project_annotation_note_ids"], json!([]));
    assert_eq!(report["project_annotation_census_complete"], false);
    assert_eq!(report["existing_project_annotation_warning_count"], 1);
    let checkpoint =
        annotation_encounter_checkpoint(&runtime, &token, project, prs, &[(1002, target)]).await;
    let mut receipts = vec![annotation_encounter_receipt(&registry, &report).await];
    assert_eq!(annotation_encounter_rows(&runtime).await, before);
    for _ in 0..2 {
        let replay = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 10),
            )
            .await
            .unwrap();
        annotation_encounter_state(&replay, prs, 0, true);
        assert_eq!(replay["existing_project_annotation_warning_count"], 1);
        assert_eq!(
            annotation_encounter_warnings(&replay),
            vec![annotation_encounter_missing(prs, 1002, target, project)]
        );
        assert_eq!(
            annotation_encounter_checkpoint(&runtime, &token, project, prs, &[(1002, target)])
                .await,
            checkpoint
        );
        receipts.push(annotation_encounter_receipt(&registry, &replay).await);
        assert_eq!(annotation_encounter_rows(&runtime).await, before);
    }
    // The acknowledgement is unchanged: only the actual edge changes whether
    // the next read warns. Each deliberate fixture mutation precedes its baseline.
    for deleted in [false, true] {
        annotation_replay_link(&runtime, &token, project, &[target], deleted).await;
        let current = annotation_encounter_rows(&runtime).await;
        let replay = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 10),
            )
            .await
            .unwrap();
        annotation_encounter_state(&replay, prs, 0, true);
        assert_eq!(
            replay["existing_project_annotation_warning_count"],
            u64::from(deleted)
        );
        let expected = if deleted {
            vec![annotation_encounter_missing(prs, 1002, target, project)]
        } else {
            vec![]
        };
        assert_eq!(annotation_encounter_warnings(&replay), expected);
        assert_eq!(
            annotation_encounter_checkpoint(&runtime, &token, project, prs, &[(1002, target)])
                .await,
            checkpoint
        );
        receipts.push(annotation_encounter_receipt(&registry, &replay).await);
        assert_eq!(annotation_encounter_rows(&runtime).await, current);
    }
    registry
        .dispatch("delete", json!({"id":target.to_string()}))
        .await
        .unwrap();
    let current = annotation_encounter_rows(&runtime).await;
    let unavailable = registry
        .dispatch(
            "git.digest",
            annotation_encounter_params(&repo, project, prs, 10),
        )
        .await
        .unwrap();
    annotation_encounter_state(&unavailable, prs, 0, true);
    assert_eq!(unavailable["existing_project_annotation_warning_count"], 1);
    assert_eq!(annotation_encounter_warnings(&unavailable),vec![format!("existing {} #1002 note {target} project annotation check unavailable: existing note changed",if prs {"pull_request"} else {"issue"})]);
    assert_eq!(
        annotation_encounter_checkpoint(&runtime, &token, project, prs, &[(1002, target)]).await,
        checkpoint
    );
    receipts.push(annotation_encounter_receipt(&registry, &unavailable).await);
    assert_eq!(annotation_encounter_rows(&runtime).await, current);
    annotation_replay_receipts_unchanged(&registry, &receipts).await;
    annotation_encounter_fetches(&logs, prs, &[10, 11, 11, 11, 11, 11]);
    registry.shutdown_audit_batch().await.unwrap();
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_issue_annotation_warning_reaches_note_above_census_prefix() {
    if crate::test_process::run_in_child() {
        return;
    }
    annotation_encounter_above_prefix(false).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_pr_annotation_warning_reaches_note_above_census_prefix() {
    if crate::test_process::run_in_child() {
        return;
    }
    annotation_encounter_above_prefix(true).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_warning_preserves_budget_and_acknowledgment_semantics() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (runtime, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project", "name":"annotation budget"}),
        )
        .await;
        let ids = annotation_encounter_seed(&runtime, &token, project, prs, 2, 0).await;
        let before = annotation_encounter_rows(&runtime).await;
        assert_eq!(before.0.as_array().unwrap().len(), 2);
        assert_eq!(before.1.as_array().unwrap().len(), 0);
        let (_dir, repo, logs, _path) = annotation_encounter_remote(prs, &[1, 2]);
        let mut completed_checkpoint = None;
        let mut receipts = Vec::new();
        for pass in 0..3 {
            let report = registry
                .dispatch(
                    "git.digest",
                    annotation_encounter_params(&repo, project, prs, 1),
                )
                .await
                .expect("budgeted existing-note digest");
            annotation_encounter_state(&report, prs, u64::from(pass < 2), pass == 2);
            let expected = (0..if pass == 0 { 1 } else { 2 })
                .map(|index| {
                    annotation_encounter_missing(prs, index as u64 + 1, ids[index], project)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                annotation_encounter_warnings(&report),
                expected,
                "fresh hits and initially acknowledged observations both warn"
            );
            assert_eq!(
                report["existing_project_annotation_warning_count"],
                expected.len()
            );
            let acknowledgments = if pass == 0 {
                vec![(1, ids[0])]
            } else {
                vec![(1, ids[0]), (2, ids[1])]
            };
            let checkpoint =
                annotation_encounter_checkpoint(&runtime, &token, project, prs, &acknowledgments)
                    .await;
            if pass == 1 {
                completed_checkpoint = Some(checkpoint.clone());
            }
            if pass == 2 {
                assert_eq!(Some(checkpoint), completed_checkpoint);
            }
            receipts.push(annotation_encounter_receipt(&registry, &report).await);
            annotation_replay_receipts_unchanged(&registry, &receipts).await;
            assert_eq!(annotation_encounter_rows(&runtime).await, before);
        }
        annotation_encounter_fetches(&logs, prs, &[1, 2, 3]);
        registry.shutdown_audit_batch().await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_warning_reports_unavailable_graph_reads() {
    use khive_db::test_hooks::TestHooks;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    if crate::test_process::run_in_child() {
        return;
    }
    let _lock = ENV_MUTEX.lock().await;
    let (runtime, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind":"project", "name":"annotation unavailable"}),
    )
    .await;
    let ids = annotation_encounter_seed(&runtime, &token, project, false, 2, 2).await;
    let before = annotation_encounter_rows(&runtime).await;
    assert_eq!(before.0.as_array().unwrap().len(), 2);
    assert_eq!(before.1.as_array().unwrap().len(), 2);
    let (_dir, repo, logs, _path) = annotation_encounter_remote(false, &[1, 2]);
    // In-memory readers share this writer connection. Refuse only the first
    // graph read: the per-note check must warn, while the mandatory commit
    // count and the later census still read the real graph successfully.
    assert_eq!(runtime.backend().pool().max_readers(), 0);
    let denied_graph_reads = Arc::new(AtomicUsize::new(0));
    let denied = Arc::clone(&denied_graph_reads);
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit_sqlite(|conn| {
            conn.test_authorizer(Some(move |context: AuthContext<'_>| match context.action {
                AuthAction::Read { table_name, .. }
                    if table_name == "graph_edges"
                        && denied
                            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok() =>
                {
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }))
        })
        .unwrap();
    let outcome = registry
        .dispatch(
            "git.digest",
            annotation_encounter_params(&repo, project, false, 1),
        )
        .await;
    // Remove the fault even if the real dispatch failed, before inspecting it.
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit_sqlite(|conn| {
            conn.test_authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        })
        .unwrap();
    assert_eq!(denied_graph_reads.load(Ordering::SeqCst), 1);
    let report = outcome.expect("annotation read failure remains an explicit warning");
    assert_eq!(report["existing_project_annotation_warning_count"], 1);
    annotation_encounter_state(&report, false, 1, false);
    let warnings = annotation_encounter_warnings(&report);
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].starts_with(&format!(
            "existing issue #1 note {} project annotation check unavailable: ",
            ids[0]
        )),
        "{warnings:?}"
    );
    assert!(!warnings[0].contains("is missing a live project annotation"));
    assert!(report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|warning| !warning
            .as_str()
            .unwrap()
            .starts_with("project annotation census unavailable: ")));
    assert_eq!(report["commits_total_in_db"], 0);
    assert_eq!(report["missing_project_annotation_count"], 0);
    assert_eq!(report["missing_project_annotation_note_ids"], json!([]));
    assert_eq!(report["project_annotation_notes_scanned"], 2);
    assert_eq!(report["project_annotation_census_complete"], true);
    annotation_encounter_checkpoint(&runtime, &token, project, false, &[(1, ids[0])]).await;
    annotation_encounter_receipt(&registry, &report).await;
    assert_eq!(annotation_encounter_rows(&runtime).await, before);

    // Replay of #1 is free; #2 reaches the now-working lookup and linked edge.
    let retry = registry
        .dispatch(
            "git.digest",
            annotation_encounter_params(&repo, project, false, 10),
        )
        .await
        .unwrap();
    annotation_encounter_state(&retry, false, 1, true);
    assert!(annotation_encounter_warnings(&retry).is_empty());
    assert_eq!(retry["existing_project_annotation_warning_count"], 0);
    assert_eq!(retry["missing_project_annotation_count"], 0);
    assert_eq!(retry["project_annotation_notes_scanned"], 2);
    assert_eq!(retry["project_annotation_census_complete"], true);
    annotation_encounter_checkpoint(
        &runtime,
        &token,
        project,
        false,
        &[(1, ids[0]), (2, ids[1])],
    )
    .await;
    annotation_encounter_receipt(&registry, &retry).await;
    assert_eq!(annotation_encounter_rows(&runtime).await, before);
    annotation_encounter_fetches(&logs, false, &[1, 11]);
    registry.shutdown_audit_batch().await.unwrap();
}

// Select each page from its actual inclusive search floor, rather than from a
// fixture call counter. Undated observations remain visible on later pages.
fn annotation_replay_remote() -> (tempfile::TempDir, PathBuf, PathBuf, PathGuard) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let bin = dir.path().join("bin");
    let logs = dir.path().join("logs");
    for path in [&repo, &bin, &logs] {
        std::fs::create_dir(path).unwrap();
    }
    init_repo(&repo);
    write_fake_gh(&repo, &bin, &logs, "[]", "[]");
    for source in ["pr", "issue"] {
        std::fs::write(logs.join(format!("{source}.tsv")), "").unwrap();
    }
    let script = format!(
        r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> {args_log}
if [ "$1" = repo ]; then
  [ "$2" = view ] && [ "$3" = fixture/repository ] || exit 2
  printf '%s\n' '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
  exit 0
fi
source="$1"
case "$source" in pr|issue) ;; *) exit 3 ;; esac
[ "$2" = list ] || exit 4
search= repo= limit=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --search) search="$2"; shift ;;
    --repo) repo="$2"; shift ;;
    --limit) limit="$2"; shift ;;
  esac
  shift
done
[ "$repo" = fixture/repository ] || exit 5
[ "$limit" -ge 1 ] && [ "$limit" -le 1000 ] || exit 6
case "$search" in
  'sort:updated-asc') floor= ;;
  'sort:updated-asc updated:>='*) floor="${{search#sort:updated-asc updated:>=}}" ;;
  *) exit 7 ;;
esac
printf '%s\t%s\t%s\n' "$source" "$search" "$limit" >> {pages_log}
printf '['
awk -F '\t' -v floor="$floor" -v limit="$limit" \
  '($1 == "-" || floor == "" || $1 >= floor) && n++ < limit {{ print $2 }}' \
  {logs}/"$source.tsv" | paste -sd, -
printf ']\n'
"#,
        args_log = annotation_encounter_quote(logs.join("args.log").to_str().unwrap()),
        pages_log = annotation_encounter_quote(logs.join("pages.log").to_str().unwrap()),
        logs = annotation_encounter_quote(logs.to_str().unwrap()),
    );
    std::fs::write(bin.join("gh"), script).unwrap();
    let path = PathGuard::install(&bin);
    (dir, repo, logs, path)
}

fn annotation_replay_remote_rows(logs: &Path, prs: bool, rows: &[(u64, Option<String>)]) {
    let make = if prs { pr_fixture } else { issue_fixture };
    let mut sorted = rows.to_vec();
    sorted.sort_by(|left, right| left.1.cmp(&right.1).then(left.0.cmp(&right.0)));
    let mut text = String::new();
    for (number, updated_at) in sorted {
        let mut value = make(number, "existing note", ANNOTATION_ENCOUNTER_TIME);
        value["updatedAt"] = json!(updated_at);
        text.push_str(updated_at.as_deref().unwrap_or("-"));
        text.push('\t');
        text.push_str(&value.to_string());
        text.push('\n');
    }
    std::fs::write(logs.join(if prs { "pr.tsv" } else { "issue.tsv" }), text).unwrap();
}

fn annotation_replay_pages(logs: &Path, expected: &[(bool, Option<&str>, u64)]) {
    let expected = expected
        .iter()
        .map(|(prs, floor, limit)| {
            let source = if *prs { "pr" } else { "issue" };
            let search = floor.map_or_else(
                || "sort:updated-asc".to_owned(),
                |floor| format!("sort:updated-asc updated:>={floor}"),
            );
            format!("{source}\t{search}\t{limit}")
        })
        .collect::<Vec<_>>();
    let actual = std::fs::read_to_string(logs.join("pages.log")).unwrap();
    assert_eq!(actual.lines().collect::<Vec<_>>(), expected);
    let argv = std::fs::read_to_string(logs.join("args.log")).unwrap();
    let fetches = argv
        .lines()
        .filter(|line| line.starts_with("pr list ") || line.starts_with("issue list "))
        .collect::<Vec<_>>();
    assert_eq!(fetches.len(), expected.len());
    for line in fetches {
        assert_eq!(repo_flag_value(line), Some("fixture/repository"));
    }
}

fn annotation_replay_time(index: u64) -> String {
    let start = chrono::DateTime::parse_from_rfc3339(ANNOTATION_ENCOUNTER_TIME).unwrap();
    (start + chrono::Duration::seconds(index as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

async fn annotation_replay_checkpoint(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project: Uuid,
    prs: bool,
    floor: Option<&str>,
    at_floor: &[(u64, Uuid)],
    undated: &[(u64, Uuid)],
) -> Value {
    let source = if prs { "prs" } else { "issues" };
    let sql = runtime.sql();
    let row = sql
        .reader()
        .await
        .unwrap()
        .query_row(SqlStatement::new(
            include_str!("../../sql/page_checkpoint_select.sql"),
            vec![
                SqlValue::Text(project.to_string()),
                SqlValue::Text(source.into()),
                SqlValue::Text(format!("{source}_checkpoint")),
            ],
        ))
        .await
        .unwrap()
        .expect("actual digest checkpoint");
    match (floor, row.get("floor")) {
        (Some(expected), Some(SqlValue::Text(actual))) => assert_eq!(actual, expected),
        (None, Some(SqlValue::Null)) => {}
        other => panic!("unexpected checkpoint floor: {other:?}"),
    }
    let Some(SqlValue::Text(raw)) = row.get("progress") else {
        panic!("missing persisted progress")
    };
    let map = |pairs: &[(u64, Uuid)]| {
        pairs
            .iter()
            .map(|(number, id)| (number.to_string(), json!(id.to_string())))
            .collect::<serde_json::Map<_, _>>()
    };
    let progress: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(
        progress,
        json!({"version":1, "namespace":token.namespace().as_str(),
            "floor":floor, "at_floor":map(at_floor), "undated":map(undated)})
    );
    progress
}

async fn annotation_replay_link(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    project: Uuid,
    ids: &[Uuid],
    deleted: bool,
) {
    let at = chrono::DateTime::parse_from_rfc3339(ANNOTATION_ENCOUNTER_TIME)
        .unwrap()
        .with_timezone(&chrono::Utc);
    let edges = ids
        .iter()
        .map(|id| khive_storage::types::Edge {
            id: Uuid::from_u128(id.as_u128() ^ (2_u128 << 96)).into(),
            namespace: token.namespace().as_str().into(),
            source_id: *id,
            target_id: project,
            relation: khive_types::EdgeRelation::Annotates,
            weight: 1.0,
            created_at: at,
            updated_at: at + chrono::Duration::seconds(1),
            deleted_at: deleted.then_some(at + chrono::Duration::seconds(1)),
            metadata: Some(json!({"fixture":"replay annotation"})),
            target_backend: None,
        })
        .collect::<Vec<_>>();
    let summary = runtime
        .graph(token)
        .unwrap()
        .upsert_edges(edges)
        .await
        .unwrap();
    assert_eq!(summary.attempted, ids.len() as u64);
    assert_eq!(summary.affected, ids.len() as u64);
    assert_eq!(summary.failed, 0, "{}", summary.first_error);
}

fn annotation_replay_exhaustions(report: &Value) -> Vec<&str> {
    report["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .filter(|warning| warning.contains("project annotation replay check limit"))
        .collect()
}

fn annotation_replay_limit_warning(prs: bool) -> String {
    format!(
        "{}: project annotation replay check limit of 128 reached; additional acknowledged records were not checked",
        if prs { "prs" } else { "issues" }
    )
}

async fn annotation_replay_receipts_unchanged(registry: &VerbRegistry, receipts: &[Value]) {
    for receipt in receipts {
        let reread = registry
            .dispatch(
                "get",
                json!({"id":receipt["payload"]["result"]["receipt_id"]}),
            )
            .await
            .unwrap();
        assert_eq!(&reread, receipt, "earlier complete receipt is immutable");
    }
}

fn annotation_replay_deny_first_graph_read(runtime: &KhiveRuntime) -> Arc<AtomicUsize> {
    use khive_db::test_hooks::TestHooks;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    assert_eq!(runtime.backend().pool().max_readers(), 0);
    let denied = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&denied);
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit_sqlite(|conn| {
            conn.test_authorizer(Some(move |context: AuthContext<'_>| match context.action {
                AuthAction::Read { table_name, .. }
                    if table_name == "graph_edges"
                        && count
                            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok() =>
                {
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }))
        })
        .unwrap();
    denied
}

fn annotation_replay_clear_graph_fault(runtime: &KhiveRuntime) {
    use khive_db::test_hooks::TestHooks;
    use rusqlite::hooks::{AuthContext, Authorization};

    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit_sqlite(|conn| {
            conn.test_authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        })
        .unwrap();
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_replay_attempt_limit_counts_healthy_and_unavailable_reads() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        for mode in ["missing", "healthy", "unavailable"] {
            let (runtime, token, registry) = fixture().await;
            let project = create(
                &registry,
                json!({"kind":"project","name":"replay attempt limit"}),
            )
            .await;
            let ids = annotation_encounter_seed(
                &runtime,
                &token,
                project,
                prs,
                129,
                if mode == "missing" { 0 } else { 128 },
            )
            .await;
            let before = annotation_encounter_rows(&runtime).await;
            let (_dir, repo, logs, _path) =
                annotation_encounter_remote(prs, &(1..=129).collect::<Vec<_>>());
            let first = registry
                .dispatch(
                    "git.digest",
                    annotation_encounter_params(&repo, project, prs, 2000),
                )
                .await
                .unwrap();
            annotation_encounter_state(&first, prs, 129, true);
            assert_eq!(
                first["existing_project_annotation_warning_count"],
                if mode == "missing" { 129 } else { 1 }
            );
            assert!(
                annotation_replay_exhaustions(&first).is_empty(),
                "fresh hits do not use replay allowance"
            );
            let acknowledgments = ids
                .iter()
                .enumerate()
                .map(|(index, id)| (index as u64 + 1, *id))
                .collect::<Vec<_>>();
            let checkpoint =
                annotation_encounter_checkpoint(&runtime, &token, project, prs, &acknowledgments)
                    .await;
            let mut receipts = vec![annotation_encounter_receipt(&registry, &first).await];
            for _ in 0..2 {
                let denied = (mode == "unavailable")
                    .then(|| annotation_replay_deny_first_graph_read(&runtime));
                let outcome = registry
                    .dispatch(
                        "git.digest",
                        annotation_encounter_params(&repo, project, prs, 1),
                    )
                    .await;
                if let Some(denied) = denied {
                    annotation_replay_clear_graph_fault(&runtime);
                    assert_eq!(denied.load(Ordering::SeqCst), 1);
                }
                let report = outcome.expect("bounded replay remains a successful digest");
                annotation_encounter_state(&report, prs, 0, true);
                assert_eq!(
                    annotation_replay_exhaustions(&report),
                    vec![annotation_replay_limit_warning(prs)]
                );
                let warnings = annotation_encounter_warnings(&report);
                match mode {
                    "missing" => {
                        assert_eq!(report["existing_project_annotation_warning_count"], 128);
                        assert_eq!(
                            warnings,
                            (0..20)
                                .map(|index| annotation_encounter_missing(
                                    prs,
                                    index as u64 + 1,
                                    ids[index],
                                    project
                                ))
                                .collect::<Vec<_>>()
                        );
                    }
                    "healthy" => {
                        assert_eq!(report["existing_project_annotation_warning_count"], 0);
                        assert!(
                            warnings.is_empty(),
                            "the unchecked missing 129th row is not an observed warning"
                        );
                    }
                    "unavailable" => {
                        assert_eq!(report["existing_project_annotation_warning_count"], 1);
                        assert_eq!(warnings.len(), 1);
                        assert!(warnings[0].starts_with(&format!(
                            "existing {} #1 note {} project annotation check unavailable: ",
                            if prs { "pull_request" } else { "issue" },
                            ids[0]
                        )));
                    }
                    _ => unreachable!(),
                }
                assert_eq!(
                    annotation_encounter_checkpoint(
                        &runtime,
                        &token,
                        project,
                        prs,
                        &acknowledgments
                    )
                    .await,
                    checkpoint
                );
                receipts.push(annotation_encounter_receipt(&registry, &report).await);
                annotation_replay_receipts_unchanged(&registry, &receipts).await;
                assert_eq!(annotation_encounter_rows(&runtime).await, before);
            }
            annotation_encounter_fetches(&logs, prs, &[1000, 130, 130]);
            registry.shutdown_audit_batch().await.unwrap();
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_replay_limit_survives_real_page_transitions() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (runtime, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"replay page boundary"}),
        )
        .await;
        let ids = annotation_encounter_seed(&runtime, &token, project, prs, 1000, 0).await;
        annotation_replay_link(&runtime, &token, project, &ids[129..], false).await;
        let before = annotation_encounter_rows(&runtime).await;
        let (_dir, repo, logs, _path) = annotation_replay_remote();
        let undated = (1..=129).map(|number| (number, None)).collect::<Vec<_>>();
        annotation_replay_remote_rows(&logs, prs, &undated);
        let first = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 2000),
            )
            .await
            .unwrap();
        annotation_encounter_state(&first, prs, 129, true);
        assert_eq!(first["existing_project_annotation_warning_count"], 129);
        let acknowledgments = ids[..129]
            .iter()
            .enumerate()
            .map(|(index, id)| (index as u64 + 1, *id))
            .collect::<Vec<_>>();
        annotation_replay_checkpoint(&runtime, &token, project, prs, None, &[], &acknowledgments)
            .await;
        let first_receipt = annotation_encounter_receipt(&registry, &first).await;
        let mut rows = undated;
        rows.extend(
            (130..=1000).map(|number| (number, Some(annotation_replay_time(number - 129)))),
        );
        annotation_replay_remote_rows(&logs, prs, &rows);
        let report = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 2000),
            )
            .await
            .unwrap();
        annotation_encounter_state(&report, prs, 871, true);
        assert_eq!(
            report["existing_project_annotation_warning_count"], 128,
            "page two must not reset replay attempts"
        );
        assert_eq!(
            annotation_encounter_warnings(&report),
            (0..20)
                .map(|index| annotation_encounter_missing(
                    prs,
                    index as u64 + 1,
                    ids[index],
                    project
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            annotation_replay_exhaustions(&report),
            vec![annotation_replay_limit_warning(prs)]
        );
        let last = annotation_replay_time(871);
        annotation_replay_checkpoint(
            &runtime,
            &token,
            project,
            prs,
            Some(&last),
            &[(1000, ids[999])],
            &acknowledgments,
        )
        .await;
        annotation_replay_pages(
            &logs,
            &[
                (prs, None, 1000),
                (prs, None, 1000),
                (prs, Some(&last), 1000),
            ],
        );
        let receipt = annotation_encounter_receipt(&registry, &report).await;
        annotation_replay_receipts_unchanged(&registry, &[first_receipt, receipt]).await;
        assert_eq!(annotation_encounter_rows(&runtime).await, before);
        registry.shutdown_audit_batch().await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_replay_uses_initial_observations_across_real_pages() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (runtime, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"initial replay observation"}),
        )
        .await;
        let ids = annotation_encounter_seed(&runtime, &token, project, prs, 1000, 0).await;
        annotation_replay_link(&runtime, &token, project, &ids[2..], false).await;
        let before = annotation_encounter_rows(&runtime).await;
        let (_dir, repo, logs, _path) = annotation_replay_remote();
        annotation_replay_remote_rows(&logs, prs, &[(1, None), (2, None)]);
        let first = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 2000),
            )
            .await
            .unwrap();
        annotation_encounter_state(&first, prs, 2, true);
        assert_eq!(first["existing_project_annotation_warning_count"], 2);
        annotation_replay_checkpoint(
            &runtime,
            &token,
            project,
            prs,
            None,
            &[],
            &[(1, ids[0]), (2, ids[1])],
        )
        .await;
        let first_receipt = annotation_encounter_receipt(&registry, &first).await;
        let last = annotation_replay_time(999);
        let mut rows = vec![(1, None), (2, Some(last.clone()))];
        rows.extend((3..=1000).map(|number| (number, Some(annotation_replay_time(number - 2)))));
        annotation_replay_remote_rows(&logs, prs, &rows);
        let report = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 2000),
            )
            .await
            .unwrap();
        annotation_encounter_state(&report, prs, 999, true);
        assert_eq!(
            report["existing_project_annotation_warning_count"], 2,
            "the changed observation is fresh once, not an initial replay on page two"
        );
        assert_eq!(
            annotation_encounter_warnings(&report),
            vec![
                annotation_encounter_missing(prs, 1, ids[0], project),
                annotation_encounter_missing(prs, 2, ids[1], project)
            ]
        );
        assert!(annotation_replay_exhaustions(&report).is_empty());
        annotation_replay_checkpoint(
            &runtime,
            &token,
            project,
            prs,
            Some(&last),
            &[(2, ids[1])],
            &[(1, ids[0])],
        )
        .await;
        annotation_replay_pages(
            &logs,
            &[
                (prs, None, 1000),
                (prs, None, 1000),
                (prs, Some(&last), 1000),
            ],
        );
        let receipt = annotation_encounter_receipt(&registry, &report).await;
        annotation_replay_receipts_unchanged(&registry, &[first_receipt, receipt]).await;
        assert_eq!(annotation_encounter_rows(&runtime).await, before);
        registry.shutdown_audit_batch().await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_warning_details_are_report_wide_and_count_all_outcomes() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (runtime, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"fresh warning details"}),
        )
        .await;
        let ids = annotation_encounter_seed(&runtime, &token, project, prs, 25, 0).await;
        let before = annotation_encounter_rows(&runtime).await;
        let (_dir, repo, logs, _path) =
            annotation_encounter_remote(prs, &(1..=25).collect::<Vec<_>>());
        let report = registry
            .dispatch(
                "git.digest",
                annotation_encounter_params(&repo, project, prs, 2000),
            )
            .await
            .unwrap();
        annotation_encounter_state(&report, prs, 25, true);
        assert_eq!(report["existing_project_annotation_warning_count"], 25);
        assert_eq!(
            annotation_encounter_warnings(&report),
            (0..20)
                .map(|index| annotation_encounter_missing(
                    prs,
                    index as u64 + 1,
                    ids[index],
                    project
                ))
                .collect::<Vec<_>>()
        );
        assert!(annotation_replay_exhaustions(&report).is_empty());
        let acknowledgments = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (index as u64 + 1, *id))
            .collect::<Vec<_>>();
        annotation_encounter_checkpoint(&runtime, &token, project, prs, &acknowledgments).await;
        annotation_encounter_receipt(&registry, &report).await;
        assert_eq!(annotation_encounter_rows(&runtime).await, before);
        annotation_encounter_fetches(&logs, prs, &[1000]);
        registry.shutdown_audit_batch().await.unwrap();
    }
    let (runtime, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind":"project","name":"combined warning details"}),
    )
    .await;
    let prs = annotation_encounter_seed(&runtime, &token, project, true, 13, 0).await;
    let issues = annotation_encounter_seed(&runtime, &token, project, false, 14, 0).await;
    annotation_replay_link(&runtime, &token, project, &[prs[12], issues[13]], false).await;
    let before = annotation_encounter_rows(&runtime).await;
    let (_dir, repo, logs, _path) = annotation_replay_remote();
    let at = Some(ANNOTATION_ENCOUNTER_TIME.to_owned());
    for source in [true, false] {
        annotation_replay_remote_rows(&logs, source, &[(1, at.clone())]);
    }
    let params = json!({"source":repo.to_str().unwrap(),"project":project.to_string(),"include":["pull_requests","issues"],"max_items":2001});
    let first = registry
        .dispatch("git.digest", params.clone())
        .await
        .unwrap();
    assert_eq!(first["existing_project_annotation_warning_count"], 2);
    let first_receipt = annotation_encounter_receipt(&registry, &first).await;
    for source in [true, false] {
        let mut rows = (1..=13)
            .map(|number| (number, at.clone()))
            .collect::<Vec<_>>();
        if !source {
            // Canonicalization emits this unrelated warning before any issue
            // point check, unlike the handler's later max_items warning.
            rows.push((14, Some("invalid-timestamp".to_owned())));
        }
        annotation_replay_remote_rows(&logs, source, &rows);
    }
    let denied = annotation_replay_deny_first_graph_read(&runtime);
    let outcome = registry.dispatch("git.digest", params).await;
    annotation_replay_clear_graph_fault(&runtime);
    assert_eq!(denied.load(Ordering::SeqCst), 1);
    let report = outcome.expect("mixed fresh/replay diagnostics preserve digest completion");
    assert_eq!(report["prs_ingested"], 0);
    assert_eq!(report["issues_ingested"], 0);
    assert_eq!(report["commits_ingested"], 0);
    assert_eq!(report["prs_skipped_existing"], 12);
    assert_eq!(report["issues_skipped_existing"], 13);
    assert_eq!(report["done"], true);
    assert_eq!(report["history_exhausted"], true);
    assert_eq!(report["cursor_stalled"], false);
    for source in ["pull_requests", "issues"] {
        assert_eq!(report["sources"][source]["state"], "completed");
    }
    assert_eq!(report["existing_project_annotation_warning_count"], 25);
    let warnings = annotation_encounter_warnings(&report);
    assert_eq!(warnings.len(), 20);
    assert!(warnings[0].starts_with(&format!(
        "existing pull_request #1 note {} project annotation check unavailable: ",
        prs[0]
    )));
    let mut expected = (2..=12)
        .map(|number| annotation_encounter_missing(true, number, prs[number as usize - 1], project))
        .collect::<Vec<_>>();
    expected.extend((1..=8).map(|number| {
        annotation_encounter_missing(false, number, issues[number as usize - 1], project)
    }));
    assert_eq!(&warnings[1..], expected);
    assert!(annotation_replay_exhaustions(&report).is_empty());
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| {
                let warning = warning.as_str().unwrap();
                warning.contains("issue #14")
                    && warning.contains("updatedAt")
                    && warning.contains("not a valid")
            }),
        "earlier timestamp warning must survive the detail cap"
    );
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("max_items")
                && warning.as_str().unwrap().contains("2000")),
        "unrelated clamping warning must survive the detail cap"
    );
    for (source, ids) in [(true, &prs[..]), (false, &issues[..13])] {
        let pairs = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (index as u64 + 1, *id))
            .collect::<Vec<_>>();
        annotation_replay_checkpoint(
            &runtime,
            &token,
            project,
            source,
            Some(ANNOTATION_ENCOUNTER_TIME),
            &pairs,
            if source { &[] } else { &[(14, issues[13])] },
        )
        .await;
    }
    annotation_replay_pages(
        &logs,
        &[
            (true, None, 1000),
            (false, None, 1000),
            (true, Some(ANNOTATION_ENCOUNTER_TIME), 1000),
            (false, Some(ANNOTATION_ENCOUNTER_TIME), 1000),
        ],
    );
    let receipt = annotation_encounter_receipt(&registry, &report).await;
    annotation_replay_receipts_unchanged(&registry, &[first_receipt, receipt]).await;
    assert_eq!(annotation_encounter_rows(&runtime).await, before);
    registry.shutdown_audit_batch().await.unwrap();
}
