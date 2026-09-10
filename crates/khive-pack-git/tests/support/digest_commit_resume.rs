//! Public commits-only continuation across a DAG and file-backed pool reopening.
use super::digest_scale::file_fixture;
use super::*;
use std::collections::BTreeSet;

fn diamond(repo: &Path) -> Vec<String> {
    init_repo(repo);
    write(repo, "root.txt", "root\n");
    commit(repo, &["root.txt"], "R");
    let root = head_sha(repo);
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, "side.txt", "side\n");
    commit(repo, &["side.txt"], "B");
    let side = head_sha(repo);
    git(repo, &["checkout", "-q", "main"]);
    write(repo, "main.txt", "main\n");
    commit(repo, &["main.txt"], "A");
    let main = head_sha(repo);
    git(repo, &["merge", "-q", "--no-ff", "side", "-m", "M"]);
    vec![root, main, side, head_sha(repo)]
}

async fn digest(registry: &VerbRegistry, repo: &Path, project: Uuid, max: u64) -> Value {
    registry
        .dispatch(
            "git.digest",
            json!({
                "source":repo.to_str().unwrap(), "project":project.to_string(),
                "include":["commits"], "max_items":max,
            }),
        )
        .await
        .expect("public commits-only digest")
}

async fn checkpoint(rt: &KhiveRuntime, project: Uuid) -> Value {
    serde_json::from_str(
        &read_git_cursor(rt, project, "commits_checkpoint")
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_diamond_max_one_reopens_database_and_finishes() {
    let _guard = ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let shas = diamond(&repo);
    let db = dir.path().join("mirror.db");
    let (rt, token, registry) = file_fixture(&db).await;
    let project = create(&registry, json!({"kind":"project","name":"diamond"})).await;
    drop(registry);
    drop(token);
    drop(rt);
    let mut positions = BTreeSet::new();
    for pass in 0..5 {
        let (rt, _token, registry) = file_fixture(&db).await;
        let report = digest(&registry, &repo, project, 1).await;
        let visits = report["commits_ingested"].as_u64().unwrap()
            + report["commits_skipped_existing"].as_u64().unwrap();
        assert!(visits <= 1, "fresh visits are bounded: {report}");
        assert_eq!(
            report["commits_skipped_existing"], 0,
            "acknowledged prefix is free: {report}"
        );
        assert_eq!(report["commits_ingested"], u64::from(pass < 4), "{report}");
        assert_eq!(
            report["done"],
            pass == 4,
            "four landings then empty completion: {report}"
        );
        let progress = checkpoint(&rt, project).await;
        assert_eq!(progress["snapshot_head"], shas[3]);
        assert_eq!(progress["base_cursor"], Value::Null);
        let cursor = read_git_cursor(&rt, project, "commits").await.unwrap();
        assert_eq!(progress["last_completed_sha"], cursor);
        if pass < 4 {
            assert!(positions.insert(cursor), "no cursor cycle");
        }
        let notes = registry
            .dispatch("list", json!({"kind":"commit","limit":10}))
            .await
            .unwrap();
        assert_eq!(list_items(&notes).len(), (pass + 1).min(4));
        if pass == 4 {
            let stored: BTreeSet<_> = list_items(&notes)
                .iter()
                .map(|n| n["properties"]["sha"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(stored, shas.iter().cloned().collect());
            assert_eq!(report["history_exhausted"], true);
            let merge = list_items(&notes)
                .iter()
                .find(|n| n["properties"]["sha"] == shas[3])
                .unwrap();
            assert_eq!(merge["properties"]["changed_paths"], json!(["side.txt"]));
        }
        // All runtime handles go out of scope before the next pass opens the DB.
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_frozen_diamond_requests_new_head_even_with_spare_budget() {
    let _guard = ENV_MUTEX.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let shas = diamond(&repo);
    let db = dir.path().join("mirror.db");
    let (rt, token, registry) = file_fixture(&db).await;
    let project = create(
        &registry,
        json!({"kind":"project","name":"advancing diamond"}),
    )
    .await;
    assert_eq!(
        digest(&registry, &repo, project, 1).await["commits_ingested"],
        1
    );
    drop(registry);
    drop(token);
    drop(rt);
    write(&repo, "later.txt", "later\n");
    commit(&repo, &["later.txt"], "after frozen tip");
    let new_head = head_sha(&repo);
    let (rt, token, registry) = file_fixture(&db).await;
    let resumed = digest(&registry, &repo, project, 20).await;
    assert_eq!(
        resumed["commits_ingested"], 3,
        "only the frozen remainder: {resumed}"
    );
    assert_eq!(resumed["commits_skipped_existing"], 0);
    assert_eq!(resumed["done"], false);
    assert_eq!(resumed["history_exhausted"], false);
    assert!(resumed["sources"]["commits"]["reason"]
        .as_str()
        .unwrap()
        .contains("HEAD changed"));
    assert_eq!(checkpoint(&rt, project).await["snapshot_head"], shas[3]);
    drop(registry);
    drop(token);
    drop(rt);
    let (rt, _token, registry) = file_fixture(&db).await;
    let next = digest(&registry, &repo, project, 20).await;
    assert_eq!(
        next["commits_ingested"], 1,
        "new tip still needs its own visit: {next}"
    );
    assert_eq!(next["commits_skipped_existing"], 0);
    assert_eq!(next["done"], true);
    assert_eq!(checkpoint(&rt, project).await["snapshot_head"], new_head);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn digest_resume_invalid_commit_continuation_fails_without_advancing() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, _token, registry) = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let shas = diamond(dir.path());
    let project = create(
        &registry,
        json!({"kind":"project","name":"invalid continuation"}),
    )
    .await;
    digest(&registry, dir.path(), project, 1).await;
    let good = checkpoint(&rt, project).await;
    let mut invalid = vec!["{".to_string(), " ".repeat(8193)];
    for (key, value) in [
        ("version", json!(2)),
        ("namespace", json!("other")),
        ("snapshot_head", json!("--all")),
        ("snapshot_head", json!("0".repeat(40))),
        ("last_completed_sha", json!(shas[1])),
    ] {
        let mut bad = good.clone();
        bad[key] = value;
        invalid.push(bad.to_string());
    }
    // Syntactically valid, matching main SHA but absent from base..tip.
    let mut missing = good.clone();
    missing["base_cursor"] = json!(shas[0]);
    invalid.push(missing.to_string());
    for raw in invalid {
        rt.sql().writer().await.unwrap().execute(SqlStatement {
            sql:"UPDATE git_mirror_cursor SET cursor_value=?1 WHERE project_id=?2 AND kind='commits_checkpoint'".into(),
            params:vec![SqlValue::Text(raw.clone()),SqlValue::Text(project.to_string())], label:None,
        }).await.unwrap();
        let failed = registry
            .dispatch(
                "git.digest",
                json!({"source":dir.path().to_str().unwrap(),
            "project":project.to_string(),"include":["commits"],"max_items":1}),
            )
            .await;
        assert!(
            failed.is_err(),
            "invalid continuation must fail before record visits: {failed:?}"
        );
        assert_eq!(
            read_git_cursor(&rt, project, "commits").await.as_deref(),
            Some(shas[0].as_str())
        );
        assert_eq!(
            read_git_cursor(&rt, project, "commits_checkpoint")
                .await
                .as_deref(),
            Some(raw.as_str())
        );
        let notes = registry
            .dispatch("list", json!({"kind":"commit","limit":10}))
            .await
            .unwrap();
        assert_eq!(list_items(&notes).len(), 1);
    }
}
