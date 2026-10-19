use super::*;
use std::io::Write;

#[path = "reconcile_tests/annotation_guards_tests.rs"]
mod annotation_guards;

use khive_runtime::{Namespace, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

use crate::GitPack;

fn git(repo: &Path, args: &[&str]) -> String {
    let output = git_command(repo, args)
        .expect("enumerate fixture filters")
        .output()
        .expect("start git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .into()
}

fn git_with_input(repo: &Path, args: &[&str], input: &[u8]) -> String {
    let mut child = git_command(repo, args)
        .expect("enumerate fixture filters")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start git");
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().expect("wait for git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .into()
}

fn commit(repo: &Path, name: &str) -> String {
    std::fs::write(repo.join(name), name).expect("write fixture file");
    git(repo, &["add", name]);
    git(repo, &["commit", "-q", "-m", name]);
    git(repo, &["rev-parse", "HEAD"])
}

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    registry: VerbRegistry,
    repo: tempfile::TempDir,
    project_a: Uuid,
    project_b: Uuid,
    source_identity: String,
    first_sha: String,
    second_sha: String,
    first_note: Uuid,
    second_note: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(GitPack::new(runtime.clone()));
        builder
            .with_runtime_event_store(&runtime)
            .expect("event store");
        let registry = builder.build().expect("registry");
        runtime.install_edge_rules(registry.all_edge_rules());
        registry.apply_schema_plans(runtime.backend());

        let repo = tempfile::tempdir().expect("repository");
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        let first_sha = commit(repo.path(), "first.txt");
        let second_sha = commit(repo.path(), "second.txt");
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/khive/reconcile-fixture.git",
            ],
        );
        let identity = repo_identity(&DigestSource::Local(repo.path().to_path_buf())).await;
        let project_a = Self::create(
            &registry,
            json!({"kind":"project","name":"anchor A","properties":{"repo_slug":identity}}),
        )
        .await;
        let project_b = Self::create(
            &registry,
            json!({"kind":"project","name":"anchor B","properties":{"repo_slug":identity}}),
        )
        .await;
        let first_note = Self::create(
            &registry,
            json!({"kind":"commit","name":"first","content":"first",
                       "properties":{"sha":first_sha},"annotates":[project_a.to_string()]}),
        )
        .await;
        let second_note = Self::create(
            &registry,
            json!({"kind":"commit","name":"second","content":"second",
                       "properties":{"sha":second_sha},"annotates":[project_b.to_string()]}),
        )
        .await;
        let fixture = Self {
            runtime,
            token,
            registry,
            repo,
            project_a,
            project_b,
            source_identity: identity,
            first_sha,
            second_sha,
            first_note,
            second_note,
        };
        fixture
            .set_checkpoint(
                fixture.project_b,
                None,
                &fixture.second_sha,
                &fixture.second_sha,
            )
            .await;
        fixture
    }

    async fn create(registry: &VerbRegistry, request: Value) -> Uuid {
        let response = registry.dispatch("create", request).await.expect("create");
        Uuid::parse_str(response["id"].as_str().expect("created id")).expect("uuid")
    }

    async fn set_checkpoint(&self, project: Uuid, base: Option<&str>, head: &str, last: &str) {
        let checkpoint = json!({
            "version": 1,
            "namespace": "local",
            "base_cursor": base,
            "snapshot_head": head,
            "last_completed_sha": last,
        });
        self.runtime
            .sql()
            .writer()
            .await
            .expect("writer")
            .execute(SqlStatement {
                sql: crate::sql::sql!("commit_checkpoint_upsert").into(),
                params: vec![
                    SqlValue::Text(project.to_string()),
                    SqlValue::Text(checkpoint.to_string()),
                    SqlValue::Text(last.into()),
                    SqlValue::Integer(123),
                ],
                label: Some("test_commit_checkpoint".into()),
            })
            .await
            .expect("checkpoint write");
    }

    fn options(&self, apply_preview_id: Option<String>) -> ReconcileOptions {
        ReconcileOptions {
            repo: self.repo.path().to_path_buf(),
            project_id: self.project_b,
            frozen_tip: self.second_sha.clone(),
            apply_preview_id,
        }
    }

    async fn run(&self, apply_preview_id: Option<String>) -> ReconcileReport {
        run_reconciliation(&self.runtime, &self.token, self.options(apply_preview_id))
            .await
            .expect("repair report")
    }

    async fn edge_bytes(&self, note: Uuid) -> Option<Vec<u8>> {
        let row = self
            .runtime
            .sql()
            .reader()
            .await
            .expect("reader")
            .query_row(SqlStatement {
                sql: "SELECT namespace,id,source_id,target_id,relation,weight, \
                          created_at,updated_at,deleted_at,metadata,target_backend \
                          FROM graph_edges WHERE namespace='local' AND source_id=?1 \
                          AND target_id=?2 AND relation='annotates'"
                    .into(),
                params: vec![
                    SqlValue::Text(note.to_string()),
                    SqlValue::Text(self.project_b.to_string()),
                ],
                label: Some("test_annotation_edge_bytes".into()),
            })
            .await
            .expect("edge read");
        row.map(|row| serde_json::to_vec(&row).expect("serialize row"))
    }

    async fn link_created_events(&self, note: Uuid) -> i64 {
        let count = self
            .runtime
            .sql()
            .reader()
            .await
            .expect("reader")
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM events WHERE verb='link' \
                          AND kind='link_created' AND target_id IN \
                          (SELECT id FROM graph_edges WHERE namespace='local' \
                           AND source_id=?1 AND target_id=?2 AND relation='annotates')"
                    .into(),
                params: vec![
                    SqlValue::Text(note.to_string()),
                    SqlValue::Text(self.project_b.to_string()),
                ],
                label: Some("test_repair_link_created_events".into()),
            })
            .await
            .expect("event count")
            .expect("COUNT returns a row");
        match count {
            SqlValue::Integer(count) => count,
            other => panic!("unexpected event count: {other:?}"),
        }
    }

    async fn execute(&self, sql: &str, params: Vec<SqlValue>) {
        self.runtime
            .sql()
            .writer()
            .await
            .expect("writer")
            .execute(SqlStatement {
                sql: sql.into(),
                params,
                label: Some("test_annotation_mutation".into()),
            })
            .await
            .expect("fixture mutation");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn two_anchors_repair_one_old_shared_sha_and_preserve_cursor_and_curated_edge() {
    let fixture = Fixture::new().await;
    assert_ne!(fixture.project_a, fixture.project_b);
    fixture
        .execute(
            "UPDATE graph_edges SET weight=0.42, metadata='{\"curated\":true}' \
                 WHERE source_id=?1 AND target_id=?2 AND relation='annotates'",
            vec![
                SqlValue::Text(fixture.second_note.to_string()),
                SqlValue::Text(fixture.project_b.to_string()),
            ],
        )
        .await;
    let curated_before = fixture.edge_bytes(fixture.second_note).await;
    let cursor_before = cursor_snapshot(&fixture.runtime, fixture.project_b)
        .await
        .unwrap();
    let preview = fixture.run(None).await;
    assert!(preview.complete_coverage, "{:?}", preview.diagnostics);
    assert_eq!(preview.counts.acknowledged_shas_examined, 2);
    assert_eq!(preview.counts.repairable_missing_links, 1);
    assert_eq!(preview.counts.live_project_edges, 1);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);
    assert_eq!(fixture.link_created_events(fixture.first_note).await, 0);
    assert_eq!(
        fixture.edge_bytes(fixture.second_note).await,
        curated_before
    );
    let applied = fixture.run(Some(preview.preview_id)).await;
    assert!(applied.success, "{applied:?}");
    assert_eq!(applied.apply.as_ref().unwrap().created, 1);
    assert!(fixture.edge_bytes(fixture.first_note).await.is_some());
    assert_eq!(fixture.link_created_events(fixture.first_note).await, 1);
    assert_eq!(
        fixture.edge_bytes(fixture.second_note).await,
        curated_before
    );
    let repaired_edge = fixture.edge_bytes(fixture.first_note).await;
    let repeat_preview = fixture.run(None).await;
    assert_eq!(repeat_preview.counts.repairable_missing_links, 0);
    let repeat = fixture.run(Some(repeat_preview.preview_id)).await;
    assert!(repeat.success);
    assert_eq!(repeat.apply.as_ref().unwrap().created, 0);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, repaired_edge);
    assert_eq!(fixture.link_created_events(fixture.first_note).await, 1);
    assert_eq!(
        cursor_snapshot(&fixture.runtime, fixture.project_b)
            .await
            .unwrap(),
        cursor_before
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn conditional_annotation_link_uses_normal_endpoint_rules() {
    let fixture = Fixture::new().await;
    let guard = annotation_guard(
        &cursor_snapshot(&fixture.runtime, fixture.project_b)
            .await
            .unwrap(),
        &fixture.first_sha,
        &fixture.source_identity,
    )
    .unwrap();
    let error = link_annotation_if_absent(
        &fixture.runtime,
        &fixture.token,
        fixture.project_a,
        fixture.project_b,
        guard,
    )
    .await
    .expect_err("an entity cannot be an annotates source");
    assert!(error.to_string().contains("annotates source"), "{error}");
    let written = fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("reader")
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM graph_edges WHERE namespace='local' \
                      AND source_id=?1 AND target_id=?2 AND relation='annotates'"
                .into(),
            params: vec![
                SqlValue::Text(fixture.project_a.to_string()),
                SqlValue::Text(fixture.project_b.to_string()),
            ],
            label: Some("test_rejected_annotation_edge_count".into()),
        })
        .await
        .expect("edge count");
    assert!(matches!(written, Some(SqlValue::Integer(0))));
    assert_eq!(fixture.link_created_events(fixture.project_a).await, 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn conditional_annotation_link_refuses_a_stale_cursor_before_inserting() {
    let fixture = Fixture::new().await;
    let guard = annotation_guard(
        &cursor_snapshot(&fixture.runtime, fixture.project_b)
            .await
            .unwrap(),
        &fixture.first_sha,
        &fixture.source_identity,
    )
    .unwrap();
    fixture
        .execute(
            "UPDATE git_mirror_cursor SET updated_at=updated_at+1 \
                 WHERE project_id=?1 AND kind='commits'",
            vec![SqlValue::Text(fixture.project_b.to_string())],
        )
        .await;

    let outcome = link_annotation_if_absent(
        &fixture.runtime,
        &fixture.token,
        fixture.first_note,
        fixture.project_b,
        guard,
    )
    .await
    .expect("stale cursor is a classified refusal");
    assert!(matches!(outcome, AnnotationOutcome::CursorChanged));
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);
    assert_eq!(fixture.link_created_events(fixture.first_note).await, 0);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn conditional_annotation_link_emits_only_for_created_outcome() {
    let fixture = Fixture::new().await;
    let guard = annotation_guard(
        &cursor_snapshot(&fixture.runtime, fixture.project_b)
            .await
            .unwrap(),
        &fixture.first_sha,
        &fixture.source_identity,
    )
    .unwrap();
    assert_eq!(fixture.link_created_events(fixture.first_note).await, 0);

    let created = link_annotation_if_absent(
        &fixture.runtime,
        &fixture.token,
        fixture.first_note,
        fixture.project_b,
        guard.clone(),
    )
    .await
    .expect("new annotation");
    assert!(matches!(created, AnnotationOutcome::Created));
    assert_eq!(fixture.link_created_events(fixture.first_note).await, 1);
    let live_bytes = fixture.edge_bytes(fixture.first_note).await;
    let before_existing_events = fixture.link_created_events(fixture.first_note).await;

    let existing = link_annotation_if_absent(
        &fixture.runtime,
        &fixture.token,
        fixture.first_note,
        fixture.project_b,
        guard.clone(),
    )
    .await
    .expect("existing live annotation");
    assert!(matches!(existing, AnnotationOutcome::ExistingLive));
    assert_eq!(
        fixture.link_created_events(fixture.first_note).await - before_existing_events,
        0,
        "an existing-live outcome emits zero new LinkCreated events"
    );
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, live_bytes);

    fixture
        .execute(
            "UPDATE graph_edges SET deleted_at=77 WHERE source_id=?1 AND target_id=?2 \
                 AND relation='annotates'",
            vec![
                SqlValue::Text(fixture.first_note.to_string()),
                SqlValue::Text(fixture.project_b.to_string()),
            ],
        )
        .await;
    let tombstone_bytes = fixture.edge_bytes(fixture.first_note).await;
    let before_tombstone_events = fixture.link_created_events(fixture.first_note).await;
    let tombstoned = link_annotation_if_absent(
        &fixture.runtime,
        &fixture.token,
        fixture.first_note,
        fixture.project_b,
        guard,
    )
    .await
    .expect("tombstoned annotation");
    assert!(matches!(tombstoned, AnnotationOutcome::Tombstoned));
    assert_eq!(
        fixture.link_created_events(fixture.first_note).await - before_tombstone_events,
        0,
        "a tombstoned outcome emits zero new LinkCreated events"
    );
    assert_eq!(
        fixture.edge_bytes(fixture.first_note).await,
        tombstone_bytes
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn tombstone_is_an_exclusion_and_remains_byte_exact() {
    let fixture = Fixture::new().await;
    link_annotation_if_absent(
        &fixture.runtime,
        &fixture.token,
        fixture.first_note,
        fixture.project_b,
        annotation_guard(
            &cursor_snapshot(&fixture.runtime, fixture.project_b)
                .await
                .unwrap(),
            &fixture.first_sha,
            &fixture.source_identity,
        )
        .unwrap(),
    )
    .await
    .expect("fixture link");
    fixture
        .execute(
            "UPDATE graph_edges SET deleted_at=77 WHERE source_id=?1 AND target_id=?2 \
                 AND relation='annotates'",
            vec![
                SqlValue::Text(fixture.first_note.to_string()),
                SqlValue::Text(fixture.project_b.to_string()),
            ],
        )
        .await;
    let before = fixture.edge_bytes(fixture.first_note).await;
    let preview = fixture.run(None).await;
    assert_eq!(preview.counts.tombstones_skipped, 1);
    assert_eq!(preview.counts.repairable_missing_links, 0);
    let apply = fixture.run(Some(preview.preview_id)).await;
    assert!(apply.success);
    assert_eq!(apply.apply.as_ref().unwrap().created, 0);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn curated_link_racing_preview_is_preserved_byte_exact() {
    let fixture = Fixture::new().await;
    let preview = fixture.run(None).await;
    assert_eq!(preview.counts.repairable_missing_links, 1);
    test_hooks::insert_curated_before_first();
    let applied = fixture.run(Some(preview.preview_id)).await;
    assert!(applied.success, "{applied:?}");
    assert_eq!(applied.apply.as_ref().unwrap().created, 0);
    assert_eq!(applied.apply.as_ref().unwrap().raced_live, 1);
    let edge = fixture.edge_bytes(fixture.first_note).await.unwrap();
    assert!(String::from_utf8_lossy(&edge).contains("curated"));
    let again = fixture.run(None).await;
    assert_eq!(again.counts.repairable_missing_links, 0);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await.unwrap(), edge);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cursor_change_before_first_link_blocks_every_write() {
    let fixture = Fixture::new().await;
    let preview = fixture.run(None).await;
    test_hooks::change_cursor_before_first();
    let applied = fixture.run(Some(preview.preview_id)).await;
    assert!(!applied.success);
    assert_eq!(applied.apply.as_ref().unwrap().created, 0);
    assert_eq!(applied.apply.as_ref().unwrap().failures, 2);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn missing_deleted_and_ambiguous_notes_refuse_apply_without_fabrication() {
    let deleted = Fixture::new().await;
    deleted
        .execute(
            "UPDATE notes SET deleted_at=42 WHERE id=?1",
            vec![SqlValue::Text(deleted.first_note.to_string())],
        )
        .await;
    let preview = deleted.run(None).await;
    assert_eq!(preview.counts.deleted_notes, 1);
    let apply = deleted.run(Some(preview.preview_id)).await;
    assert!(!apply.success);
    assert_eq!(apply.apply.as_ref().unwrap().created, 0);

    let missing = Fixture::new().await;
    missing
        .execute(
            "DELETE FROM notes WHERE id=?1",
            vec![SqlValue::Text(missing.first_note.to_string())],
        )
        .await;
    let preview = missing.run(None).await;
    assert_eq!(preview.counts.missing_notes, 1);
    assert!(!missing.run(Some(preview.preview_id)).await.success);

    let ambiguous = Fixture::new().await;
    let _duplicate = Fixture::create(
        &ambiguous.registry,
        json!({"kind":"commit","name":"duplicate","content":"duplicate",
                   "properties":{"sha":ambiguous.first_sha}}),
    )
    .await;
    let preview = ambiguous.run(None).await;
    assert_eq!(preview.counts.ambiguous_notes, 1);
    assert!(!ambiguous.run(Some(preview.preview_id)).await.success);
    assert_eq!(ambiguous.edge_bytes(ambiguous.first_note).await, None);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn link_failure_after_one_write_reports_partial_and_leaves_retryable_candidate() {
    let fixture = Fixture::new().await;
    fixture
        .execute(
            "DELETE FROM graph_edges WHERE source_id=?1 AND target_id=?2 \
                 AND relation='annotates'",
            vec![
                SqlValue::Text(fixture.second_note.to_string()),
                SqlValue::Text(fixture.project_b.to_string()),
            ],
        )
        .await;
    let preview = fixture.run(None).await;
    assert_eq!(preview.counts.repairable_missing_links, 2);
    test_hooks::set(test_hooks::Fault::DeleteNote(fixture.second_note));
    let applied = fixture.run(Some(preview.preview_id)).await;
    assert!(!applied.success);
    assert_eq!(applied.apply.as_ref().unwrap().created, 1);
    assert_eq!(applied.apply.as_ref().unwrap().failures, 1);
    assert!(fixture.edge_bytes(fixture.first_note).await.is_some());
    assert_eq!(fixture.edge_bytes(fixture.second_note).await, None);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cursor_byte_or_timestamp_change_refuses_stale_or_inflight_apply() {
    let fixture = Fixture::new().await;
    let preview = fixture.run(None).await;
    fixture
        .execute(
            "UPDATE git_mirror_cursor SET cursor_value=cursor_value || 'x' \
                 WHERE project_id=?1 AND kind='commits'",
            vec![SqlValue::Text(fixture.project_b.to_string())],
        )
        .await;
    let refused = fixture.run(Some(preview.preview_id)).await;
    assert!(!refused.success);
    assert_eq!(refused.apply.as_ref().unwrap().created, 0);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);

    let checkpoint = Fixture::new().await;
    let preview = checkpoint.run(None).await;
    checkpoint
        .execute(
            "UPDATE git_mirror_cursor SET cursor_value=cursor_value || 'x' \
                 WHERE project_id=?1 AND kind='commits_checkpoint'",
            vec![SqlValue::Text(checkpoint.project_b.to_string())],
        )
        .await;
    let refused = checkpoint.run(Some(preview.preview_id)).await;
    assert!(!refused.success);
    assert_eq!(refused.apply.as_ref().unwrap().created, 0);
    assert_eq!(checkpoint.edge_bytes(checkpoint.first_note).await, None);

    let inflight = Fixture::new().await;
    let preview = inflight.run(None).await;
    test_hooks::set(test_hooks::Fault::ChangeCursorTimestamp);
    let applied = inflight.run(Some(preview.preview_id)).await;
    assert!(!applied.success);
    assert!(!applied.cursors_unchanged);
    assert_eq!(applied.apply.as_ref().unwrap().created, 1);
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn configured_gpg_program_is_not_executed_during_preview_or_apply() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new().await;
    let tree_ref = format!("{}^{{tree}}", fixture.second_sha);
    let tree = git(fixture.repo.path(), &["rev-parse", &tree_ref]);
    let raw = format!(
        "tree {tree}\nparent {}\nauthor Test <test@example.com> 946684800 +0000\n\
             committer Test <test@example.com> 946684800 +0000\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n fake\n -----END PGP SIGNATURE-----\n\n\
             signed child\n",
        fixture.second_sha
    );
    let signed_sha = git_with_input(
        fixture.repo.path(),
        &["hash-object", "-w", "-t", "commit", "--stdin"],
        raw.as_bytes(),
    );
    assert!(git(fixture.repo.path(), &["cat-file", "-p", &signed_sha]).contains("gpgsig"));
    git(
        fixture.repo.path(),
        &["update-ref", "refs/heads/main", &signed_sha],
    );
    fixture
        .set_checkpoint(fixture.project_b, None, &signed_sha, &fixture.second_sha)
        .await;

    let marker_dir = tempfile::tempdir().unwrap();
    let script = marker_dir.path().join("gpg-marker.sh");
    let marker = marker_dir.path().join("called");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf called > \"{}\"\nexit 1\n",
            marker.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script, permissions).unwrap();
    git(
        fixture.repo.path(),
        &["config", "log.showSignature", "true"],
    );
    git(
        fixture.repo.path(),
        &["config", "gpg.program", script.to_str().unwrap()],
    );

    let mut options = fixture.options(None);
    options.frozen_tip = signed_sha;
    let preview = run_reconciliation(&fixture.runtime, &fixture.token, options.clone())
        .await
        .unwrap();
    assert!(preview.complete_coverage, "{preview:?}");
    assert!(!marker.exists(), "preview executed repository gpg.program");
    options.apply_preview_id = Some(preview.preview_id);
    let applied = run_reconciliation(&fixture.runtime, &fixture.token, options)
        .await
        .unwrap();
    assert!(applied.success, "{applied:?}");
    assert!(!marker.exists(), "apply executed repository gpg.program");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn full_checkpoint_in_shallow_clone_refuses_incomplete_coverage_and_apply() {
    let fixture = Fixture::new().await;
    let third_sha = commit(fixture.repo.path(), "third.txt");
    Fixture::create(
        &fixture.registry,
        json!({"kind":"commit","name":"third","content":"third",
                   "properties":{"sha":third_sha.clone()},
                   "annotates":[fixture.project_b.to_string()]}),
    )
    .await;
    fixture
        .set_checkpoint(fixture.project_b, None, &third_sha, &third_sha)
        .await;
    let mut options = fixture.options(None);
    options.frozen_tip = third_sha.clone();
    let full = run_reconciliation(&fixture.runtime, &fixture.token, options.clone())
        .await
        .unwrap();
    assert!(full.complete_coverage, "{full:?}");
    assert_eq!(full.counts.acknowledged_shas_examined, 3);
    assert_eq!(full.counts.repairable_missing_links, 1);

    let shallow = tempfile::tempdir().unwrap();
    let source = format!("file://{}", fixture.repo.path().display());
    git(
        fixture.repo.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "clone",
            "--quiet",
            "--depth=2",
            "--no-local",
            &source,
            shallow.path().to_str().unwrap(),
        ],
    );
    git(
        shallow.path(),
        &[
            "config",
            "remote.origin.url",
            "https://github.com/khive/reconcile-fixture.git",
        ],
    );
    assert_eq!(
        repo_identity(&DigestSource::Local(shallow.path().to_path_buf())).await,
        fixture.source_identity
    );
    assert_eq!(
        git(shallow.path(), &["rev-parse", "--is-shallow-repository"]),
        "true"
    );
    let shallow_walk = git(shallow.path(), &["log", "--format=%H", &third_sha]);
    assert!(!shallow_walk
        .lines()
        .any(|sha| sha == fixture.first_sha.as_str()));

    options.repo = shallow.path().to_path_buf();
    let preview = run_reconciliation(&fixture.runtime, &fixture.token, options.clone())
        .await
        .unwrap();
    assert!(!preview.complete_coverage, "{preview:?}");
    assert_eq!(preview.counts.coverage_errors, 1);
    assert!(preview.diagnostics[0].contains("shallow repository"));
    options.apply_preview_id = Some(preview.preview_id);
    let refused = run_reconciliation(&fixture.runtime, &fixture.token, options)
        .await
        .unwrap();
    assert!(!refused.success);
    assert_eq!(refused.apply.as_ref().unwrap().created, 0);
    assert_eq!(fixture.edge_bytes(fixture.first_note).await, None);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn graft_cannot_hide_acknowledged_ancestor_from_reconciliation() {
    let fixture = Fixture::new().await;
    let grafts = fixture.repo.path().join(".git/info/grafts");
    std::fs::write(&grafts, format!("{}\n", fixture.second_sha)).unwrap();

    assert_eq!(
        git(
            fixture.repo.path(),
            &["rev-parse", "--is-shallow-repository"]
        ),
        "false"
    );
    let grafted_walk = git_command(fixture.repo.path(), &["log", "--format=%H", "HEAD"])
        .expect("enumerate fixture filters")
        .env_remove("GIT_GRAFT_FILE")
        .output()
        .expect("grafted walk");
    assert!(grafted_walk.status.success());
    assert_eq!(
        String::from_utf8(grafted_walk.stdout).unwrap().trim(),
        fixture.second_sha
    );

    let preview = fixture.run(None).await;
    assert!(preview.complete_coverage, "{preview:?}");
    assert_eq!(preview.counts.acknowledged_shas_examined, 2);
    assert_eq!(preview.counts.repairable_missing_links, 1);
    let applied = fixture.run(Some(preview.preview_id)).await;
    assert!(applied.success, "{applied:?}");
    assert!(fixture.edge_bytes(fixture.first_note).await.is_some());

    let restored_walk = git_command(fixture.repo.path(), &["log", "--format=%H", "HEAD"])
        .expect("enumerate fixture filters")
        .output()
        .expect("ungrafted walk");
    assert!(restored_walk.status.success());
    assert!(restored_walk.stderr.is_empty());
    assert_eq!(
        String::from_utf8(restored_walk.stdout)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec![fixture.second_sha.as_str(), fixture.first_sha.as_str()]
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn prior_walk_and_unavailable_or_diverged_tip_are_incomplete() {
    let fixture = Fixture::new().await;
    fixture
        .set_checkpoint(
            fixture.project_b,
            Some(&fixture.first_sha),
            &fixture.second_sha,
            &fixture.second_sha,
        )
        .await;
    let incomplete = fixture.run(None).await;
    assert!(!incomplete.complete_coverage);
    assert_eq!(incomplete.counts.coverage_errors, 1);
    assert!(incomplete.diagnostics[0].contains("non-null base_cursor"));
    assert!(!fixture.run(Some(incomplete.preview_id)).await.success);

    let unavailable = Fixture::new().await;
    let mut options = unavailable.options(None);
    options.frozen_tip = "0".repeat(40);
    let preview = run_reconciliation(&unavailable.runtime, &unavailable.token, options)
        .await
        .expect("incomplete preview");
    assert!(!preview.complete_coverage);
    assert_eq!(preview.counts.coverage_errors, 1);

    let diverged = Fixture::new().await;
    git(diverged.repo.path(), &["checkout", "--orphan", "unrelated"]);
    let orphan_sha = commit(diverged.repo.path(), "orphan.txt");
    git(diverged.repo.path(), &["checkout", "main"]);
    let mut options = diverged.options(None);
    options.frozen_tip = orphan_sha;
    let preview = run_reconciliation(&diverged.runtime, &diverged.token, options)
        .await
        .expect("diverged preview");
    assert!(!preview.complete_coverage);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn merge_dag_prefix_includes_a_nonancestor_sibling() {
    let fixture = Fixture::new().await;
    git(
        fixture.repo.path(),
        &["checkout", "-q", "-b", "side", &fixture.first_sha],
    );
    commit(fixture.repo.path(), "side.txt");
    git(fixture.repo.path(), &["checkout", "-q", "main"]);
    commit(fixture.repo.path(), "main.txt");
    git(
        fixture.repo.path(),
        &["merge", "-q", "--no-ff", "-m", "merge branches", "side"],
    );
    let merge = git(fixture.repo.path(), &["rev-parse", "HEAD"]);
    let walk = git(
        fixture.repo.path(),
        &[
            "log",
            "--reverse",
            "--topo-order",
            "--format=%H",
            &merge,
            "--",
        ],
    );
    let shas: Vec<_> = walk.lines().collect();
    let (index, last) = shas
        .iter()
        .enumerate()
        .find(|(index, last)| {
            shas[..*index].iter().any(|older| {
                !git_status(
                    fixture.repo.path(),
                    &["merge-base", "--is-ancestor", older, last],
                )
                .unwrap()
            })
        })
        .expect("merge walk has a sibling before a branch tip");
    let checkpoint = CommitCheckpoint {
        version: 1,
        namespace: "local".into(),
        base_cursor: None,
        snapshot_head: merge,
        last_completed_sha: (*last).into(),
    };
    let prefix = acknowledged_prefix(fixture.repo.path(), &checkpoint).unwrap();
    let expected: Vec<String> = shas[..=index].iter().map(|sha| (*sha).to_owned()).collect();
    assert_eq!(prefix, expected);
    assert!(prefix[..index].iter().any(|older| {
        !git_status(
            fixture.repo.path(),
            &["merge-base", "--is-ancestor", older, last],
        )
        .unwrap()
    }));

    // A non-null base SHA cannot prove that a previous walk visited the
    // non-ancestor sibling above, even though this latest span is valid.
    fixture
        .set_checkpoint(
            fixture.project_b,
            Some(&fixture.first_sha),
            &checkpoint.snapshot_head,
            &checkpoint.last_completed_sha,
        )
        .await;
    let mut options = fixture.options(None);
    options.frozen_tip = checkpoint.snapshot_head.clone();
    let incomplete = run_reconciliation(&fixture.runtime, &fixture.token, options.clone())
        .await
        .expect("incomplete preview");
    assert!(!incomplete.complete_coverage);
    assert_eq!(incomplete.counts.coverage_errors, 1);
    assert!(incomplete.diagnostics[0].contains("non-null base_cursor"));
    options.apply_preview_id = Some(incomplete.preview_id);
    let refused = run_reconciliation(&fixture.runtime, &fixture.token, options)
        .await
        .expect("refused apply");
    assert!(!refused.success);
    assert_eq!(refused.apply.as_ref().unwrap().created, 0);
}
