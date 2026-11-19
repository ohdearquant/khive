/// A commit note is shared by SHA, while commit cursors belong to each
/// project. Reusing the note must still link the second project and the
/// modules resolved from its snapshot before acknowledging its cursor.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn shared_commit_note_links_second_project_and_new_snapshot_module_once() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project_a = create(&registry, json!({"kind": "project", "name": "shared-a"})).await;
    let project_b = create(&registry, json!({"kind": "project", "name": "shared-b"})).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "src/lib.rs", "pub fn shared() {}\n");
    commit(repo, &["src/lib.rs"], "Shared commit");
    let sha = head_sha(repo);

    let options = |project: Uuid| IngestOptions::unbounded(repo.to_path_buf(), project.to_string());
    let first = run_ingest(&rt, &token, &registry, options(project_a))
        .await
        .expect("first project ingest");
    assert_eq!(first.commits_ingested, 1, "{first:?}");
    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let commit_id = Uuid::parse_str(list_items(&commits)[0]["id"].as_str().expect("commit id"))
        .expect("commit UUID");

    // The second project's module arrives only after the first ingest. The
    // shared note cannot have acquired this annotation on its create path.
    let module_b = create(
        &registry,
        json!({
            "kind": "concept",
            "entity_type": "module",
            "name": "shared_b_module",
            "properties": {
                "source_project": "shared-b",
                "source_path": "src/lib.rs",
                "source_revision": sha.clone(),
            }
        }),
    )
    .await;
    assert!(incoming_annotating_ids(&registry, module_b)
        .await
        .is_empty());

    let second = run_ingest(&rt, &token, &registry, options(project_b))
        .await
        .expect("second project ingest");
    assert_eq!(second.commits_skipped_existing, 1, "{second:?}");
    assert_eq!(second.commits_ingested, 0, "{second:?}");
    assert_eq!(
        read_git_cursor(&rt, project_b, "commits").await,
        Some(sha.clone())
    );
    for target in [project_a, project_b, module_b] {
        let neighbors = registry
            .dispatch(
                "neighbors",
                json!({"id": target.to_string(), "direction": "incoming", "relations": ["annotates"]}),
            )
            .await
            .expect("annotation neighbors");
        let rows = neighbors.as_array().expect("neighbor array");
        assert_eq!(rows.len(), 1, "one edge to {target}: {rows:?}");
        assert_eq!(rows[0]["id"], commit_id.to_string());
    }

    let curated = registry
        .dispatch(
            "link",
            json!({
                "source_id": commit_id.to_string(),
                "target_id": project_b.to_string(),
                "relation": "annotates",
                "weight": 0.35,
                "metadata": {"curator": "retained on replay"},
            }),
        )
        .await
        .expect("curate second project annotation");
    let curated_id = Uuid::parse_str(curated["id"].as_str().expect("curated edge id"))
        .expect("curated edge UUID");
    let before_replay = rt
        .get_edge_including_deleted(&token, curated_id)
        .await
        .expect("read curated edge")
        .expect("curated edge exists");
    assert_eq!(before_replay.weight, 0.35);
    assert_eq!(
        before_replay.metadata,
        Some(json!({"curator": "retained on replay"}))
    );

    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("cursor writer");
    writer
        .execute(SqlStatement {
            sql: "DELETE FROM git_mirror_cursor WHERE project_id=?1 AND kind IN ('commits','commits_checkpoint')".into(),
            params: vec![SqlValue::Text(project_b.to_string())],
            label: Some("test_rewalk_shared_commit".into()),
        })
        .await
        .expect("reset project B cursor");
    drop(writer);
    let replay = run_ingest(&rt, &token, &registry, options(project_b))
        .await
        .expect("replay second project");
    assert_eq!(replay.commits_skipped_existing, 1, "{replay:?}");
    let after_replay = rt
        .get_edge_including_deleted(&token, curated_id)
        .await
        .expect("read edge after replay")
        .expect("curated edge remains");
    assert_eq!(after_replay.id, before_replay.id);
    assert_eq!(after_replay.created_at, before_replay.created_at);
    assert_eq!(after_replay.updated_at, before_replay.updated_at);
    assert_eq!(after_replay.weight, before_replay.weight);
    assert_eq!(after_replay.metadata, before_replay.metadata);
    assert_eq!(
        incoming_annotating_ids(&registry, project_b).await,
        std::collections::BTreeSet::from([commit_id.to_string()]),
    );
    let rows = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits after replay");
    assert_eq!(list_items(&rows).len(), 1, "the shared note stays singular");
}

