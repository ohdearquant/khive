//! Issue #2520: remote fetch work follows the remaining record-visit budget.
use super::*;
use khive_pack_git::ingest::IngestSourceState;
use std::collections::BTreeSet;

const TIE: &str = "2026-01-01T00:00:01Z";
const LATER: &str = "2026-01-01T00:00:02Z";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_budgeted_gh(repo: &Path, bin: &Path, logs: &Path, prs: &[Value], issues: &[Value]) {
    write_fake_gh(repo, bin, logs, "[]", "[]");
    let mut arms = String::new();
    for (kind, records) in [("pr", prs), ("issue", issues)] {
        let mut rows = records.to_vec();
        // Preserve remote order inside a timestamp tie: local number sorting
        // must not turn that ordering into a numeric high-water predicate.
        rows.sort_by(|a, b| a["updatedAt"].as_str().cmp(&b["updatedAt"].as_str()));
        let file = logs.join(format!("{kind}.jsonl"));
        let body = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        std::fs::write(&file, body).unwrap();
        let undated = rows
            .iter()
            .take_while(|row| row["updatedAt"].is_null())
            .count();
        let floors: BTreeSet<_> = std::iter::once("")
            .chain(rows.iter().filter_map(|row| row["updatedAt"].as_str()))
            .collect();
        for floor in floors {
            let first = rows
                .iter()
                .position(|row| row["updatedAt"].as_str().is_some_and(|date| date >= floor))
                .unwrap_or(rows.len())
                + 1;
            let search = if floor.is_empty() {
                "sort:updated-asc".to_owned()
            } else {
                format!("sort:updated-asc updated:>={floor}")
            };
            arms.push_str(&format!(
                "  {}) file={}; first={first}; undated={undated} ;;\n",
                shell_quote(&format!("{kind}|{search}")),
                shell_quote(file.to_str().unwrap())
            ));
        }
    }
    let script = format!(
        r#"#!/bin/sh
set -eu
if [ "$1" = repo ]; then
  [ "$2" = view ] && [ "$3" = fixture/repository ] || exit 2
  printf '%s\n' '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
  exit 0
fi
kind="$1"
[ "$2" = list ] || exit 3
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
printf '%s %s %s\n' "$kind" "$limit" "$search" >> {log}
case "$kind|$search" in
{arms}  *) exit 6 ;;
esac
printf '['
if [ "$limit" -le "$undated" ]; then
  sed -n "1,${{limit}}p" "$file"
else
  if [ "$undated" -gt 0 ]; then
    sed -n "1,${{undated}}p" "$file"
  fi
  sed -n "${{first}},$((first + limit - undated - 1))p" "$file"
fi | paste -sd, -
printf ']\n'
"#,
        log = shell_quote(logs.join("fetches.log").to_str().unwrap())
    );
    std::fs::write(bin.join("gh"), script).unwrap();
}

fn remote_fixture(
    prs: &[Value],
    issues: &[Value],
) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, PathGuard) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let bin = dir.path().join("bin");
    let logs = dir.path().join("logs");
    for path in [&repo, &bin, &logs] {
        std::fs::create_dir(path).unwrap();
    }
    init_repo(&repo);
    write_budgeted_gh(&repo, &bin, &logs, prs, issues);
    let guard = PathGuard::install(&bin);
    (dir, repo, bin, logs, guard)
}

fn options(repo: &Path, project: Uuid, prs: bool, issues: bool, max: u64) -> IngestOptions {
    IngestOptions {
        repo: repo.into(),
        expected_github_repo: Some("fixture/repository".into()),
        project: project.to_string(),
        max_items: Some(max),
        include: IngestInclude {
            commits: false,
            issues,
            pull_requests: prs,
        },
    }
}

