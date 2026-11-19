/// Full end-to-end: a fixture repo with three commits (two touching a
/// tracked ADR path, one unrelated), a pre-ingested PR that a squash-merge
/// commit references by `(#NNN)` suffix — asserts the provenance query
/// genre: incoming `annotates` from the document yields exactly the
/// touching commits, and the squash-merge commit's PR edge resolves.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_links_commits_to_document_and_pr_by_provenance_query() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "acceptance-repo"}),
    )
    .await;
    let doc_id = create(
        &registry,
        json!({
            "kind": "document",
            "name": "ADR-045-test.md",
            "properties": {"source_uri": "docs/adr/ADR-045-test.md"},
        }),
    )
    .await;
    let pr_id = create(
        &registry,
        json!({
            "kind": "pull_request",
            "content": "",
            "properties": {"number": 42, "title": "Add ADR-045", "project_id": project_id.to_string()},
            "annotates": [project_id.to_string()],
        }),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    write(
        repo,
        "docs/adr/ADR-045-test.md",
        "# ADR-045\n\nInitial draft.\n",
    );
    commit(repo, &["docs/adr/ADR-045-test.md"], "Add ADR-045 (#42)");
    let commit1 = head_sha(repo);

    write(repo, "src/lib.rs", "// unrelated change\n");
    commit(repo, &["src/lib.rs"], "Unrelated source change");

    write(repo, "docs/adr/ADR-045-test.md", "# ADR-045\n\nRevised.\n");
    commit(repo, &["docs/adr/ADR-045-test.md"], "Revise ADR-045");
    let commit3 = head_sha(repo);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 3,
        "all three commits ingest: {report:?}"
    );
    assert_eq!(
        report.commits_total_in_db, 3,
        "commits_total_in_db is a DB-derived row count, independent of the \
         in-memory ingested delta: {report:?}"
    );

    // Provenance query genre: incoming `annotates` from the document.
    let doc_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": doc_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = doc_neighbors.as_array().expect("array");
    assert_eq!(
        hits.len(),
        2,
        "exactly the two touching commits annotate the document: {hits:?}"
    );
    for h in hits {
        assert_eq!(h["kind"], "commit");
    }
    // `neighbors` returns the note's own UUID, not its `properties.sha` — resolve
    // each hit through `get` to compare against the real commit shas.
    let mut hit_shas: Vec<String> = Vec::new();
    for h in hits {
        let id = h["id"].as_str().expect("neighbor id");
        let got = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("get ok");
        hit_shas.push(
            got["properties"]["sha"]
                .as_str()
                .expect("commit note has properties.sha")
                .to_string(),
        );
    }
    assert!(
        hit_shas.contains(&commit1),
        "commit1 {commit1} must be among document neighbors: {hit_shas:?}"
    );
    assert!(
        hit_shas.contains(&commit3),
        "commit3 {commit3} must be among document neighbors: {hit_shas:?}"
    );

    // The squash-merge commit's PR edge resolves: pr_id has exactly one
    // incoming `annotates` from a commit.
    let pr_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let pr_hits = pr_neighbors.as_array().expect("array");
    assert_eq!(
        pr_hits.len(),
        1,
        "exactly one commit annotates the PR: {pr_hits:?}"
    );
    assert_eq!(pr_hits[0]["kind"], "commit");

    // The project entity is annotated by all three commits and the PR.
    let project_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": project_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        project_neighbors.as_array().expect("array").len(),
        4,
        "3 commits + 1 pull_request annotate the project: {project_neighbors:?}"
    );
}