/// Both projects complete their SHA lookup before either keyed create starts.
/// The key arbitrates the concurrent creates, and the losing pass must still
/// attach its project before it advances its own checkpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(config_ledger)]
async fn overlapping_commit_digests_publish_one_note_and_both_project_checkpoints() {
    let _guard = ENV_MUTEX.lock().await;
    let (runtime, token, registry) = fixture().await;
    let project_a = create(&registry, json!({"kind": "project", "name": "overlap-a"})).await;
    let project_b = create(&registry, json!({"kind": "project", "name": "overlap-b"})).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "one commit\n");
    commit(repo, &["README.md"], "Shared overlapping commit");
    let sha = head_sha(repo);

    let embeds = Arc::new(BothLookupsCommitEmbedService {
        before_create: tokio::sync::Barrier::new(2),
        arrivals: AtomicUsize::new(0),
    });
    runtime.register_embedder(BothLookupsCommitEmbedderProvider(Arc::clone(&embeds)));

    let runtime = Arc::new(runtime);
    let registry = Arc::new(registry);
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let spawn_digest = |project: Uuid| {
        let runtime = Arc::clone(&runtime);
        let registry = Arc::clone(&registry);
        let token = token.clone();
        let start = Arc::clone(&start);
        let repo = repo.to_path_buf();
        tokio::spawn(async move {
            start.wait().await;
            run_ingest(
                &runtime,
                &token,
                &registry,
                IngestOptions::unbounded(repo, project.to_string()),
            )
            .await
        })
    };
    let a = spawn_digest(project_a);
    let b = spawn_digest(project_b);
    start.wait().await;
    let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(a, b)
    })
    .await
    .expect("both digests must finish after the post-lookup barrier");
    assert_eq!(
        embeds.arrivals.load(Ordering::SeqCst),
        2,
        "both digests must reach embedding after their SHA lookup"
    );
    let a = a.expect("first ingest task").expect("first digest");
    let b = b.expect("second ingest task").expect("second digest");
    assert!(!a.cursor_stalled && !b.cursor_stalled, "{a:?}; {b:?}");
    assert_eq!(a.commits_ingested + b.commits_ingested, 1, "{a:?}; {b:?}");
    assert_eq!(a.commits_skipped_existing + b.commits_skipped_existing, 1);
    assert_eq!(
        read_git_cursor(&runtime, project_a, "commits").await,
        Some(sha.clone())
    );
    assert_eq!(
        read_git_cursor(&runtime, project_b, "commits").await,
        Some(sha)
    );

    let notes = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commit notes");
    let notes = list_items(&notes);
    assert_eq!(notes.len(), 1, "one live SHA holder: {notes:?}");
    let id = notes[0]["id"].as_str().expect("commit ID");
    for project in [project_a, project_b] {
        assert_eq!(
            incoming_annotating_ids(&registry, project).await,
            std::collections::BTreeSet::from([id.to_string()]),
            "project {project} must link the shared commit"
        );
    }
}

