//! Real continuation pages and deterministic SHA lookup work as their corpus grows.

use super::digest_scale::file_fixture;
use super::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;

const COMMIT_LOOKUP: &str = include_str!("../../sql/commits_by_sha_select.sql");

fn linear_history(repo: &Path, count: usize) -> Vec<String> {
    init_repo(repo);
    let mut stream = String::new();
    for index in 1..=count {
        let message = format!("Fixture commit {index}\n");
        let contents = format!("{index}\n");
        stream.push_str(&format!(
            "commit refs/heads/main\nmark :{index}\n\
             committer Fixture <fixture@example.com> {} +0000\ndata {}\n{message}",
            1_735_689_600 + index,
            message.len(),
        ));
        if index > 1 {
            stream.push_str(&format!("from :{}\n", index - 1));
        }
        stream.push_str(&format!(
            "M 100644 inline tracked.txt\ndata {}\n{contents}\n",
            contents.len(),
        ));
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real git fast-import");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git fast-import: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--reverse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let shas: Vec<_> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(shas.len(), count);
    shas
}

fn lookup_work(rt: &KhiveRuntime, sha: &str) -> (Vec<String>, u64) {
    let work = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&work);
    let writer = rt.backend().pool().try_writer().unwrap();
    let mut statement = writer.conn().prepare(COMMIT_LOOKUP).unwrap();
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
    let rows = statement
        .query_map(["local", sha], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>();
    writer
        .conn()
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    (rows.unwrap(), work.load(Ordering::Relaxed))
}

async fn stored_commits(rt: &KhiveRuntime) -> BTreeMap<String, String> {
    let rows = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT id,json_extract(properties,'$.sha') AS sha FROM notes \
                  WHERE kind='commit' AND namespace=?1 AND deleted_at IS NULL"
                .into(),
            params: vec![SqlValue::Text("local".into())],
            label: Some("test_commit_page_inventory".into()),
        })
        .await
        .unwrap();
    let commits: BTreeMap<_, _> = rows
        .iter()
        .map(|row| {
            let id = row.text("id").unwrap();
            Uuid::parse_str(id).expect("persisted commit ID");
            (row.text("sha").unwrap().to_owned(), id.to_owned())
        })
        .collect();
    assert_eq!(commits.len(), rows.len(), "duplicate persisted SHAs");
    commits
}

async fn assert_cursor(rt: &KhiveRuntime, project: Uuid, head: &str, completed: &str) {
    assert_eq!(
        read_git_cursor(rt, project, "commits").await.as_deref(),
        Some(completed)
    );
    let checkpoint: Value = serde_json::from_str(
        &read_git_cursor(rt, project, "commits_checkpoint")
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        checkpoint,
        json!({"version":1,"namespace":"local","base_cursor":null,
               "snapshot_head":head,"last_completed_sha":completed})
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_pages_preserve_progress_and_bound_sha_lookup_work() {
    let _guard = ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    // One Git process builds actual objects and changed paths for the production walk.
    let shas = linear_history(&repo, 256);
    let absent = "0".repeat(shas[0].len());
    assert!(!shas.contains(&absent));
    let db = dir.path().join("mirror.db");
    let (rt, token, registry) = file_fixture(&db).await;
    let project = create(&registry, json!({"kind":"project","name":"commit work"})).await;
    drop(registry);
    drop(token);
    drop(rt);

    let mut previous = 0;
    let mut samples = Vec::new();
    for total in [16, 64, 256, 256] {
        let (rt, token, registry) = file_fixture(&db).await;
        if previous > 0 {
            assert_cursor(&rt, project, &shas[255], &shas[previous - 1]).await;
        }
        let added = total - previous;
        let completion = added == 0;
        let report = run_ingest(
            &rt,
            &token,
            &registry,
            IngestOptions {
                repo: repo.clone(),
                expected_github_repo: None,
                project: project.to_string(),
                max_items: Some(added.max(1) as u64),
                include: IngestInclude {
                    commits: true,
                    issues: false,
                    pull_requests: false,
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(report.commits_ingested, added as u64, "{report:?}");
        assert_eq!(report.commits_skipped_existing, 0, "{report:?}");
        assert_eq!(report.commits_total_in_db, total as u64, "{report:?}");
        assert_eq!(
            report.parent_edges_created,
            (added - usize::from(previous == 0)) as u64
        );
        assert_eq!(report.done, completion, "{report:?}");
        assert_eq!(report.history_exhausted, total == shas.len(), "{report:?}");
        assert!(!report.cursor_stalled, "{report:?}");
        assert_eq!(report.writes_refused, 0, "{report:?}");
        assert!(report.warnings.is_empty(), "{report:?}");
        assert_eq!(report.gh_available, None);
        assert_eq!((report.issues_ingested, report.prs_ingested), (0, 0));
        assert_cursor(&rt, project, &shas[255], &shas[total - 1]).await;
        let stored = stored_commits(&rt).await;
        let mut expected = shas[..total].to_vec();
        expected.sort();
        assert_eq!(stored.keys().cloned().collect::<Vec<_>>(), expected);

        if !completion {
            let mut work = Vec::new();
            for sha in [&shas[0], &shas[total - 1], &absent] {
                // Probe the production SQL on the ingested corpus; this does not
                // instrument the async reader used inside run_ingest itself.
                let (ids, instructions) = lookup_work(&rt, sha);
                assert_eq!(
                    ids,
                    stored.get(sha).cloned().into_iter().collect::<Vec<_>>()
                );
                assert!(instructions > 0, "the production lookup must execute");
                work.push(instructions);
            }
            println!("COMMIT_PAGE_WORK total={total} early_late_miss={work:?}");
            samples.push(work);
        }
        previous = total;
        // Drop every handle before the next pass reopens the file-backed store.
    }
    assert_eq!(samples.len(), 3);
    for (probe, name) in ["early hit", "late hit", "miss"].into_iter().enumerate() {
        for sample in &samples[1..] {
            assert!(
                sample[probe] <= samples[0][probe] * 2,
                "SHA lookup work must stay bounded as ingested history grows: \
                 {name}, counts=[16,64,256], work={samples:?}"
            );
        }
    }
}