fn fetch_limits(logs: &Path) -> Vec<(String, usize)> {
    std::fs::read_to_string(logs.join("fetches.log"))
        .unwrap()
        .lines()
        .map(|line| {
            let mut fields = line.split_whitespace();
            (
                fields.next().unwrap().to_owned(),
                fields.next().unwrap().parse().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_fetch_budget_is_shared_between_remote_sources() {
    let _lock = ENV_MUTEX.lock().await;
    for max in [5, 20] {
        let (rt, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"shared fetch budget"}),
        )
        .await;
        let prs = vec![pr_fixture(1, "first", TIE), pr_fixture(2, "second", LATER)];
        let issues: Vec<_> = (1..=30).map(|n| issue_fixture(n, "issue", TIE)).collect();
        let (_dir, repo, _bin, logs, _path) = remote_fixture(&prs, &issues);
        let report = run_ingest(
            &rt,
            &token,
            &registry,
            options(&repo, project, true, true, max),
        )
        .await
        .unwrap();
        assert_eq!(report.prs_ingested, 2, "{report:?}");
        assert_eq!(report.issues_ingested, max - 2, "{report:?}");
        assert_eq!(
            fetch_limits(&logs),
            vec![
                ("pr".into(), max as usize),
                ("issue".into(), max as usize - 2)
            ]
        );
        assert!(matches!(
            report.sources.pull_requests,
            Some(IngestSourceState::Completed)
        ));
        assert!(matches!(
            report.sources.issues,
            Some(IngestSourceState::StoppedEarly(_))
        ));
        assert!(
            !report.done,
            "a full reduced issue page is incomplete: {report:?}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_fetch_budget_one_visit_resumes_undated_and_timestamp_ties() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"fetch boundary ties"}),
        )
        .await;
        let make = if prs { pr_fixture } else { issue_fixture };
        let mut undated = make(10, "undated", TIE);
        undated["updatedAt"] = Value::Null;
        let mut rows = vec![
            undated,
            make(30, "third", TIE),
            make(20, "second", TIE),
            make(40, "later", LATER),
        ];
        let (pr_rows, issue_rows) = if prs {
            (rows.clone(), vec![])
        } else {
            (vec![], rows.clone())
        };
        let (_dir, repo, bin, logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let opts = options(&repo, project, prs, !prs, 1);
        for pass in 0..5 {
            if pass == 3 {
                rows.push(make(5, "late lower-number tie", TIE));
                let (pr_rows, issue_rows) = if prs {
                    (rows.clone(), vec![])
                } else {
                    (vec![], rows.clone())
                };
                write_budgeted_gh(&repo, &bin, &logs, &pr_rows, &issue_rows);
            }
            let report = run_ingest(&rt, &token, &registry, opts.clone())
                .await
                .unwrap();
            assert_eq!(
                report.prs_ingested + report.issues_ingested,
                1,
                "pass={pass}: {report:?}"
            );
            assert_eq!(
                report.prs_skipped_existing + report.issues_skipped_existing,
                0,
                "{report:?}"
            );
            assert!(
                !report.done,
                "an exact-budget pass is incomplete: {report:?}"
            );
        }
        let final_report = run_ingest(&rt, &token, &registry, opts).await.unwrap();
        assert!(
            final_report.done,
            "a short replay proves completion: {final_report:?}"
        );
        assert_eq!(
            final_report.prs_ingested + final_report.issues_ingested,
            0,
            "{final_report:?}"
        );
        let operation = if prs { "pr" } else { "issue" };
        assert_eq!(
            fetch_limits(&logs),
            [1, 2, 3, 4, 5, 3].map(|limit| (operation.to_owned(), limit))
        );
        let kind = if prs { "pull_request" } else { "issue" };
        let listed = registry
            .dispatch("list", json!({"kind":kind,"limit":10}))
            .await
            .unwrap();
        let numbers: BTreeSet<_> = list_items(&listed)
            .iter()
            .map(|row| row["properties"]["number"].as_u64().unwrap())
            .collect();
        assert_eq!(numbers, BTreeSet::from([5, 10, 20, 30, 40]));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_fetch_budget_preserves_a_failed_cursor_before_later_existing_rows() {
    let _lock = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind":"project","name":"fetch frozen cursor"}),
    )
    .await;
    create(&registry, json!({"kind":"issue","name":"existing","content":"existing",
        "properties":{"number":30,"project_id":project.to_string()},"annotates":[project.to_string()]})).await;
    let newest = "2026-01-01T00:00:03Z";
    let mut rejected = issue_fixture(20, "rejected", LATER);
    rejected["stateReason"] = json!("unknown");
    let mut rows = vec![
        issue_fixture(10, "good", TIE),
        rejected,
        issue_fixture(30, "existing", newest),
    ];
    let (_dir, repo, bin, logs, _path) = remote_fixture(&[], &rows);
    let opts = options(&repo, project, false, true, 2);
    let first = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .unwrap();
    assert_eq!(first.issues_ingested, 1, "{first:?}");
    assert!(first.cursor_stalled && !first.done, "{first:?}");
    let second = run_ingest(&rt, &token, &registry, opts).await.unwrap();
    assert_eq!(second.issues_skipped_existing, 1, "{second:?}");
    assert!(second.cursor_stalled && !second.done, "{second:?}");
    assert_eq!(
        read_git_cursor(&rt, project, "issues").await.as_deref(),
        Some(TIE)
    );
    rows[1]["stateReason"] = Value::Null;
    write_budgeted_gh(&repo, &bin, &logs, &[], &rows);
    let repaired = run_ingest(
        &rt,
        &token,
        &registry,
        options(&repo, project, false, true, 1),
    )
    .await
    .unwrap();
    assert_eq!(repaired.issues_ingested, 1, "{repaired:?}");
    assert!(!repaired.cursor_stalled, "{repaired:?}");
    assert_eq!(
        read_git_cursor(&rt, project, "issues").await.as_deref(),
        Some(LATER)
    );
    let finished = run_ingest(
        &rt,
        &token,
        &registry,
        options(&repo, project, false, true, 2),
    )
    .await
    .unwrap();
    assert!(finished.done && !finished.cursor_stalled, "{finished:?}");
    assert_eq!(finished.issues_skipped_existing, 1, "{finished:?}");
    assert_eq!(
        fetch_limits(&logs),
        vec![
            ("issue".into(), 2),
            ("issue".into(), 3),
            ("issue".into(), 2),
            ("issue".into(), 3)
        ]
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_fetch_budget_reduces_the_second_page_and_resumes_existing_history() {
    let _lock = ENV_MUTEX.lock().await;
    for prs in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind":"project","name":"second fetch page"}),
        )
        .await;
        let kind = if prs { "pull_request" } else { "issue" };
        let make = if prs { pr_fixture } else { issue_fixture };
        let rows: Vec<_> = (1..=1003)
            .map(|n| {
                let timestamp = chrono::DateTime::from_timestamp(1_767_225_600 + n as i64, 0)
                    .unwrap()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                make(n, "existing", &timestamp)
            })
            .collect();
        {
            let writer = rt.backend().pool().try_writer().unwrap();
            writer.conn().execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1003) INSERT INTO notes(id,namespace,kind,name,content,properties,created_at,updated_at) SELECT printf('25200000-0000-4000-8000-%012x',i),'local',?1,'existing','existing',json_object('number',i,'project_id',?2),i,i FROM n", (kind,project.to_string())).unwrap();
        }
        let (pr_rows, issue_rows) = if prs { (rows, vec![]) } else { (vec![], rows) };
        let (_dir, repo, _bin, logs, _path) = remote_fixture(&pr_rows, &issue_rows);
        let first = run_ingest(
            &rt,
            &token,
            &registry,
            options(&repo, project, prs, !prs, 1002),
        )
        .await
        .unwrap();
        assert_eq!(
            first.prs_skipped_existing + first.issues_skipped_existing,
            1002,
            "{first:?}"
        );
        assert!(!first.done, "{first:?}");
        let second = run_ingest(
            &rt,
            &token,
            &registry,
            options(&repo, project, prs, !prs, 5),
        )
        .await
        .unwrap();
        assert_eq!(
            second.prs_skipped_existing + second.issues_skipped_existing,
            1,
            "{second:?}"
        );
        assert!(second.done, "{second:?}");
        let operation = if prs { "pr" } else { "issue" };
        assert_eq!(
            fetch_limits(&logs),
            [1000, 3, 6].map(|limit| (operation.to_owned(), limit))
        );
    }
}