/// Issue #1604: changed-path properties and exact ADR-085 `source_path`
/// annotations make module churn and repeated cross-project co-change
/// computable from graph reads without reopening git history.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_records_changed_paths_and_links_code_modules() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "path-provenance-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    write(repo, "crates/crate-a/src/lib.rs", "pub fn a() {}\n");
    write(repo, "crates/crate-b/src/lib.rs", "pub fn b() {}\n");
    commit(
        repo,
        &["crates/crate-a/src/lib.rs", "crates/crate-b/src/lib.rs"],
        "Add both crates",
    );
    let both_1 = head_sha(repo);

    write(
        repo,
        "crates/crate-a/src/lib.rs",
        "pub fn a() {}\npub fn a2() {}\n",
    );
    commit(repo, &["crates/crate-a/src/lib.rs"], "Change crate A");
    let only_a = head_sha(repo);

    write(
        repo,
        "crates/crate-a/src/lib.rs",
        "pub fn a() {}\npub fn a2() {}\npub fn a3() {}\n",
    );
    write(
        repo,
        "crates/crate-b/src/lib.rs",
        "pub fn b() {}\npub fn b2() {}\n",
    );
    commit(
        repo,
        &["crates/crate-a/src/lib.rs", "crates/crate-b/src/lib.rs"],
        "Change both crates again",
    );
    let both_2 = head_sha(repo);

    // ADR-085 scopes physical paths to a repository snapshot. A second map
    // can legitimately carry the same relative path, so the revision must
    // participate in the binding or digesting this repository would
    // fabricate an annotation to the other repository's module.
    let other_dir = tempfile::tempdir().expect("other tempdir");
    let other_repo = other_dir.path();
    init_repo(other_repo);
    write(
        other_repo,
        "crates/crate-a/src/lib.rs",
        "pub fn from_other_repo() {}\n",
    );
    commit(
        other_repo,
        &["crates/crate-a/src/lib.rs"],
        "Other repository",
    );
    let other_revision = head_sha(other_repo);
    assert_ne!(both_2, other_revision);

    let module_a = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "crate_a",
            "properties": {
                "source_project": "crate-a",
                "source_path": "crates/crate-a/src/lib.rs",
                "source_revision": both_2.clone()
            }
        }),
    )
    .await;
    let module_b = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "crate_b",
            "properties": {
                "source_project": "crate-b",
                "source_path": "crates/crate-b/src/lib.rs",
                "source_revision": both_2.clone()
            }
        }),
    )
    .await;
    let other_repo_module = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "other_crate_a",
            "properties": {
                "source_project": "other-crate-a",
                "source_path": "crates/crate-a/src/lib.rs",
                "source_revision": other_revision
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
    assert_eq!(report.commits_ingested, 3, "{report:?}");

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let paths_by_sha: std::collections::HashMap<String, Vec<String>> = list_items(&commits)
        .iter()
        .map(|note| {
            let sha = note["properties"]["sha"].as_str().expect("sha").to_string();
            let paths = note["properties"]["changed_paths"]
                .as_array()
                .expect("changed_paths array")
                .iter()
                .map(|path| path.as_str().expect("path string").to_string())
                .collect();
            (sha, paths)
        })
        .collect();
    let both_paths = vec![
        "crates/crate-a/src/lib.rs".to_string(),
        "crates/crate-b/src/lib.rs".to_string(),
    ];
    assert_eq!(paths_by_sha[&both_1], both_paths);
    assert_eq!(
        paths_by_sha[&only_a],
        vec!["crates/crate-a/src/lib.rs".to_string()]
    );
    assert_eq!(paths_by_sha[&both_2], both_paths);

    let module_a_commits = incoming_annotating_ids(&registry, module_a).await;
    let module_b_commits = incoming_annotating_ids(&registry, module_b).await;
    assert_eq!(module_a_commits.len(), 3, "crate-a churn");
    assert_eq!(module_b_commits.len(), 2, "crate-b churn");
    assert!(
        incoming_annotating_ids(&registry, other_repo_module)
            .await
            .is_empty(),
        "same relative path from another repository must not cross-annotate"
    );
    assert_eq!(
        module_a_commits.intersection(&module_b_commits).count(),
        2,
        "the cross-crate pair co-changed in two commits"
    );
}