/// An embedding failure may pause for an arbitrarily long time after a
/// commit's SHA lookup. Another digest must not see its note or checkpoint it
/// until the create has published all dependent work successfully.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(config_ledger)]
async fn failing_commit_embedding_never_exposes_a_provisional_sha_holder() {
    let _guard = ENV_MUTEX.lock().await;
    let (runtime, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind": "project", "name": "provisional-commit"}),
    )
    .await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    runtime.register_embedder(HeldFailCommitEmbedderProvider(Arc::new(
        HeldFailCommitEmbedService {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
        },
    )));

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "pending\n");
    commit(repo, &["README.md"], "held-failing-commit");

    let runtime = Arc::new(runtime);
    let registry = Arc::new(registry);
    let ingest_runtime = Arc::clone(&runtime);
    let ingest_registry = Arc::clone(&registry);
    let ingest_token = token.clone();
    let repo_path = repo.to_path_buf();
    let task = tokio::spawn(async move {
        run_ingest(
            &ingest_runtime,
            &ingest_token,
            &ingest_registry,
            IngestOptions::unbounded(repo_path, project.to_string()),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("commit embedding did not start within five seconds")
        .expect("commit reached embedding preparation");
    let notes = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list while embedding is held");
    assert!(
        list_items(&notes).is_empty(),
        "a provisional SHA must not be visible: {notes}"
    );
    assert_eq!(read_git_cursor(&runtime, project, "commits").await, None);

    release_tx.send(()).expect("release held embedding");
    let report = task.await.expect("ingest task").expect("ingest report");
    assert!(report.cursor_stalled, "{report:?}");
    assert_eq!(read_git_cursor(&runtime, project, "commits").await, None);
    let notes = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list after failed embedding");
    assert!(list_items(&notes).is_empty(), "{notes}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn legacy_duplicate_sha_is_detected_before_checkpoint_advances() {
    let _guard = ENV_MUTEX.lock().await;
    let (runtime, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind": "project", "name": "duplicate-legacy"}),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "duplicate\n");
    commit(repo, &["README.md"], "Duplicate legacy SHA");
    let sha = head_sha(repo);

    for _ in 0..2 {
        registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "content": "legacy commit note",
                    "properties": {"sha": sha, "parents": []},
                }),
            )
            .await
            .expect("historical unkeyed commit create");
    }
    let report = run_ingest(
        &runtime,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project.to_string()),
    )
    .await
    .expect("a post-walk failure is reported as stopped early");
    let Some(khive_pack_git::ingest::IngestSourceState::StoppedEarly(reason)) =
        report.sources.commits.as_ref()
    else {
        panic!("duplicate SHA must stop the commit source: {report:?}");
    };
    assert!(
        reason.contains(&sha) && reason.contains("reconcile legacy duplicates"),
        "{report:?}"
    );
    assert!(
        report.warnings.iter().any(
            |warning| warning.contains(&sha) && warning.contains("reconcile legacy duplicates")
        ),
        "{report:?}"
    );
    assert_eq!(report.commits_ingested, 0, "{report:?}");
    assert!(
        !report.done,
        "a stopped source cannot report completion: {report:?}"
    );
    assert_eq!(read_git_cursor(&runtime, project, "commits").await, None);
    let notes = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commit notes after the refused pass");
    assert_eq!(
        list_items(&notes).len(),
        2,
        "no third note may be created: {notes}"
    );
}

/// A pre-existing malformed holder can survive from an older write path or
/// direct database repair. The ingest cannot advance past it, and its warning
/// must identify the row that needs reconciliation.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn conflicting_commit_key_names_the_holder_and_stalls_cursor() {
    let _guard = ENV_MUTEX.lock().await;
    let (runtime, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind": "project", "name": "key-holder-repo"}),
    )
    .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "one commit\n");
    commit(repo, &["README.md"], "Key holder mismatch");
    let sha = head_sha(repo);
    let other_sha = if sha == "b".repeat(40) {
        "a".repeat(40)
    } else {
        "b".repeat(40)
    };
    let holder_id = Uuid::new_v4();
    let key = format!("git.commit:{sha}");
    let mut writer = runtime.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO notes(id, namespace, kind, key, content, properties, created_at, updated_at) \
                  VALUES(?1, 'local', 'commit', ?2, '', ?3, 0, 0)"
                .into(),
            params: vec![
                SqlValue::Text(holder_id.to_string()),
                SqlValue::Text(key.clone()),
                SqlValue::Text(json!({"sha": other_sha, "parents": []}).to_string()),
            ],
            label: Some("test_conflicting_commit_key_holder".into()),
        })
        .await
        .expect("seed pre-existing key holder outside the hook");
    drop(writer);

    let mut options = IngestOptions::unbounded(repo.to_path_buf(), project.to_string());
    options.include.issues = false;
    options.include.pull_requests = false;
    let report = run_ingest(&runtime, &token, &registry, options)
        .await
        .expect("a key collision is an in-band commit write failure");
    assert!(report.cursor_stalled && !report.done, "{report:?}");
    assert_eq!(read_git_cursor(&runtime, project, "commits").await, None);
    assert!(
        report.warnings.iter().any(|warning| {
            warning.contains(&holder_id.to_string())
                && warning.contains("kind commit")
                && warning.contains(&other_sha)
                && warning.contains(&sha)
        }),
        "the warning must identify the conflicting holder: {report:?}"
    );
}
