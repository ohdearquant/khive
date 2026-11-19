// ── KindHook validation unit tests ──────────────────────────────────────────

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_hook_rejects_bad_sha() {
    let (_rt, _token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "content": "bad commit",
                "properties": {"sha": "not-a-sha"},
            }),
        )
        .await
        .expect_err("bad sha must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("40-character hex"),
        "error must name the valid shape: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_hook_rejects_ungoverned_state_reason() {
    let (_rt, _token, registry) = fixture().await;
    let project_id = create(&registry, json!({"kind": "project", "name": "hook-repo"})).await;
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "content": "bad issue",
                "properties": {
                    "number": 7,
                    "state_reason": "wontfix",
                    "project_id": project_id.to_string(),
                },
            }),
        )
        .await
        .expect_err("ungoverned state_reason must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("completed") && msg.contains("not_planned"),
        "error must list the governed set: {msg}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_hook_requires_exact_project_id_and_canonicalizes_complete_spelling() {
    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "roundtrip-hook-repo"}),
    )
    .await;

    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "content": "canonical project anchor",
                "properties": {
                    "number": 1482,
                    "project_id": format!("{{{}}}", project_id),
                },
            }),
        )
        .await
        .expect("complete alternate UUID spelling is accepted");
    let issue_id = created["id"]
        .as_str()
        .expect("created issue id")
        .parse::<Uuid>()
        .expect("created issue UUID");
    let stored = rt
        .notes(&token)
        .expect("notes store")
        .get_note(issue_id)
        .await
        .expect("read issue")
        .expect("issue exists");
    let canonical_project_id = project_id.as_hyphenated().to_string();
    assert_eq!(
        stored
            .properties
            .as_ref()
            .and_then(|properties| properties.get("project_id"))
            .and_then(Value::as_str),
        Some(canonical_project_id.as_str()),
        "the persisted strict provenance handle is canonical lowercase dashed form"
    );

    let project_prefix = project_id.to_string()[..8].to_string();
    let error = registry
        .dispatch(
            "create",
            json!({
                "kind": "pull_request",
                "content": "prefix is not an exact project anchor",
                "properties": {
                    "number": 1483,
                    "project_id": project_prefix,
                },
            }),
        )
        .await
        .expect_err("project prefix must be rejected");
    let message = error.to_string();
    assert!(message.contains("can miss or be ambiguous"), "{message}");
    assert!(message.contains("one exact project"), "{message}");
}

