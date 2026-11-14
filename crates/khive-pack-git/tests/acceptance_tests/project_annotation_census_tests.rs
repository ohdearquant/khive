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
    let mut notes = Vec::new();
    let mut edges = Vec::new();
    let mut ids = Vec::new();
    let at = chrono::DateTime::parse_from_rfc3339(ANNOTATION_ENCOUNTER_TIME)
        .unwrap()
        .with_timezone(&chrono::Utc);
    for number in 1..=total {
        let id = Uuid::parse_str(&format!("63480000-0000-4000-8000-{number:012x}")).unwrap();
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
                id: Uuid::parse_str(&format!("63480001-0000-4000-8000-{number:012x}"))
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

async fn annotation_encounter_receipt(registry: &VerbRegistry, report: &Value) {
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
    annotation_encounter_checkpoint(&runtime, &token, project, prs, &[(1002, target)]).await;
    annotation_encounter_receipt(&registry, &report).await;
    assert_eq!(annotation_encounter_rows(&runtime).await, before);
    annotation_encounter_fetches(&logs, prs, &[10]);
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
        for pass in 0..3 {
            let report = registry
                .dispatch(
                    "git.digest",
                    annotation_encounter_params(&repo, project, prs, 1),
                )
                .await
                .expect("budgeted existing-note digest");
            annotation_encounter_state(&report, prs, u64::from(pass < 2), pass == 2);
            let expected = if pass < 2 {
                vec![annotation_encounter_missing(
                    prs,
                    pass as u64 + 1,
                    ids[pass],
                    project,
                )]
            } else {
                vec![]
            };
            assert_eq!(
                annotation_encounter_warnings(&report),
                expected,
                "only fresh lookup hits warn"
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
            annotation_encounter_receipt(&registry, &report).await;
            assert_eq!(annotation_encounter_rows(&runtime).await, before);
        }
        annotation_encounter_fetches(&logs, prs, &[1, 2, 3]);
        registry.shutdown_audit_batch().await.unwrap();
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn existing_annotation_warning_reports_unavailable_graph_reads() {
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
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "ALTER TABLE graph_edges RENAME TO annotation_unavailable_edges",
            vec![],
        ))
        .await
        .unwrap();
    let outcome = registry
        .dispatch(
            "git.digest",
            annotation_encounter_params(&repo, project, false, 1),
        )
        .await;
    // Restore even if the real dispatch failed, before inspecting its result.
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement::new(
            "ALTER TABLE annotation_unavailable_edges RENAME TO graph_edges",
            vec![],
        ))
        .await
        .unwrap();
    let report = outcome.expect("annotation read failure remains an explicit warning");
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
        .any(|warning| warning
            .as_str()
            .unwrap()
            .starts_with("project annotation census unavailable: ")));
    assert!(report["missing_project_annotation_count"].is_null());
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
