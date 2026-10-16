/// A tombstoned annotation is a curation choice. Replaying a shared commit
/// must skip it, create other missing annotations, and advance the cursor.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn shared_commit_replay_skips_tombstone_and_creates_missing_module_link() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_a = create(&registry, json!({"kind": "project", "name": "link-a"})).await;
    let project_b = create(&registry, json!({"kind": "project", "name": "link-b"})).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "src/lib.rs", "pub fn shared() {}\n");
    commit(repo, &["src/lib.rs"], "Shared commit");
    let sha = head_sha(repo);
    let options = |project: Uuid| IngestOptions::unbounded(repo.to_path_buf(), project.to_string());
    run_ingest(&rt, &token, &registry, options(project_a))
        .await
        .expect("first project ingest");
    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let commit_id = list_items(&commits)[0]["id"].as_str().expect("commit id");

    let module_b = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "link_b_module",
            "properties": {
                "source_project": "link-b",
                "source_path": "src/lib.rs",
                "source_revision": sha.clone(),
            }
        }),
    )
    .await;

    let edge = registry
        .dispatch(
            "link",
            json!({
                "source_id": commit_id,
                "target_id": project_b.to_string(),
                "relation": "annotates",
            }),
        )
        .await
        .expect("seed second project edge");
    let edge_id = Uuid::parse_str(edge["id"].as_str().expect("edge id")).expect("edge UUID");
    assert!(rt
        .delete_edge(&token, edge_id, false)
        .await
        .expect("soft delete edge"));

    let replay = run_ingest(&rt, &token, &registry, options(project_b))
        .await
        .expect("tombstone is skipped during replay");
    assert!(!replay.cursor_stalled, "{replay:?}");
    assert_eq!(replay.commits_skipped_existing, 1, "{replay:?}");
    assert_eq!(read_git_cursor(&rt, project_b, "commits").await, Some(sha));
    assert!(incoming_annotating_ids(&registry, project_b)
        .await
        .is_empty());
    assert_eq!(
        incoming_annotating_ids(&registry, module_b).await,
        std::collections::BTreeSet::from([commit_id.to_string()]),
    );
    let tombstone = rt
        .get_edge_including_deleted(&token, edge_id)
        .await
        .expect("read tombstone")
        .expect("tombstone remains");
    assert!(tombstone.deleted_at.is_some());
}

/// A shared revision can occur in two forks. Revision-plus-path narrows the
/// repository snapshot, but it must not become an arbitrary tie-breaker when
/// two live module rows still claim that identity.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_skips_ambiguous_snapshot_path_bindings() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "ambiguous-path-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "src/lib.rs", "pub fn shared() {}\n");
    commit(repo, &["src/lib.rs"], "Shared snapshot");
    let revision = head_sha(repo);

    // An ambiguous key (two live rows, one `source_path`) NO ingested
    // commit touches must not inflate the skip count: only a commit path
    // that actually hits the ambiguous key counts.
    for name in [
        "untouched_ambiguous_module_a",
        "untouched_ambiguous_module_b",
    ] {
        create(
            &registry,
            json!({
                "kind": "concept",
                "entity_type": "module",
                "name": name,
                "properties": {
                    "source_project": "untouched-project",
                    "source_path": "src/untouched.rs",
                    "source_revision": revision.clone()
                }
            }),
        )
        .await;
    }

    let first = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "first_shared_module",
            "properties": {
                "source_project": "first-project",
                "source_path": "src/lib.rs",
                "source_revision": revision.clone()
            }
        }),
    )
    .await;
    let second = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "second_shared_module",
            "properties": {
                "source_project": "second-project",
                "source_path": "src/lib.rs",
                "source_revision": revision
            }
        }),
    )
    .await;

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1, "{report:?}");
    assert_eq!(
        report.code_module_ambiguous_path_skips, 1,
        "only the commit-touched ambiguous key counts; the untouched one does not: {report:?}"
    );
    let skip_warnings: Vec<&str> = report
        .warnings
        .iter()
        .map(String::as_str)
        .filter(|w| w.contains("skipped code-module annotation"))
        .collect();
    assert_eq!(
        skip_warnings.len(),
        1,
        "one bounded skip warning per run: {:?}",
        report.warnings
    );
    assert!(
        skip_warnings[0].contains("src/lib.rs"),
        "the bounded warning names the first skipped path so the count is \
         actionable: {}",
        skip_warnings[0]
    );
    assert!(incoming_annotating_ids(&registry, first).await.is_empty());
    assert!(incoming_annotating_ids(&registry, second).await.is_empty());
}
