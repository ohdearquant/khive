//! End-to-end acceptance test for the ADR-088 git-lifecycle pack (v0).
//!
//! Builds a synthetic fixture repo with `git` inside a tempdir, runs one
//! ingest pass against an in-memory runtime, and asserts the provenance
//! query genre works: traversing/searching from a pre-created `document`
//! entity via incoming `annotates` edges yields exactly the commits that
//! touched its path, and a squash-merge commit's PR edge resolves. Also
//! covers `KindHook` validation and secret-masking on ingested content.

use std::path::Path;
use std::process::Command;

use khive_pack_git::ingest::{run_ingest, IngestOptions};
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use uuid::Uuid;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

async fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let rt = rt();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(rt.backend());
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    (rt, token, registry)
}

async fn create(registry: &VerbRegistry, body: Value) -> Uuid {
    let resp = registry.dispatch("create", body).await.expect("create ok");
    Uuid::parse_str(resp["id"].as_str().expect("id present")).expect("id is uuid")
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_repo(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test User"]);
}

fn write(repo: &Path, rel: &str, contents: &str) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn commit(repo: &Path, rel_paths: &[&str], message: &str) {
    for p in rel_paths {
        git(repo, &["add", p]);
    }
    git(repo, &["commit", "-q", "-m", message]);
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Full end-to-end: a fixture repo with three commits (two touching a
/// tracked ADR path, one unrelated), a pre-ingested PR that a squash-merge
/// commit references by `(#NNN)` suffix — asserts the provenance query
/// genre: incoming `annotates` from the document yields exactly the
/// touching commits, and the squash-merge commit's PR edge resolves.
#[tokio::test]
async fn ingest_links_commits_to_document_and_pr_by_provenance_query() {
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
            "properties": {"number": 42, "title": "Add ADR-045"},
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
        IngestOptions {
            repo: repo.to_path_buf(),
            project: project_id.to_string(),
        },
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 3,
        "all three commits ingest: {report:?}"
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

/// Coordinator addendum requirement: a commit message containing a
/// credential-shaped token must be masked before it is stored.
#[tokio::test]
async fn ingest_masks_secrets_in_commit_message() {
    let (rt, token, registry) = fixture().await;

    let project_id = create(&registry, json!({"kind": "project", "name": "secret-repo"})).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    write(repo, "README.md", "hello\n");
    commit(
        repo,
        &["README.md"],
        &format!("Rotate deploy key\n\naccidentally committed {fake_token} here"),
    );

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions {
            repo: repo.to_path_buf(),
            project: project_id.to_string(),
        },
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("list returns a plain array");
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !stored_content.contains(fake_token),
        "raw token must not survive into stored content: {stored_content:?}"
    );
    assert!(
        stored_content.contains("***MASKED***") || stored_content.contains("MASKED"),
        "masked marker must be present: {stored_content:?}"
    );
}

// ── KindHook validation unit tests ──────────────────────────────────────────

#[tokio::test]
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
async fn issue_hook_rejects_ungoverned_state_reason() {
    let (_rt, _token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "create",
            json!({
                "kind": "issue",
                "content": "bad issue",
                "properties": {"number": 7, "state_reason": "wontfix"},
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
