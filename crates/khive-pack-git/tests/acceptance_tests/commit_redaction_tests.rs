const REDACTED_COMMIT_MESSAGE: &str = "Commit message was redacted after a secret-gate refusal.";

fn commit_message_refused_after_field_masking() -> String {
    use khive_runtime::secret_gate::{check, mask_for_redaction_surface, RedactionSurface};

    let subject = "Update configuration documentation";
    let synthetic = ["AK", "IA", "QWERTYUIOPASDFGH"].concat();
    let body = format!(
        "{{\"sample\": \"\\u0041{}\"}}\n{}\nFixes #7",
        &synthetic[1..],
        "plain prose\n".repeat(3_000)
    );
    for field in [subject, body.as_str()] {
        assert_eq!(
            mask_for_redaction_surface(RedactionSurface::GitIngest, field),
            field,
            "the fixture must survive the separate subject and body masking"
        );
    }
    let message = format!("{subject}\n\n{body}");
    assert!(message.len() > 32_768);
    assert!(matches!(
        check(&message).expect_err("the write gate decodes the escaped synthetic credential"),
        RuntimeError::SecretDetected(_)
    ));
    message
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_redaction_preserves_provenance_and_advances_bounded_cursor() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    let project = create(
        &registry,
        json!({"kind": "project", "name": "redacted-commit-repo"}),
    )
    .await;
    let issue = create(
        &registry,
        json!({
            "kind": "issue", "content": "A reference target",
            "properties": {"number": 7, "project_id": project.to_string()},
            "annotates": [project.to_string()],
        }),
    )
    .await;
    let captured = Arc::new(Mutex::new(Vec::new()));
    rt.register_embedder(CapturingEmbedProvider {
        captured: Arc::clone(&captured),
        dims: 8,
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "root.txt", "root\n");
    commit(repo, &["root.txt"], "Root commit");
    let root_sha = head_sha(repo);
    write(repo, "changed.txt", "changed\n");
    commit(
        repo,
        &["changed.txt"],
        &commit_message_refused_after_field_masking(),
    );
    let redacted_sha = head_sha(repo);
    let committed_at = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["show", "-s", "--format=%cI", "HEAD"])
        .output()
        .expect("read commit date");
    assert!(committed_at.status.success());
    let committed_at = String::from_utf8(committed_at.stdout).expect("date UTF-8");
    write(repo, "later.txt", "later\n");
    commit(repo, &["later.txt"], "Later commit");
    let later_sha = head_sha(repo);
    write(repo, "beyond.txt", "beyond\n");
    commit(repo, &["beyond.txt"], "Beyond the first page");
    let final_sha = head_sha(repo);

    let mut opts = IngestOptions::unbounded(repo.to_path_buf(), project.to_string());
    opts.include = IngestInclude {
        commits: true,
        issues: false,
        pull_requests: false,
    };
    opts.max_items = Some(3);
    let report = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .expect("first page");
    assert_eq!(report.commits_ingested, 3, "{report:?}");
    assert!(!report.cursor_stalled, "{report:?}");
    assert_eq!(report.writes_refused, 1, "{report:?}");
    assert_eq!(report.write_refusals.len(), 1);
    assert_eq!(report.write_refusals[0].record_kind, "commit");
    assert_eq!(report.write_refusals[0].record_key, redacted_sha);
    assert_eq!(report.write_refusals[0].redacted, Some(true));
    assert_eq!(report.commit_embeddings_truncated, 0);
    assert_eq!(report.parent_edges_created, 2);
    assert_eq!(report.reference_edges_created, 0);
    assert_eq!(report.reference_edges_unresolved, 0);
    assert_eq!(
        read_git_cursor(&rt, project, "commits").await,
        Some(later_sha.clone())
    );

    let notes = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("commit notes");
    let note = list_items(&notes)
        .iter()
        .find(|note| note["properties"]["sha"] == redacted_sha)
        .expect("redacted note");
    assert_eq!(note["content"], REDACTED_COMMIT_MESSAGE);
    assert!(note["name"].as_str().unwrap().ends_with(" [redacted]"));
    assert_eq!(note["properties"]["author"], "Test User");
    assert_eq!(note["properties"]["author_email"], "test@example.com");
    assert_eq!(note["properties"]["committed_at"], committed_at.trim());
    assert_eq!(note["properties"]["parents"], json!([root_sha]));
    assert_eq!(note["properties"]["changed_paths"], json!(["changed.txt"]));
    let note_id = note["id"].as_str().unwrap();
    for (direction, sha) in [
        ("incoming", root_sha.as_str()),
        ("outgoing", later_sha.as_str()),
    ] {
        let expected = list_items(&notes)
            .iter()
            .find(|row| row["properties"]["sha"] == sha)
            .expect("adjacent commit");
        let neighbors = registry
            .dispatch(
                "neighbors",
                json!({
                    "id": note_id, "direction": direction, "relations": ["precedes"],
                }),
            )
            .await
            .expect("parent edge neighbors");
        let neighbors = neighbors.as_array().expect("neighbor array");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0]["id"], expected["id"]);
    }
    let annotations = registry
        .dispatch(
            "neighbors",
            json!({
                "id": note_id, "direction": "outgoing", "relations": ["annotates"],
            }),
        )
        .await
        .expect("redacted commit annotations");
    let annotations = annotations.as_array().expect("neighbor array");
    assert!(annotations
        .iter()
        .any(|target| target["id"].as_str() == Some(project.to_string().as_str())));
    assert!(annotations
        .iter()
        .all(|target| target["id"].as_str() != Some(issue.to_string().as_str())));
    let keyed = registry
        .dispatch(
            "get",
            json!({"key": format!("git.commit:{redacted_sha}"), "note_kind": "commit"}),
        )
        .await
        .expect("keyed redacted note");
    assert_eq!(keyed["id"], note["id"]);

    let seen = captured.lock().expect("capture").clone();
    assert_eq!(
        seen.len(),
        3,
        "only the stored messages are embedded: {seen:?}"
    );
    assert!(seen.iter().any(|text| text == REDACTED_COMMIT_MESSAGE));
    assert!(seen
        .iter()
        .all(|text| !text.contains("Fixes #7") && !text.contains("QWERTYUIOPASDFGH")));

    let resumed = run_ingest(&rt, &token, &registry, opts.clone())
        .await
        .expect("next page");
    assert_eq!(resumed.commits_ingested, 1, "{resumed:?}");
    assert_eq!(resumed.commits_skipped_existing, 0);
    assert_eq!(resumed.writes_refused, 0);
    assert!(!resumed.cursor_stalled);
    assert_eq!(
        read_git_cursor(&rt, project, "commits").await,
        Some(final_sha)
    );
    let replay = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("finished replay");
    assert_eq!(replay.commits_ingested, 0);
    assert_eq!(replay.writes_refused, 0);
    assert!(replay.done);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_redaction_and_plain_create_failures_stall_until_storage_recovers() {
    let _guard = ENV_MUTEX.lock().await;
    for redact in [false, true] {
        let (rt, token, registry) = fixture().await;
        let project = create(
            &registry,
            json!({"kind": "project", "name": "commit-storage-retry"}),
        )
        .await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_repo(repo);
        write(repo, "root.txt", "root\n");
        commit(repo, &["root.txt"], "Root commit");
        let root_sha = head_sha(repo);
        write(repo, "failed.txt", "failed\n");
        let message = if redact {
            commit_message_refused_after_field_masking()
        } else {
            "Ordinary commit".into()
        };
        commit(repo, &["failed.txt"], &message);
        let failed_sha = head_sha(repo);
        write(repo, "later.txt", "later\n");
        commit(repo, &["later.txt"], "Later commit");
        let final_sha = head_sha(repo);

        let mut writer = rt.sql().writer().await.expect("writer");
        writer.execute(SqlStatement {
            sql: format!("CREATE TRIGGER refuse_selected_commit BEFORE INSERT ON notes WHEN NEW.kind='commit' AND json_extract(NEW.properties, '$.sha')='{failed_sha}' BEGIN SELECT RAISE(ABORT, 'synthetic commit write failure'); END"),
            params: vec![],
            label: Some("test_refuse_selected_commit".into()),
        }).await.expect("install storage failure");
        drop(writer);

        let mut opts = IngestOptions::unbounded(repo.to_path_buf(), project.to_string());
        opts.include = IngestInclude {
            commits: true,
            issues: false,
            pull_requests: false,
        };
        let report = run_ingest(&rt, &token, &registry, opts.clone())
            .await
            .expect("refused pass");
        assert!(report.cursor_stalled && !report.done, "{report:?}");
        assert_eq!(report.commits_ingested, 2);
        assert_eq!(
            read_git_cursor(&rt, project, "commits").await,
            Some(root_sha)
        );
        assert_eq!(report.writes_refused, u64::from(redact));
        if redact {
            assert_eq!(report.write_refusals[0].redacted, Some(false));
        } else {
            assert!(
                report.write_refusals.is_empty(),
                "a storage failure is not a secret refusal"
            );
        }
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("synthetic commit write failure")));
        let notes = registry
            .dispatch("list", json!({"kind": "commit", "limit": 10}))
            .await
            .expect("notes after failure");
        assert!(list_items(&notes)
            .iter()
            .all(|note| note["properties"]["sha"] != failed_sha));

        let mut writer = rt.sql().writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "DROP TRIGGER refuse_selected_commit".into(),
                params: vec![],
                label: Some("test_restore_commit_storage".into()),
            })
            .await
            .expect("restore storage");
        drop(writer);
        let resumed = run_ingest(&rt, &token, &registry, opts)
            .await
            .expect("retry stored history");
        assert!(!resumed.cursor_stalled, "{resumed:?}");
        assert_eq!(resumed.commits_ingested, 1);
        assert_eq!(
            read_git_cursor(&rt, project, "commits").await,
            Some(final_sha)
        );
        if redact {
            assert_eq!(resumed.write_refusals[0].redacted, Some(true));
        }
    }
}