/// Before this fix, the masking boundary only
/// lowercased `stateReason` -- validation against the governed enum was left
/// entirely to `IssueLikeHook::prepare_create`, whose error interpolated the
/// raw value verbatim (`"issue properties.state_reason {reason:?} invalid"`).
/// That error propagated straight into `ingest_issues`'s `report.warnings`
/// (`format!("create issue #{number}: {e}")`), so a credential-shaped
/// `stateReason` landed in the ingest report before the secret gate (which
/// only scans title/body/labels/author) ever had a chance to see it.
///
/// This drives the full `run_ingest` path with a credential-shaped
/// `stateReason` and asserts it never appears anywhere in `report.warnings`,
/// is never persisted as an issue record, and the record is cleanly
/// warn-and-skipped (fail-closed, matching ADR-088 §3) rather than silently
/// coerced or dropped-but-created.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_ingest_never_echoes_credential_shaped_state_reason() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "state-reason-leak-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    const CREDENTIAL: &str = "ghp_FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE1";

    let issue_json = json!([
        {"number": 42, "title": "credential-shaped stateReason", "author": {"login": "a"},
         "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:00Z",
         "updatedAt": "2026-01-01T00:00:00Z", "labels": [], "stateReason": CREDENTIAL, "body": ""}
    ])
    .to_string();

    write_fake_gh(&repo, &bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.pull_requests = false;
    opts.include.commits = false;

    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 0,
        "the ungoverned-stateReason record must be rejected, not created: {report:?}"
    );
    assert!(
        !report.warnings.is_empty(),
        "the rejection must be reported as a warning: {report:?}"
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("issue #42")),
        "the warning must name the rejected record: {:?}",
        report.warnings
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains(CREDENTIAL)),
        "the raw credential-shaped stateReason must never appear in report.warnings: {:?}",
        report.warnings
    );
    assert_eq!(
        report.writes_refused, 0,
        "a governed-field validation failure is not a secret-gate refusal"
    );
    assert!(report.write_refusals.is_empty());

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = list_items(&issues_list);
    assert!(
        items.is_empty(),
        "the ungoverned record must never land: {items:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_hook_requires_properties_project_id() {
    let (_rt, _token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "content": "no project_id",
                "properties": {"number": 8},
            }),
        )
        .await
        .expect_err("missing project_id must be rejected");
    assert!(format!("{err}").contains("project_id"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_hook_requires_properties_sha() {
    let (_rt, _token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "create",
            json!({"kind": "commit", "content": "no sha", "properties": {}}),
        )
        .await
        .expect_err("missing sha must be rejected");
    assert!(format!("{err}").contains("sha"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_hook_rejects_invalid_changed_path_shapes() {
    let (_rt, _token, registry) = fixture().await;
    for changed_paths in [
        json!("src/lib.rs"),
        json!(["/absolute.rs"]),
        json!(["C:/absolute.rs"]),
        json!(["C:\\absolute.rs"]),
        json!(["C:drive-relative.rs"]),
        json!(["src\\file.rs"]),
        json!(["\\\\server\\share\\file.rs"]),
        json!(["src/../secret.rs"]),
        json!([""]),
        json!(["src//file.rs"]),
        json!(["./src/file.rs"]),
        json!(["src/./file.rs"]),
        json!(["src/file.rs\0trailing"]),
        json!([7]),
        json!(["z.rs", "a.rs"]),
        json!(["same.rs", "same.rs"]),
    ] {
        let err = registry
            .dispatch(
                "create",
                json!({
                    "kind": "commit",
                    "content": "invalid changed path fixture",
                    "properties": {
                        "sha": "0000000000000000000000000000000000000000",
                        "changed_paths": changed_paths
                    }
                }),
            )
            .await
            .expect_err("invalid changed_paths must be rejected");
        assert!(format!("{err}").contains("changed_paths"), "{err}");
    }
}

/// The contract's canonical shape for a genuinely empty commit is `[]`;
/// the hook must accept it (a missing or null `changed_paths` is likewise
/// optional for manually created commit notes).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_hook_accepts_empty_changed_paths() {
    let (_rt, _token, registry) = fixture().await;
    registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "content": "empty commit fixture",
                "properties": {
                    "sha": "0000000000000000000000000000000000000000",
                    "changed_paths": []
                }
            }),
        )
        .await
        .expect("an empty changed_paths array is the canonical empty-commit shape");

    // An explicit JSON `null` carries no path facts and is treated the same
    // as an absent property.
    registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "content": "null changed_paths fixture",
                "properties": {
                    "sha": "1111111111111111111111111111111111111111",
                    "changed_paths": null
                }
            }),
        )
        .await
        .expect("null changed_paths is optional, same as absent");
}

/// The must-keep control for the canonical shape: a sorted, deduplicated
/// array of `/`-separated repo-relative paths is accepted (e.g. `src/lib.rs`
/// survives the tightened path-shape validation unchanged).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn commit_hook_accepts_canonical_changed_paths() {
    let (_rt, _token, registry) = fixture().await;
    registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "content": "canonical changed_paths fixture",
                "properties": {
                    "sha": "2222222222222222222222222222222222222222",
                    "changed_paths": ["src/lib.rs"]
                }
            }),
        )
        .await
        .expect("a sorted, deduplicated repo-relative path array is accepted");

    // The multi-element canonical shape the ingester emits: every element
    // repo-relative, the whole array sorted and deduplicated.
    registry
        .dispatch(
            "create",
            json!({
                "kind": "commit",
                "content": "canonical multi-path fixture",
                "properties": {
                    "sha": "3333333333333333333333333333333333333333",
                    "changed_paths": [
                        "crates/a/src/lib.rs",
                        "crates/b/src/lib.rs",
                        "docs/api/ingest.md",
                        "src/main.rs"
                    ]
                }
            }),
        )
        .await
        .expect("a sorted multi-element repo-relative path array is accepted");
}
