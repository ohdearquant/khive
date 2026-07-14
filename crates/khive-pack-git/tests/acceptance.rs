//! End-to-end acceptance test for the ADR-088 git-lifecycle pack (v0).
//!
//! Builds a synthetic fixture repo with `git` inside a tempdir, runs one
//! ingest pass against an in-memory runtime, and asserts the provenance
//! query genre works: traversing/searching from a pre-created `document`
//! entity via incoming `annotates` edges yields exactly the commits that
//! touched its path, and a squash-merge commit's PR edge resolves. Also
//! covers `KindHook` validation and secret-masking on ingested content.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use khive_pack_git::ingest::{run_ingest, IngestOptions};
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, EntityCreateSpec, KhiveRuntime, Namespace,
    NamespaceToken, RuntimeConfig, RuntimeError, RuntimeResult, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use uuid::Uuid;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

/// `PATH` (and, transitively, which `gh`/`git` binaries `Command::new` resolves
/// to) is process-global state. Every test that calls `run_ingest` — whether
/// or not it installs a fake `gh` fixture — must serialize on this mutex, or a
/// concurrently running fake-`gh` test's `PATH` mutation leaks into it.
static ENV_MUTEX: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// RAII guard: prepends `bin_dir` to `PATH` for the duration of the guard,
/// restoring the prior `PATH` on drop (even on panic).
struct PathGuard {
    prior: Option<String>,
}

impl PathGuard {
    fn install(bin_dir: &Path) -> Self {
        let prior = std::env::var("PATH").ok();
        let new_path = match &prior {
            Some(p) => format!("{}:{p}", bin_dir.display()),
            None => bin_dir.display().to_string(),
        };
        std::env::set_var("PATH", new_path);
        Self { prior }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.prior {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// Write a PATH-shadowing fake `gh` executable into `bin_dir` that logs every
/// invocation's cwd and argv into `log_dir`, and replies to `pr list` /
/// `issue list` with the given canned JSON bodies (and to `--version` with a
/// trivial success, matching `gh_available`'s probe).
fn write_fake_gh(bin_dir: &Path, log_dir: &Path, pr_json: &str, issue_json: &str) {
    std::fs::write(log_dir.join("pr_response.json"), pr_json).expect("write pr fixture");
    std::fs::write(log_dir.join("issue_response.json"), issue_json).expect("write issue fixture");

    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$(pwd)" >> '{cwd_log}'
printf '%s\n' "$*" >> '{args_log}'
case "$1" in
  --version)
    echo "gh version 2.0.0 (fake)"
    ;;
  pr)
    cat '{pr_json_path}'
    ;;
  issue)
    cat '{issue_json_path}'
    ;;
  *)
    echo "fake gh: unsupported args: $*" 1>&2
    exit 1
    ;;
esac
"#,
        cwd_log = log_dir.join("cwd.log").display(),
        args_log = log_dir.join("args.log").display(),
        pr_json_path = log_dir.join("pr_response.json").display(),
        issue_json_path = log_dir.join("issue_response.json").display(),
    );
    let script_path = bin_dir.join("gh");
    std::fs::write(&script_path, script).expect("write fake gh script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("fake gh metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod fake gh");
    }
}

async fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let rt = rt();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    // Mirrors the production boot sequence (`serve.rs`): without this call,
    // `KhiveRuntime`'s pack-installed entity-type validator (the
    // `create_many` defense-in-depth layer) is never wired, so a test built
    // on a fixture that skips it would still pass even if that runtime-layer
    // aggregate were absent or builtin-only (PR #925).
    registry.call_register_entity_type_validators(&rt);
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

/// Sentinel body substring that triggers [`FailOnceEmbeddingService`].
const CURSOR_FAIL_SENTINEL: &str = "cursor-fail-sentinel";

/// #763 masks credential-shaped PR title/body instead of dropping the record
/// (design: `architect-2/approved_design.md` §3), so the pre-#763 cursor-stall
/// fixture — a leaked-credential-shaped PR title — now lands successfully
/// instead of failing. This test-only embedder is the design's mandated
/// replacement: it fails exactly once for any text containing
/// [`CURSOR_FAIL_SENTINEL`], then succeeds for every later call (including a
/// retry of the same content), giving cursor-stall tests a deterministic,
/// secret-detection-independent create failure.
struct FailOnceEmbeddingService {
    failed_once: AtomicBool,
}

#[async_trait]
impl EmbeddingService for FailOnceEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.iter().any(|t| t.contains(CURSOR_FAIL_SENTINEL))
            && !self.failed_once.swap(true, Ordering::SeqCst)
        {
            return Err(EmbedError::Internal(
                "injected cursor-stall test failure".to_string(),
            ));
        }
        Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fail-once-test-embedder"
    }
}

struct FailOnceEmbedderProvider;

#[async_trait]
impl EmbedderProvider for FailOnceEmbedderProvider {
    fn name(&self) -> &str {
        "fail-once-test-embedder"
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(FailOnceEmbeddingService {
            failed_once: AtomicBool::new(false),
        }))
    }
}

/// Full end-to-end: a fixture repo with three commits (two touching a
/// tracked ADR path, one unrelated), a pre-ingested PR that a squash-merge
/// commit references by `(#NNN)` suffix — asserts the provenance query
/// genre: incoming `annotates` from the document yields exactly the
/// touching commits, and the squash-merge commit's PR edge resolves.
#[tokio::test]
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
    let _guard = ENV_MUTEX.lock().await;
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
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
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

/// Issue #763 exact acceptance repro: a PR body containing a bare 64-char hex
/// hash near the standalone word "token" must ingest with only the flagged
/// span masked — the containing PR note (and its surrounding prose) must be
/// retained, not dropped.
#[tokio::test]
async fn ingest_masks_pr_body_hash_near_token_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-hash-near-token-repo"}),
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

    let hex64 = "deadbeef".repeat(8);
    assert_eq!(hex64.len(), 64, "fixture hash must be exactly 64 hex chars");
    let pr_json = json!([{
        "number": 42,
        "title": "Document the rotation process",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "docs/rotation",
        "mergeCommit": null,
        "body": format!("Rotated the deploy token. Old hash was {hex64} before rotation.")
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !content.contains(&hex64),
        "raw 64-hex hash must not survive into stored content: {content:?}"
    );
    let expected = "Rotated the deploy token. Old hash was ***MASKED*** before rotation.";
    assert_eq!(
        content, expected,
        "only the flagged hash span is replaced, surrounding prose is retained exactly: {content:?}"
    );
    let report_debug = format!("{report:?}");
    assert!(
        !report_debug.contains(&hex64),
        "the report itself must not carry the raw detected hash: {report_debug}"
    );
}

/// A credential-shaped PR title (not just body) must also be masked in place
/// rather than causing the whole PR note to be rejected by the runtime's
/// recursive `properties` secret scan.
#[tokio::test]
async fn ingest_masks_credential_shaped_pr_title_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-title-credential-repo"}),
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

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let pr_json = json!([{
        "number": 7,
        "title": format!("Rotate leaked {fake_token} immediately"),
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/rotate",
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    assert!(
        !stored_name.contains(fake_token) && !stored_title.contains(fake_token),
        "raw credential must not survive into name or properties.title: \
         name={stored_name:?}, title={stored_title:?}"
    );
    assert!(
        stored_name.contains("***MASKED***") && stored_title.contains("***MASKED***"),
        "masked marker must be present in both name and properties.title: \
         name={stored_name:?}, title={stored_title:?}"
    );
    let stored_content = item["content"].as_str().expect("content is string");
    assert_eq!(
        stored_content, "",
        "an empty body must remain empty, not acquire a masking placeholder: {stored_content:?}"
    );
}

/// Issue #801: a credential-shaped issue title (sibling site of #763/#785)
/// must be masked in place rather than causing the whole issue note to be
/// rejected by the runtime's recursive `properties` secret scan.
#[tokio::test]
async fn ingest_masks_credential_shaped_issue_title_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-title-credential-repo"}),
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

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let issue_json = json!([{
        "number": 11,
        "title": format!("Rotate leaked {fake_token} immediately"),
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #11")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    assert!(
        !stored_name.contains(fake_token) && !stored_title.contains(fake_token),
        "raw credential must not survive into name or properties.title: \
         name={stored_name:?}, title={stored_title:?}"
    );
    assert!(
        stored_name.contains("***MASKED***") && stored_title.contains("***MASKED***"),
        "masked marker must be present in both name and properties.title: \
         name={stored_name:?}, title={stored_title:?}"
    );
}

/// Issue #977: a legitimate issue whose title mentions a credential-like
/// word (e.g. "key", "auth", "token") while the body separately contains an
/// unrelated full UUID must not be write-blocked by the `uuid-near-trigger`
/// heuristic. `MaskedIssueFields` masks `title` and `body` independently
/// before the note is ever created, so a trigger word in one field cannot
/// "arm" a UUID in the other -- covers the exact cross-field shape reported
/// live against `git.digest`.
#[tokio::test]
async fn ingest_does_not_block_issue_with_credential_word_in_title_and_uuid_in_body() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-977-cross-field-repo"}),
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

    let issue_json = json!([{
        "number": 977,
        "title": "Rotate the api_key configuration",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": "See tracking record 550e8400-e29b-41d4-a716-446655440000 for details."
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #977")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(
        stored_content.contains("550e8400-e29b-41d4-a716-446655440000"),
        "an unrelated UUID with no trigger word nearby in its own field must \
         survive verbatim, not be masked away: {stored_content:?}"
    );
}

/// Issue #977 sibling case: the credential-like word and the UUID appear
/// together in the SAME field (the body). `mask_secrets` -- not the hard
/// `check()` gate -- is what `ingest_issues` applies to `body`, so the
/// ambiguous span is redacted in place and the issue still lands, rather
/// than the whole note being dropped with "reword the source" advice that
/// makes no sense for content the ingester does not own.
#[tokio::test]
async fn ingest_masks_credential_word_and_uuid_co_occurring_in_issue_body() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-977-same-field-repo"}),
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

    let issue_json = json!([{
        "number": 978,
        "title": "Investigate flaky ingest",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": "We rotated the auth key, tracking record 550e8400-e29b-41d4-a716-446655440000 for details."
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #978")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !stored_content.contains("550e8400-e29b-41d4-a716-446655440000"),
        "the ambiguous UUID-near-trigger span must be masked, not stored raw: {stored_content:?}"
    );
    assert!(
        stored_content.contains("***MASKED***"),
        "masked marker must be present in place of the ambiguous span: {stored_content:?}"
    );
}

/// A clean (non-credential) issue title and an empty body must pass through
/// `ingest_issues` unchanged -- guards against a future over-aggressive
/// masking regression that the detector-positive test above cannot catch.
#[tokio::test]
async fn ingest_leaves_clean_issue_title_unmasked() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-clean-title-repo"}),
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

    let issue_json = json!([{
        "number": 12,
        "title": "Fix the flaky retry test",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(report.issues_ingested, 1, "{report:?}");

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(
        item["name"].as_str().unwrap(),
        "#12 Fix the flaky retry test"
    );
    assert_eq!(
        item["properties"]["title"].as_str().unwrap(),
        "Fix the flaky retry test"
    );
}

/// PR #835: the runtime secret gate recursively scans
/// every string in `properties` (not just `title`), so a credential-shaped
/// label name previously tripped the gate on `create()` even after the
/// title was masked, silently dropping the whole issue note.
#[tokio::test]
async fn ingest_masks_credential_shaped_issue_label_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-label-credential-repo"}),
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

    let fake_token = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let issue_json = json!([{
        "number": 13,
        "title": "Rotate the leaked deploy key",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [{"name": fake_token}],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #13")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_labels = item["properties"]["labels"]
        .as_array()
        .expect("properties.labels is array");
    assert_eq!(stored_labels.len(), 1);
    let stored_label = stored_labels[0].as_str().expect("label is string");
    assert!(
        !stored_label.contains(fake_token),
        "raw credential must not survive into properties.labels: {stored_label:?}"
    );
    assert!(
        stored_label.contains("***MASKED***"),
        "masked marker must be present in properties.labels: {stored_label:?}"
    );
}

/// Same finding as above, for the author login field: a credential-shaped
/// login previously tripped the recursive secret gate on `properties` and
/// silently dropped the issue note.
#[tokio::test]
async fn ingest_masks_credential_shaped_issue_author_login_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-login-credential-repo"}),
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

    let fake_token = "ghp_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
    let issue_json = json!([{
        "number": 14,
        "title": "Investigate flaky CI runner",
        "author": {"login": fake_token},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "the issue must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("issue #14")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_author = item["properties"]["author"]
        .as_str()
        .expect("properties.author is string");
    assert!(
        !stored_author.contains(fake_token),
        "raw credential must not survive into properties.author: {stored_author:?}"
    );
    assert!(
        stored_author.contains("***MASKED***"),
        "masked marker must be present in properties.author: {stored_author:?}"
    );
}

/// PR #835: `created_at`/`closed_at` bypassed
/// `MaskedIssueFields` entirely and entered `properties` as arbitrary raw
/// strings, so a credential-shaped `createdAt` tripped the runtime's
/// recursive secret-gate scan on `properties` and silently dropped the
/// whole issue. The fix parses every issue timestamp into a canonical
/// RFC3339 form (or rejects it) before it ever reaches `properties` -- a
/// credential-shaped value is not a valid timestamp, so it is rejected
/// (becomes `null`) rather than persisted raw, and the issue itself must
/// still be ingested.
#[tokio::test]
async fn ingest_rejects_credential_shaped_issue_created_at_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-created-at-credential-repo"}),
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

    let fake_token = "ghp_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";
    let issue_json = json!([{
        "number": 20,
        "title": "Investigate credential-shaped createdAt",
        "author": {"login": "octocat"},
        "createdAt": fake_token,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "an unparseable createdAt must not drop the issue: {report:?}"
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(
        item["properties"]["created_at"].is_null(),
        "an unparseable createdAt must be rejected, not persisted raw: {:?}",
        item["properties"]["created_at"]
    );
    let dumped = item.to_string();
    assert!(
        !dumped.contains(fake_token),
        "raw credential-shaped createdAt must not survive anywhere in the stored record: {dumped}"
    );
}

/// Sibling of the `createdAt` regression above, for `closedAt` (ingest.rs
/// line ~1550).
#[tokio::test]
async fn ingest_rejects_credential_shaped_issue_closed_at_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-closed-at-credential-repo"}),
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

    let fake_token = "ghp_FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    let issue_json = json!([{
        "number": 21,
        "title": "Investigate credential-shaped closedAt",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": fake_token,
        "updatedAt": "2026-01-01T00:00:00Z",
        "labels": [],
        "stateReason": "",
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 1,
        "an unparseable closedAt must not drop the issue: {report:?}"
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert!(
        item["properties"]["closed_at"].is_null(),
        "an unparseable closedAt must be rejected, not persisted raw: {:?}",
        item["properties"]["closed_at"]
    );
    let dumped = item.to_string();
    assert!(
        !dumped.contains(fake_token),
        "raw credential-shaped closedAt must not survive anywhere in the stored record: {dumped}"
    );
}

/// `updatedAt` is not stored in `properties` at all -- it is only used to
/// advance the paging cursor (ingest.rs line ~1632).
/// A credential-shaped `updatedAt` must still not drop the issue, and the
/// cursor persisted to `git_mirror_cursor` must never contain the raw
/// value; it must advance only from a sibling record's validated,
/// canonicalized timestamp.
#[tokio::test]
async fn ingest_rejects_credential_shaped_issue_updated_at_and_cursor_never_persists_raw_value() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-updated-at-credential-repo"}),
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

    let fake_token = "ghp_EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";
    let issue_json = json!([
        {
            "number": 22,
            "title": "Credential-shaped updatedAt",
            "author": {"login": "octocat"},
            "createdAt": "2026-01-01T00:00:00Z",
            "closedAt": null,
            "updatedAt": fake_token,
            "labels": [],
            "stateReason": "",
            "body": ""
        },
        {
            "number": 23,
            "title": "Clean sibling issue",
            "author": {"login": "octocat"},
            "createdAt": "2026-01-02T00:00:00Z",
            "closedAt": null,
            "updatedAt": "2026-01-02T00:00:00Z",
            "labels": [],
            "stateReason": "",
            "body": ""
        }
    ])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, 2,
        "an unparseable updatedAt must not drop the issue: {report:?}"
    );

    let cursor = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written from the sibling issue's valid updatedAt");
    assert!(
        !cursor.contains(fake_token),
        "the paging cursor must never persist a raw credential-shaped updatedAt: {cursor:?}"
    );
    assert!(
        cursor.starts_with("2026-01-02"),
        "the cursor must advance to the sibling issue's valid, canonicalized updatedAt: {cursor:?}"
    );
}

/// Multiple credential spans across both the title and the body of the same
/// PR must all be masked, and exactly one PR note must be written.
#[tokio::test]
async fn ingest_masks_multiple_credential_spans_in_pr_title_and_body() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-multi-span-repo"}),
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

    let title_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let body_token = "ghp_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    let pr_json = json!([{
        "number": 13,
        "title": format!("Purge {title_token} from history"),
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/purge",
        "mergeCommit": null,
        "body": format!("Also found {body_token} committed earlier, and again {body_token} in a second commit.")
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "exactly one PR note written: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1, "no duplicate PR notes: {items:?}");
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    let stored_content = item["content"].as_str().expect("content is string");
    assert!(
        !stored_name.contains(title_token)
            && !stored_title.contains(title_token)
            && !stored_content.contains(body_token),
        "no raw credential span may survive in any surface: \
         name={stored_name:?}, title={stored_title:?}, content={stored_content:?}"
    );
    assert_eq!(
        stored_content.matches("***MASKED***").count(),
        2,
        "both body occurrences of the credential must be masked independently: {stored_content:?}"
    );
    assert!(
        stored_title.contains("***MASKED***"),
        "title span must be masked: {stored_title:?}"
    );
}

/// Issue #763 regression: a clean (non-credential) PR title and a
/// null body must pass through `ingest_prs` byte-for-byte unchanged — a bare
/// 64-hex string with no trigger word nearby is allowlisted by
/// `mask_secrets`, so nothing in this fixture should ever be replaced.
/// Guards against a future over-aggressive masking regression that the
/// detector-positive tests above cannot catch, since they never exercise
/// clean input.
#[tokio::test]
async fn ingest_leaves_clean_pr_title_and_null_body_unmasked() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-clean-title-repo"}),
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

    // A bare 64-hex string (git-SHA/checksum shape) with no trigger word
    // anywhere in the title is allowlisted by `mask_secrets` (`!near_trigger
    // && is_pure_hex`) — this title must survive unchanged.
    let hex64 = "deadbeef".repeat(8);
    let title = format!("Document the {hex64} commit reference");
    let pr_json = json!([{
        "number": 9,
        "title": title,
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "docs/reference",
        "mergeCommit": null,
        "body": null
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(report.prs_ingested, 1, "the PR must land: {report:?}");
    assert_eq!(
        report.prs_skipped_existing, 0,
        "a first-seen PR is never counted as skipped-existing: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");
    let stored_content = item["content"].as_str().expect("content is string");

    assert_eq!(
        stored_title, title,
        "a clean title must be stored byte-for-byte unchanged"
    );
    assert_eq!(
        stored_name,
        format!("#9 {title}"),
        "the name must be the unmodified `#<number> <title>` form"
    );
    assert_eq!(
        stored_content, "",
        "a null body must serialize as an empty, unmasked content string"
    );
    assert!(
        !stored_title.contains("***MASKED***") && !stored_name.contains("***MASKED***"),
        "no masking marker may appear on clean input: title={stored_title:?}, name={stored_name:?}"
    );

    let project_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": project_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let pr_neighbor_count = project_neighbors
        .as_array()
        .expect("array")
        .iter()
        .filter(|h| h["kind"] == "pull_request")
        .count();
    assert_eq!(
        pr_neighbor_count, 1,
        "the PR's project annotation edge must still materialize for clean input: {project_neighbors:?}"
    );
}

/// Diagnostic #763 order-of-operations invariant: masking must run BEFORE
/// `NAME_MAX_CHARS` truncation, not after. The raw (unmasked) title is
/// constructed long enough that the `#<number> <title>` name exceeds the
/// 120-char budget partway through the credential token, but the masked
/// title (marker shrinks the token from 38 to 12 chars) comfortably fits —
/// a trailing marker placed after the token only survives in the stored
/// name if masking ran first.
#[tokio::test]
async fn ingest_masks_pr_title_before_truncating_name_to_max_chars() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-mask-before-truncate-repo"}),
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

    let filler = "x".repeat(80);
    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    // The token must be its own whitespace-delimited unit (a leading space
    // separates it from `filler`) so the vendor-prefix detector sees a token
    // that actually starts with `ghp_`, not a merged `filler+token` blob.
    let title = format!("{filler} {fake_token} TAIL_SURVIVED_MARKER");
    // Sanity-check the fixture math this test depends on: the raw name (with
    // the full token) must exceed NAME_MAX_CHARS (120) before the tail
    // marker even starts, while the masked name comfortably fits under it.
    let raw_name_len = format!("#1 {title}").len();
    assert!(
        raw_name_len > 120,
        "fixture must exceed NAME_MAX_CHARS pre-mask: {raw_name_len}"
    );

    let pr_json = json!([{
        "number": 1,
        "title": title,
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/order",
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_name = item["name"].as_str().expect("name is string");
    let stored_title = item["properties"]["title"]
        .as_str()
        .expect("properties.title is string");

    assert!(
        stored_name.chars().count() <= 120,
        "the stored name must respect NAME_MAX_CHARS: {stored_name:?}"
    );
    assert!(
        !stored_name.contains(fake_token) && !stored_title.contains(fake_token),
        "raw credential must not survive: name={stored_name:?}, title={stored_title:?}"
    );
    assert!(
        stored_name.contains("***MASKED***"),
        "masked marker must be present in the truncated name: {stored_name:?}"
    );
    assert!(
        stored_name.contains("TAIL_SURVIVED_MARKER"),
        "masking must run before truncation so the trailing marker, which would be \
         cut away if the raw (unmasked) name were truncated first, survives: {stored_name:?}"
    );
}

/// Diagnostic #763 interaction check: a masked credential span and a
/// `Fixes #N` cross-reference in the same PR body must not interfere with
/// each other — masking must not corrupt the reference token, and the
/// post-ingest reference-extraction sweep (which runs over the
/// already-masked stored text) must still resolve the reference.
#[tokio::test]
async fn ingest_masks_pr_body_credential_without_breaking_fixes_reference() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-mask-and-reference-repo"}),
    )
    .await;
    let issue_id = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 42, "title": "Some bug", "project_id": project_id.to_string()},
            "annotates": [project_id.to_string()],
        }),
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

    let fake_token = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let pr_json = json!([{
        "number": 7,
        "title": "Rotate the leaked credential",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/rotate",
        "mergeCommit": null,
        "body": format!("Rotated {fake_token} out of the config. Fixes #42.")
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert_eq!(
        report.reference_edges_created, 1,
        "the Fixes #42 reference must resolve even though the body was masked: {report:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_str().expect("content is string");
    assert!(
        !content.contains(fake_token),
        "raw credential must not survive: {content:?}"
    );
    assert!(
        content.contains("***MASKED***"),
        "masked marker must be present: {content:?}"
    );
    assert!(
        content.contains("Fixes #42"),
        "the reference text itself must be retained in stored content: {content:?}"
    );

    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = issue_neighbors.as_array().expect("array");
    assert_eq!(
        hits.len(),
        1,
        "exactly one PR annotates the referenced issue: {hits:?}"
    );
    assert_eq!(hits[0]["kind"], "pull_request");
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
async fn issue_ingest_never_echoes_credential_shaped_state_reason() {
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

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
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

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues ok");
    let items = issues_list.as_array().expect("array");
    assert!(
        items.is_empty(),
        "the ungoverned record must never land: {items:?}"
    );
}

#[tokio::test]
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

// ── project-scoped idempotency ──────────────────────────────────────────

/// GitHub issue/PR numbers are repository-scoped: two different `project`
/// entities can each have a `#1`. Both a direct `find_by_number` collision
/// and the commit ingester's squash-merge-suffix PR fallback must resolve
/// within the ingesting project only, never across projects.
#[tokio::test]
async fn issue_and_pr_idempotency_is_scoped_per_project() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_a = create(&registry, json!({"kind": "project", "name": "repo-a"})).await;
    let project_b = create(&registry, json!({"kind": "project", "name": "repo-b"})).await;

    // Project A already has issue #1 and PR #1 (as if ingested in a prior pass).
    let pr_a = create(
        &registry,
        json!({
            "kind": "pull_request",
            "content": "",
            "properties": {"number": 1, "title": "A#1", "project_id": project_a.to_string()},
            "annotates": [project_a.to_string()],
        }),
    )
    .await;
    let issue_a = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 1, "title": "issue A#1", "project_id": project_a.to_string()},
            "annotates": [project_a.to_string()],
        }),
    )
    .await;

    // Project B's fixture repo has its own squash-merge commit suffixed
    // "(#1)" -- before project B has any PR #1 of its own, the fallback must
    // NOT resolve to project A's PR #1.
    let dir = tempfile::tempdir().expect("tempdir");
    let repo_b: PathBuf = dir.path().to_path_buf();
    init_repo(&repo_b);
    write(&repo_b, "README.md", "b\n");
    commit(&repo_b, &["README.md"], "Add repo-b feature (#1)");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo_b.clone(), project_b.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");
    assert_eq!(report.commits_ingested, 1);

    let pr_a_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_a.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        pr_a_neighbors.as_array().expect("array").len(),
        0,
        "project B's commit must never attach to project A's PR #1: {pr_a_neighbors:?}"
    );

    // Directly create project B's own issue #1 and PR #1 (both numbered the
    // same as project A's), then ingest a second squash-merge commit -- the
    // fallback must now resolve within project B.
    let pr_b = create(
        &registry,
        json!({
            "kind": "pull_request",
            "content": "",
            "properties": {"number": 1, "title": "B#1", "project_id": project_b.to_string()},
            "annotates": [project_b.to_string()],
        }),
    )
    .await;
    let issue_b = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 1, "title": "issue B#1", "project_id": project_b.to_string()},
            "annotates": [project_b.to_string()],
        }),
    )
    .await;
    assert_ne!(pr_a, pr_b, "both projects' #1 PRs are distinct records");
    assert_ne!(
        issue_a, issue_b,
        "both projects' #1 issues are distinct records"
    );

    write(&repo_b, "src/lib.rs", "// b2\n");
    commit(&repo_b, &["src/lib.rs"], "Add another repo-b feature (#1)");

    run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo_b.clone(), project_b.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    let pr_b_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_b.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        pr_b_neighbors.as_array().expect("array").len(),
        1,
        "project B's own PR #1 resolves the squash-merge fallback: {pr_b_neighbors:?}"
    );
    let pr_a_neighbors_after = registry
        .dispatch(
            "neighbors",
            json!({"id": pr_a.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    assert_eq!(
        pr_a_neighbors_after.as_array().expect("array").len(),
        0,
        "project A's PR #1 remains untouched: {pr_a_neighbors_after:?}"
    );
}

// ── gh boundary contract + per-record warning aggregation ───────────────────

/// End-to-end over a PATH-shadowing fake `gh`: locks the four demo-found
/// regression classes ((a) no `-C`, correct cwd; (b) empty `stateReason`
/// omitted; (c) uppercase enum values lowercased; (d) all four governed
/// values accepted), asserts `pull_request` properties use `base_ref`/
/// `head_ref` (not `base`/`head`), and asserts one ungoverned-`state_reason`
/// issue between two valid ones aborts only its own record (one warning,
/// both neighbors still land) rather than the whole ingest pass.
#[tokio::test]
async fn gh_boundary_contract_and_partial_ingest_failure() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "gh-boundary-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo: PathBuf = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("mk repo dir");
    init_repo(&repo);
    write(&repo, "README.md", "hello\n");
    commit(&repo, &["README.md"], "Initial commit");
    let repo_canon = repo.canonicalize().expect("canonicalize repo");

    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("mk bin dir");
    let log_dir = dir.path().join("log");
    std::fs::create_dir_all(&log_dir).expect("mk log dir");

    let pr_json = json!([{
        "number": 99,
        "title": "Add feature",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": "2026-01-02T00:00:00Z",
        "closedAt": "2026-01-02T00:00:00Z",
        "updatedAt": "2026-01-02T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "feature/x",
        "mergeCommit": null,
        "body": "PR body"
    }])
    .to_string();

    // 6 issues, ordered good, good, BAD, good, good, good -- #3's ungoverned
    // `stateReason` must warn-and-skip without aborting #4/#5/#6, and the two
    // good records sandwiching it (#2, #4) must both land.
    let issue_json = json!([
        {"number": 1, "title": "i1", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-01T00:00:01Z", "labels": [], "stateReason": "", "body": ""},
        {"number": 2, "title": "i2", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:02Z", "updatedAt": "2026-01-01T00:00:02Z", "labels": [], "stateReason": "NOT_PLANNED", "body": ""},
        {"number": 3, "title": "i3-bad", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:03Z", "updatedAt": "2026-01-01T00:00:03Z", "labels": [], "stateReason": "WONTFIX", "body": ""},
        {"number": 4, "title": "i4", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:04Z", "updatedAt": "2026-01-01T00:00:04Z", "labels": [], "stateReason": "COMPLETED", "body": ""},
        {"number": 5, "title": "i5", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:05Z", "updatedAt": "2026-01-01T00:00:05Z", "labels": [], "stateReason": "REOPENED", "body": ""},
        {"number": 6, "title": "i6", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": "2026-01-01T00:00:06Z", "updatedAt": "2026-01-01T00:00:06Z", "labels": [], "stateReason": "DUPLICATE", "body": ""}
    ])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");

    assert!(
        report.gh_available,
        "fake gh must be found on PATH: {report:?}"
    );
    assert_eq!(report.prs_ingested, 1, "{report:?}");
    assert_eq!(
        report.issues_ingested, 5,
        "5 of 6 issues land, #3 warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("issue #3"))
            .count(),
        1,
        "exactly one warning names the ungoverned record: {:?}",
        report.warnings
    );

    // (a) gh is never invoked with -C, and always runs with the repo as cwd.
    let args_log = std::fs::read_to_string(log_dir.join("args.log")).expect("read args.log");
    assert!(
        !args_log.contains("-C"),
        "gh must never receive an unsupported -C flag: {args_log}"
    );
    // ADR-088 Amendment 1: the paging rewrite (--search
    // "sort:updated-asc ...") must not silently drop --state all -- gh
    // defaults to open-only listing, which would make closed issues and
    // closed/merged PRs vanish from every ingest.
    let pr_and_issue_invocations: Vec<&str> = args_log
        .lines()
        .filter(|l| l.starts_with("pr ") || l.starts_with("issue "))
        .collect();
    assert!(
        !pr_and_issue_invocations.is_empty(),
        "expected at least one gh pr/issue list invocation: {args_log}"
    );
    for line in &pr_and_issue_invocations {
        assert!(
            line.contains("--state all"),
            "every gh pr/issue list invocation must request --state all: {line:?}"
        );
    }
    let cwd_log = std::fs::read_to_string(log_dir.join("cwd.log")).expect("read cwd.log");
    let cwd_lines: Vec<&str> = cwd_log.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        !cwd_lines.is_empty(),
        "gh must have been invoked at least once"
    );
    for line in &cwd_lines {
        let logged = Path::new(line)
            .canonicalize()
            .expect("canonicalize logged cwd");
        assert_eq!(
            logged, repo_canon,
            "every gh invocation must run with the repo as its current_dir"
        );
    }

    // (b)/(c)/(d): pull_request properties use base_ref/head_ref.
    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 20}))
        .await
        .expect("list issues ok");
    let issue_items = issues_list.as_array().expect("array");
    let issue_by_number = |n: u64| -> &Value {
        issue_items
            .iter()
            .find(|i| i["properties"]["number"].as_u64() == Some(n))
            .unwrap_or_else(|| panic!("issue #{n} must be stored: {issue_items:?}"))
    };
    assert!(
        issue_by_number(1)["properties"]
            .get("state_reason")
            .is_none(),
        "empty stateReason must be omitted, not stored as \"\""
    );
    assert_eq!(
        issue_by_number(2)["properties"]["state_reason"],
        "not_planned"
    );
    assert_eq!(
        issue_by_number(4)["properties"]["state_reason"],
        "completed"
    );
    assert_eq!(issue_by_number(5)["properties"]["state_reason"], "reopened");
    assert_eq!(
        issue_by_number(6)["properties"]["state_reason"],
        "duplicate"
    );
    assert!(
        !issue_items
            .iter()
            .any(|i| i["properties"]["number"].as_u64() == Some(3)),
        "the ungoverned record must never land: {issue_items:?}"
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let pr_items = prs_list.as_array().expect("array");
    let pr99 = pr_items
        .iter()
        .find(|i| i["properties"]["number"].as_u64() == Some(99))
        .expect("PR #99 must be stored");
    assert_eq!(pr99["properties"]["base_ref"], "main");
    assert_eq!(pr99["properties"]["head_ref"], "feature/x");
    assert!(
        pr99["properties"].get("base").is_none() && pr99["properties"].get("head").is_none(),
        "ADR-088 names these base_ref/head_ref, not base/head: {pr99:?}"
    );

    // Second pass: the frozen cursor retries #3 (fails again) without
    // re-creating any already-landed record.
    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");
    assert_eq!(
        report2.issues_ingested, 0,
        "already-landed issues are found by natural key, not re-created: {report2:?}"
    );
    assert_eq!(
        report2
            .warnings
            .iter()
            .filter(|w| w.contains("issue #3"))
            .count(),
        1,
        "the frozen cursor retries the failed record every pass, not just once: {:?}",
        report2.warnings
    );
}

// ── raw `updatedAt` must never reach the
//    paging floor via a full page ─────────────────────────────────────────

/// Before this fix, `ingest_issues` sorted the raw `Vec<GhIssue>` and derived
/// its paging continuation floor (`last_updated_at`) from the raw,
/// pre-`MaskedIssueFields` `updated_at` before `MaskedIssueFields::new` ever
/// ran. A credential-shaped raw value sorts LAST under raw ASCII string
/// comparison (any letter-leading string outranks a digit-leading RFC3339
/// timestamp), so it could become the next `gh --search updated:>=...`
/// argument -- exposing it in process arguments -- or, once dropped by
/// canonicalization, corrupt the frozen-cursor invariant.
///
/// This test forces the vulnerable branch directly: a page of exactly
/// `PAGE_LIMIT` (1000) issues, one of which has a credential-shaped raw
/// `updatedAt` that fails RFC3339 parsing. Under the fix, the ENTIRE page is
/// masked/canonicalized before sorting, so the malformed record's
/// `updated_at` becomes `None` and sorts FIRST, never becoming
/// `last_updated_at`. Asserts the raw value never appears in any recorded
/// `gh` invocation's argv, in the persisted paging cursor, or in any
/// persisted issue record, and that a full page still triggers a
/// continuation fetch (not a false `WindowComplete`) -- pagination remains
/// resumable.
#[tokio::test]
async fn issue_full_page_never_leaks_raw_updated_at_into_paging_floor() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "full-page-repo"}),
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

    // Mirrors `ingest.rs`'s private `PAGE_LIMIT` -- `gh {pr,issue} list
    // --search` never returns more than this many results per page.
    const PAGE_LIMIT: usize = 1000;
    const CREDENTIAL: &str = "sk-ant-api03-FAKE1234567890FAKE1234567890FAKE1234567890FAKE";

    let mut issues: Vec<Value> = (1..PAGE_LIMIT)
        .map(|i| {
            let minute = i / 60;
            let second = i % 60;
            json!({
                "number": i,
                "title": format!("issue {i}"),
                "author": {"login": "a"},
                "createdAt": "2026-01-01T00:00:00Z",
                "closedAt": null,
                "updatedAt": format!("2026-01-01T{minute:02}:{second:02}:00Z"),
                "labels": [],
                "stateReason": "",
                "body": ""
            })
        })
        .collect();
    // Raw ASCII compare: 's' (0x73) outranks every digit (0x30-0x39), so
    // this record's raw `updatedAt` sorts after all 999 valid timestamps
    // above -- exactly the "sorts last" shape the finding describes.
    issues.push(json!({
        "number": PAGE_LIMIT,
        "title": "issue with malformed updatedAt",
        "author": {"login": "a"},
        "createdAt": "2026-01-01T00:00:00Z",
        "closedAt": null,
        "updatedAt": CREDENTIAL,
        "labels": [],
        "stateReason": "",
        "body": ""
    }));
    assert_eq!(
        issues.len(),
        PAGE_LIMIT,
        "page must be exactly PAGE_LIMIT-sized to force the continuation branch"
    );
    let issue_json = Value::Array(issues).to_string();

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json);
    let _path_guard = PathGuard::install(&bin_dir);

    let mut opts = IngestOptions::unbounded(repo.clone(), project_id.to_string());
    opts.include.pull_requests = false;
    opts.include.commits = false;

    let report = run_ingest(&rt, &token, &registry, opts)
        .await
        .expect("ingest ok");

    assert_eq!(
        report.issues_ingested, PAGE_LIMIT as u64,
        "every record lands -- the malformed record only loses its updated_at field: {report:?}"
    );

    let args_log = std::fs::read_to_string(log_dir.join("args.log")).expect("read args.log");
    assert!(
        !args_log.contains(CREDENTIAL),
        "the raw credential-shaped updatedAt must never reach a gh invocation's argv \
         (paging floor leak): {args_log}"
    );
    // A full (PAGE_LIMIT-sized) page must trigger a second `gh issue list`
    // invocation -- proving pagination did not silently treat the full page
    // as window-complete.
    let issue_invocations = args_log.lines().filter(|l| l.starts_with("issue ")).count();
    assert!(
        issue_invocations >= 2,
        "a full page must be followed by a continuation fetch: {args_log}"
    );

    let cursor = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written");
    assert!(
        !cursor.contains(CREDENTIAL),
        "the persisted paging cursor must never contain the raw credential value: {cursor}"
    );

    // `list` clamps over-cap requests and returns `{items, effective_limit, ..}`
    // instead of a bare array (#894), so scan every persisted issue by paging.
    let mut scanned = 0usize;
    let mut offset = 0u64;
    loop {
        let page = registry
            .dispatch(
                "list",
                json!({"kind": "issue", "limit": (PAGE_LIMIT + 10) as u64, "offset": offset}),
            )
            .await
            .expect("list issues ok");
        let items = match page.as_array() {
            Some(items) => items.clone(),
            None => page
                .get("items")
                .and_then(Value::as_array)
                .expect("clamped list response must carry an items array")
                .clone(),
        };
        assert!(
            items.iter().all(|i| !i.to_string().contains(CREDENTIAL)),
            "no persisted issue record may contain the raw credential value"
        );
        scanned += items.len();
        if items.is_empty() {
            break;
        }
        offset += items.len() as u64;
    }
    assert!(
        scanned >= PAGE_LIMIT,
        "paging scan must cover every persisted issue: scanned {scanned}"
    );
}

// ── unsorted `gh` output must not desync the frozen-cursor retry guarantee ──

/// Reads the git-ingest cursor row directly, mirroring `ingest.rs`'s private
/// `read_cursor` — the acceptance tests otherwise only observe cursor state
/// indirectly through `IngestReport` deltas across passes, which is not
/// precise enough to assert the exact freeze point below.
async fn read_git_cursor(rt: &KhiveRuntime, project_id: Uuid, kind: &str) -> Option<String> {
    use khive_storage::types::{SqlStatement, SqlValue};
    let sql = rt.sql();
    let mut r = sql.reader().await.expect("sql reader");
    let row = r
        .query_row(SqlStatement {
            sql: "SELECT cursor_value FROM git_mirror_cursor WHERE project_id=?1 AND kind=?2"
                .into(),
            params: vec![
                SqlValue::Text(project_id.to_string()),
                SqlValue::Text(kind.to_string()),
            ],
            label: Some("test_read_git_cursor".into()),
        })
        .await
        .expect("query cursor row");
    row.and_then(|r| match r.get("cursor_value") {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    })
}

/// `gh issue list` / `gh pr list` make no ordering guarantee: the
/// frozen-cursor retry contract only holds if records are
/// walked in nondecreasing `updatedAt` order, so unsorted `gh` output can let
/// a later, newer record's timestamp overwrite the freeze point before an
/// earlier, older record fails — after which the older failure looks
/// already-covered by the cursor and is skipped forever instead of retried.
///
/// This fixture returns three issues out of raw order — `[#10 (newest,
/// good), #20 (older than #10, BAD: ungoverned stateReason), #5 (oldest,
/// good)]` — so an unsorted walk would create #10 before #20 fails, wrongly
/// freezing the cursor at #10's timestamp (newer than #20's). Sorting
/// ascending first (the fix) walks `#5, #20, #10`, so the freeze lands at
/// #5's timestamp instead — at/below #20's — and pass 2 (after #20's
/// `stateReason` is corrected upstream) retries and lands it without
/// duplicating #5 or #10.
#[tokio::test]
async fn issue_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-order-repo"}),
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

    let bad_issue_json = |state_reason: &str| {
        json!([
            {"number": 10, "title": "i10-newest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "labels": [], "stateReason": "completed", "body": ""},
            {"number": 20, "title": "i20-older-bad", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "labels": [], "stateReason": state_reason, "body": ""},
            {"number": 5, "title": "i5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "labels": [], "stateReason": "", "body": ""}
        ])
        .to_string()
    };

    write_fake_gh(&bin_dir, &log_dir, "[]", &bad_issue_json("WONTFIX"));
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");

    assert_eq!(
        report.issues_ingested, 2,
        "#5 and #10 both land, #20 warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("issue #20"))
            .count(),
        1,
        "exactly one warning names the ungoverned record: {:?}",
        report.warnings
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, "2025-12-01T00:00:00Z",
        "cursor must freeze at #5's timestamp (at/below failed #20's \
         2026-01-01T00:00:00Z), not advance to #10's later 2026-01-03T00:00:00Z \
         just because #10 appeared earlier in gh's raw (unsorted) output: {cursor_after_pass1:?}"
    );

    // Upstream correction: #20's stateReason is fixed to a governed value —
    // pass 2 must retry and land it without duplicating #5 or #10.
    std::fs::write(
        log_dir.join("issue_response.json"),
        bad_issue_json("completed"),
    )
    .expect("rewrite issue fixture with corrected stateReason");

    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    assert_eq!(
        report2.issues_ingested, 1,
        "only #20 (now corrected) is newly created on pass 2: {report2:?}"
    );
    assert_eq!(
        report2.issues_skipped_existing, 2,
        "#5 and #10 are found by natural key, not duplicated: {report2:?}"
    );
    assert!(
        report2.warnings.iter().all(|w| !w.contains("issue #20")),
        "#20 must not warn once its stateReason is corrected: {:?}",
        report2.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 20}))
        .await
        .expect("list issues ok");
    let numbers: Vec<u64> = issues_list
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|i| i["properties"]["number"].as_u64())
        .collect();
    assert_eq!(
        numbers.len(),
        3,
        "exactly #5, #10, #20 — no duplicates: {numbers:?}"
    );

    let cursor_after_pass2 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written");
    assert_eq!(cursor_after_pass2, "2026-01-03T00:00:00Z");
}

/// PR mirror of `issue_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing`.
/// `pull_request` has no `stateReason` field in `gh pr list --json` (verified
/// against the live `gh` CLI's field list — only `issue`s carry one), so this
/// fixture forces the per-record failure a different, equally real way. Prior
/// to #763 this used a leaked-credential-shaped PR title, but #763 now masks
/// credential-shaped title/body in place instead of dropping the record, so
/// that mechanism would land #20 instead of failing it. Per #763's approved
/// design (`architect-2/approved_design.md` §3, "Required tests"), the
/// replacement is a deterministic, secret-detection-independent failure: a
/// test-only embedder ([`FailOnceEmbeddingService`]) that fails exactly once
/// for a PR body containing [`CURSOR_FAIL_SENTINEL`].
#[tokio::test]
async fn pr_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-order-repo"}),
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

    let bad_pr_json = json!([
        {"number": 10, "title": "pr10-newest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2026-01-03T00:00:00Z", "baseRefName": "main", "headRefName": "f10", "mergeCommit": null, "body": ""},
        {"number": 20, "title": "pr20-older-bad", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2026-01-01T00:00:00Z", "baseRefName": "main", "headRefName": "f20", "mergeCommit": null, "body": CURSOR_FAIL_SENTINEL},
        {"number": 5, "title": "pr5-oldest-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": "2025-12-01T00:00:00Z", "baseRefName": "main", "headRefName": "f5", "mergeCommit": null, "body": ""}
    ])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &bad_pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");

    assert_eq!(
        report.prs_ingested, 2,
        "#5 and #10 both land, #20 (injected embedder failure) warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("pull_request #20"))
            .count(),
        1,
        "exactly one warning names the rejected record: {:?}",
        report.warnings
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, "2025-12-01T00:00:00Z",
        "cursor must freeze at #5's timestamp (at/below failed #20's \
         2026-01-01T00:00:00Z), not advance to #10's later 2026-01-03T00:00:00Z \
         just because #10 appeared earlier in gh's raw (unsorted) output: {cursor_after_pass1:?}"
    );

    // The embedder's one-shot fuse is already spent — pass 2 retries #20 with
    // the SAME fixture (no upstream correction needed) and must land it
    // without duplicating #5 or #10.
    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    assert_eq!(
        report2.prs_ingested, 1,
        "only #20 (embedder fuse now spent) is newly created on pass 2: {report2:?}"
    );
    assert_eq!(
        report2.prs_skipped_existing, 2,
        "#5 and #10 are found by natural key, not duplicated: {report2:?}"
    );
    assert!(
        report2
            .warnings
            .iter()
            .all(|w| !w.contains("pull_request #20")),
        "#20 must not warn once the embedder fuse is spent: {:?}",
        report2.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 20}))
        .await
        .expect("list prs ok");
    let numbers: Vec<u64> = prs_list
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|i| i["properties"]["number"].as_u64())
        .collect();
    assert_eq!(
        numbers.len(),
        3,
        "exactly #5, #10, #20 — no duplicates: {numbers:?}"
    );

    let cursor_after_pass2 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written");
    assert_eq!(cursor_after_pass2, "2026-01-03T00:00:00Z");
}

// ── equal-`updated_at` ties must not strand a failed record ─────────────────

/// Sorting alone (the earlier fix) does not cover a TIE: if a successful
/// record and a failing record share the exact same `updated_at`, the
/// success still advances the cursor to that shared timestamp, and an
/// exclusive `updated > cursor` retry check would then see the failed
/// record's `updated_at == cursor` on the next pass and treat it as
/// not-new — stranding it forever even with correct sort order. `is_new` is
/// now inclusive (`updated >= cursor`), so every record AT the cursor
/// timestamp is re-examined every pass until the cursor moves past it; the
/// already-landed one is a cheap no-op via the natural-key lookup.
///
/// Fixture: #5 (good) and #20 (bad — ungoverned `stateReason`) share the
/// identical `updatedAt`, #5 first in gh's raw output so it lands before
/// #20 fails and freezes the cursor at that shared timestamp. Pass 2 (after
/// #20's `stateReason` is corrected) must retry and land #20 without
/// duplicating #5.
#[tokio::test]
async fn issue_ingest_retries_tie_at_cursor_timestamp() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "issue-tie-repo"}),
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

    const TIE_AT: &str = "2026-02-01T00:00:00Z";
    let issue_json = |state_reason: &str| {
        json!([
            {"number": 5, "title": "i5-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": TIE_AT, "labels": [], "stateReason": "", "body": ""},
            {"number": 20, "title": "i20-bad-tied", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "closedAt": null, "updatedAt": TIE_AT, "labels": [], "stateReason": state_reason, "body": ""}
        ])
        .to_string()
    };

    write_fake_gh(&bin_dir, &log_dir, "[]", &issue_json("WONTFIX"));
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");

    assert_eq!(
        report.issues_ingested, 1,
        "#5 lands, #20 (tied timestamp, ungoverned stateReason) warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("issue #20"))
            .count(),
        1,
        "exactly one warning names the ungoverned record: {:?}",
        report.warnings
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "issues")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, TIE_AT,
        "cursor freezes at #5's timestamp, which is the SAME as failed #20's — \
         the exact tie the exclusive `updated > cursor` check used to strand \
         #20 on: {cursor_after_pass1:?}"
    );

    // Upstream correction: #20's stateReason is fixed to a governed value —
    // pass 2 must retry the tied record and land it without duplicating #5.
    std::fs::write(log_dir.join("issue_response.json"), issue_json("completed"))
        .expect("rewrite issue fixture with corrected stateReason");

    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    assert_eq!(
        report2.issues_ingested, 1,
        "only #20 (now corrected) is newly created on pass 2, proving the \
         tied timestamp did not strand it: {report2:?}"
    );
    assert_eq!(
        report2.issues_skipped_existing, 1,
        "#5 is found by natural key, not duplicated, even though it is \
         re-examined every pass at the tied cursor timestamp: {report2:?}"
    );
    assert!(
        report2.warnings.iter().all(|w| !w.contains("issue #20")),
        "#20 must not warn once its stateReason is corrected: {:?}",
        report2.warnings
    );

    let issues_list = registry
        .dispatch("list", json!({"kind": "issue", "limit": 20}))
        .await
        .expect("list issues ok");
    let numbers: Vec<u64> = issues_list
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|i| i["properties"]["number"].as_u64())
        .collect();
    assert_eq!(
        numbers.len(),
        2,
        "exactly #5, #20 — no duplicates: {numbers:?}"
    );
}

/// PR mirror of `issue_ingest_retries_tie_at_cursor_timestamp` — see that
/// test and `ingest_prs`'s sort-rationale comment for the tie hazard. Uses
/// the fail-once-embedder failure mechanism (see
/// `pr_ingest_sorts_by_updated_at_so_frozen_cursor_survives_out_of_order_listing`
/// for why the pre-#763 leaked-credential-in-title mechanism no longer
/// forces a create failure) since `pull_request` has no `stateReason` field.
#[tokio::test]
async fn pr_ingest_retries_tie_at_cursor_timestamp() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;
    rt.register_embedder(FailOnceEmbedderProvider);

    let project_id = create(&registry, json!({"kind": "project", "name": "pr-tie-repo"})).await;

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

    const TIE_AT: &str = "2026-02-01T00:00:00Z";
    let pr_json = json!([
        {"number": 5, "title": "pr5-good", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": TIE_AT, "baseRefName": "main", "headRefName": "f5", "mergeCommit": null, "body": ""},
        {"number": 20, "title": "pr20-bad-tied", "author": {"login": "a"}, "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null, "updatedAt": TIE_AT, "baseRefName": "main", "headRefName": "f20", "mergeCommit": null, "body": CURSOR_FAIL_SENTINEL}
    ])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 1)");

    assert_eq!(
        report.prs_ingested, 1,
        "#5 lands, #20 (tied timestamp, injected embedder failure) warns-and-skips: {report:?}"
    );
    assert_eq!(
        report
            .warnings
            .iter()
            .filter(|w| w.contains("pull_request #20"))
            .count(),
        1,
        "exactly one warning names the rejected record: {:?}",
        report.warnings
    );

    let cursor_after_pass1 = read_git_cursor(&rt, project_id, "prs")
        .await
        .expect("cursor must be written (#5 landed before the stall)");
    assert_eq!(
        cursor_after_pass1, TIE_AT,
        "cursor freezes at #5's timestamp, which is the SAME as failed #20's — \
         the exact tie the exclusive `updated > cursor` check used to strand \
         #20 on: {cursor_after_pass1:?}"
    );

    // The embedder's one-shot fuse is already spent — pass 2 retries the tied
    // record with the SAME fixture and must land it without duplicating #5.
    let report2 = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok (pass 2)");

    assert_eq!(
        report2.prs_ingested, 1,
        "only #20 (embedder fuse now spent) is newly created on pass 2, proving the \
         tied timestamp did not strand it: {report2:?}"
    );
    assert_eq!(
        report2.prs_skipped_existing, 1,
        "#5 is found by natural key, not duplicated, even though it is \
         re-examined every pass at the tied cursor timestamp: {report2:?}"
    );
    assert!(
        report2
            .warnings
            .iter()
            .all(|w| !w.contains("pull_request #20")),
        "#20 must not warn once the embedder fuse is spent: {:?}",
        report2.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 20}))
        .await
        .expect("list prs ok");
    let numbers: Vec<u64> = prs_list
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|i| i["properties"]["number"].as_u64())
        .collect();
    assert_eq!(
        numbers.len(),
        2,
        "exactly #5, #20 — no duplicates: {numbers:?}"
    );
}

// ── `git.digest` verb (ADR-088 Amendment 1) ─────────────────────────────────

/// End-to-end over the `git.digest` verb itself (not `run_ingest` directly):
/// no `project` argument auto-creates the repo-anchor entity (reported via
/// `project_created`), a `Closes #N` commit message materializes an
/// `annotates` edge (with `ref_kind: "closes"`) from the commit to a
/// pre-existing `issue` note, a second commit's `parents[]` materializes a
/// `precedes` edge from parent to child, and both commit and issue notes get
/// the amendment's readable `name`.
#[tokio::test]
async fn digest_verb_auto_creates_project_and_enriches_references() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Initial commit");

    let first = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 1)");

    assert_eq!(first["done"], true, "{first}");
    assert_eq!(first["project_created"], true, "{first}");
    assert_eq!(first["commits_ingested"], 1, "{first}");
    let project_id = first["project_id"]
        .as_str()
        .expect("project_id present")
        .to_string();

    // Second digest call for the same local path resolves the SAME project
    // (no duplicate anchor entity).
    let second = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 2, no new commits)");
    assert_eq!(second["project_created"], false, "{second}");
    assert_eq!(second["project_id"], project_id, "{second}");
    assert_eq!(
        second["commits_ingested"], 0,
        "no new commits since pass 1: {second}"
    );

    // Pre-create an issue #42 the project already tracks (as if ingested by
    // an earlier `gh`-backed pass), then a commit whose message closes it.
    let issue_id = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 42, "title": "Some bug", "project_id": project_id},
            "annotates": [project_id],
        }),
    )
    .await;

    write(repo, "src/lib.rs", "// fix\n");
    commit(repo, &["src/lib.rs"], "Fix the bug\n\nCloses #42");
    let commit2_sha = head_sha(repo);

    let third = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "project": project_id, "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 3)");
    assert_eq!(third["commits_ingested"], 1, "{third}");
    assert_eq!(
        third["reference_edges_created"], 1,
        "the Closes #42 reference resolves: {third}"
    );
    assert_eq!(
        third["parent_edges_created"], 1,
        "the second commit's parent link to the first: {third}"
    );

    // The issue has exactly one incoming `annotates` edge, from the closing
    // commit, carrying ref_kind=closes.
    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = issue_neighbors.as_array().expect("array");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["kind"], "commit");

    // The closing commit's own record carries the readable name and its sha.
    let commit_note = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = commit_note.as_array().expect("array");
    let closing = items
        .iter()
        .find(|c| c["properties"]["sha"] == commit2_sha)
        .expect("closing commit note present");
    let name = closing["name"].as_str().expect("name present");
    assert!(
        name.contains("Fix the bug"),
        "commit name must carry the subject: {name:?}"
    );

    // Parent -> child `precedes` edge: the first commit precedes the second.
    let first_commit = items
        .iter()
        .find(|c| c["properties"]["sha"] != commit2_sha)
        .expect("first commit note present");
    let first_commit_id = first_commit["id"].as_str().expect("id present");
    let precedes_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": first_commit_id, "direction": "outgoing", "relations": ["precedes"]}),
        )
        .await
        .expect("neighbors ok");
    let precedes_hits = precedes_neighbors.as_array().expect("array");
    assert_eq!(
        precedes_hits.len(),
        1,
        "the first commit precedes exactly the second: {precedes_hits:?}"
    );
    assert_eq!(
        precedes_hits[0]["id"].as_str().unwrap(),
        closing["id"].as_str().unwrap()
    );
}

/// `max_items` bounds work per call and the response is cursor-resumable:
/// looping `git.digest` calls until `done` eventually ingests every commit
/// with no duplicates.
#[tokio::test]
async fn digest_verb_max_items_is_bounded_and_resumable() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    for i in 0..3 {
        write(repo, "f.txt", &format!("v{i}\n"));
        commit(repo, &["f.txt"], &format!("commit {i}"));
    }

    let mut total_ingested = 0u64;
    let mut project_id: Option<String> = None;
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls <= 10, "must converge well within 10 calls");
        let mut args =
            json!({"source": repo.to_str().unwrap(), "max_items": 1, "include": ["commits"]});
        if let Some(p) = &project_id {
            args["project"] = json!(p);
        }
        let resp = registry
            .dispatch("git.digest", args)
            .await
            .expect("digest ok");
        project_id = Some(resp["project_id"].as_str().unwrap().to_string());
        total_ingested += resp["commits_ingested"].as_u64().unwrap();
        if resp["done"].as_bool().unwrap() {
            break;
        }
    }
    assert_eq!(total_ingested, 3, "all three commits eventually ingest");

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    assert_eq!(
        list.as_array().expect("array").len(),
        3,
        "no duplicates across the bounded/resumed calls"
    );
}

/// Source validation surfaces as a normal verb error (ssh:// rejected,
/// ADR-088 Amendment 1 security posture) rather than panicking or silently
/// no-op'ing.
#[tokio::test]
async fn digest_verb_rejects_ssh_source() {
    let (_rt, _token, registry) = fixture().await;
    let err = registry
        .dispatch(
            "git.digest",
            json!({"source": "ssh://git@github.com/org/repo.git"}),
        )
        .await
        .expect_err("ssh source must be rejected");
    assert!(format!("{err}").contains("SSH"), "{err}");
}

/// `max_items` boundary handling (ADR-088 Amendment 1):
/// a negative value must clamp to the lower bound (1), NOT silently fall
/// through `as_u64`'s failure into the 500 default. `0` clamps to 1 too;
/// values above 2000 clamp to 2000; a non-integer value is a hard error.
#[tokio::test]
async fn digest_verb_max_items_negative_and_zero_clamp_to_one() {
    let _guard = ENV_MUTEX.lock().await;

    for requested in [-1, 0] {
        let (_rt, _token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_repo(repo);
        for i in 0..3 {
            write(repo, "f.txt", &format!("v{i}\n"));
            commit(repo, &["f.txt"], &format!("commit {i}"));
        }

        let resp = registry
            .dispatch(
                "git.digest",
                json!({"source": repo.to_str().unwrap(), "max_items": requested, "include": ["commits"]}),
            )
            .await
            .unwrap_or_else(|e| panic!("digest ok for max_items={requested}: {e}"));
        assert_eq!(
            resp["commits_ingested"].as_u64().unwrap(),
            1,
            "max_items={requested} must clamp to the lower bound (1 item this call): {resp:?}"
        );
        assert!(
            !resp["done"].as_bool().unwrap(),
            "2 commits remain after a 1-item pass: {resp:?}"
        );
    }
}

#[tokio::test]
async fn digest_verb_max_items_above_cap_clamps_to_two_thousand() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "f.txt", "v\n");
    commit(repo, &["f.txt"], "only commit");

    let resp = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 2001, "include": ["commits"]}),
        )
        .await
        .expect("digest ok");
    assert_eq!(resp["commits_ingested"].as_u64().unwrap(), 1);
    assert!(
        resp["done"].as_bool().unwrap(),
        "a single-commit repo finishes in one call however large max_items clamps to: {resp:?}"
    );
}

#[tokio::test]
async fn digest_verb_max_items_at_boundary_values() {
    let _guard = ENV_MUTEX.lock().await;
    for (requested, expected_ingested) in [(1i64, 1u64), (2000i64, 1u64)] {
        let (_rt, _token, registry) = fixture().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_repo(repo);
        write(repo, "f.txt", "v\n");
        commit(repo, &["f.txt"], "only commit");

        let resp = registry
            .dispatch(
                "git.digest",
                json!({"source": repo.to_str().unwrap(), "max_items": requested, "include": ["commits"]}),
            )
            .await
            .unwrap_or_else(|e| panic!("digest ok for max_items={requested}: {e}"));
        assert_eq!(
            resp["commits_ingested"].as_u64().unwrap(),
            expected_ingested,
            "max_items={requested}: {resp:?}"
        );
    }
}

#[tokio::test]
async fn digest_verb_rejects_non_integer_max_items() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "f.txt", "v\n");
    commit(repo, &["f.txt"], "only commit");

    let err = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": "lots"}),
        )
        .await
        .expect_err("a non-integer max_items must be a hard error, not a silent default");
    assert!(
        format!("{err}").contains("max_items"),
        "error must name the offending field: {err}"
    );
}

// ── #764: over-cap commit payload truncation ────────────────────────────────

/// Test-only [`EmbeddingService`] that records every text it is asked to
/// embed (so a test can assert exactly what reached the "provider") and
/// returns a deterministic constant vector, never failing.
struct CapturingEmbedService {
    captured: Arc<Mutex<Vec<String>>>,
    dims: usize,
}

#[async_trait]
impl EmbeddingService for CapturingEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        self.captured
            .lock()
            .expect("captured mutex")
            .extend(texts.iter().cloned());
        Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "capturing-test-embedder"
    }
}

struct CapturingEmbedProvider {
    captured: Arc<Mutex<Vec<String>>>,
    dims: usize,
}

#[async_trait]
impl EmbedderProvider for CapturingEmbedProvider {
    fn name(&self) -> &str {
        "capturing-test-embedder"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(CapturingEmbedService {
            captured: Arc::clone(&self.captured),
            dims: self.dims,
        }))
    }
}

/// A commit message over the 32,768-byte embedding cap must still create the
/// complete note (full content stored/FTS-indexed), must send only a capped,
/// UTF-8-safe head prefix to the embedder, and must report the truncation.
#[tokio::test]
async fn ingest_truncates_over_cap_commit_embedding_and_reports_it() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    // The project anchor entity is created (and, via `create_entity`, embedded)
    // BEFORE the capturing embedder is registered, so only the commit note's
    // embed call below is captured.
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "big-commit-repo"}),
    )
    .await;

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    rt.register_embedder(CapturingEmbedProvider {
        captured: Arc::clone(&captured),
        dims: 8,
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

    // 51,648 ASCII chars (matches the issue's first-consumer report), with a
    // unique term at the very start (head, must be embedded/searchable) and
    // a unique term at the very end (tail, must be stored/FTS-searchable but
    // is beyond the embedding cap).
    let filler = "x".repeat(51_648 - "head-term-unique ".len() - " tail-term-unique".len());
    let message = format!("head-term-unique {filler} tail-term-unique");
    assert!(message.len() > 32_768, "fixture must exceed the cap");
    commit(repo, &["README.md"], &message);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);
    assert_eq!(
        report.commit_embeddings_truncated, 1,
        "over-cap commit must be reported as truncated"
    );

    // Full content remains stored (including the tail sentinel beyond the cap).
    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(stored_content.contains("head-term-unique"));
    assert!(
        stored_content.contains("tail-term-unique"),
        "full commit message, including the tail beyond the cap, must be stored"
    );

    // Exactly one embed call was made, with a capped, head-only prefix.
    let seen = captured.lock().expect("captured mutex").clone();
    assert_eq!(seen.len(), 1, "exactly one embed call: {seen:?}");
    let embedded_text = &seen[0];
    assert!(
        embedded_text.len() <= 32_768,
        "embedder input must be at or under the cap: {} bytes",
        embedded_text.len()
    );
    assert!(
        embedded_text.contains("head-term-unique"),
        "embedder input must retain the head term"
    );
    assert!(
        !embedded_text.contains("tail-term-unique"),
        "embedder input must not reach past the cap into the tail"
    );
    assert!(
        stored_content.starts_with(embedded_text.as_str()),
        "embedder input must be a proper prefix of the stored content"
    );

    // A vector was actually inserted for this note (not skipped).
    let vectors = rt
        .vectors_for_model(&token, "capturing-test-embedder")
        .expect("vector store for capturing embedder");
    assert_eq!(
        vectors.count().await.expect("vector count"),
        1,
        "the capped head must have produced exactly one vector row"
    );

    // The head term is retrievable through the `search` verb (the design's
    // "vector evidence/search finds the head" requirement, satisfied here via
    // its search form in addition to the direct embedder-capture evidence
    // above), and the tail term — beyond the embedding cap but still part of
    // the fully stored/FTS-indexed content — remains independently
    // searchable too.
    // FTS5's bareword query grammar treats a run of whitespace-separated
    // terms as an implicit (adjacency-required) phrase, so the query text
    // uses spaces rather than the fixture's hyphenated compound — both tokenize
    // identically against the indexed content (hyphens are FTS5 word
    // separators too), but only the space form parses as the intended
    // three-token phrase rather than one hyphenated bareword.
    let head_hits = registry
        .dispatch(
            "search",
            json!({"kind": "commit", "query": "head term unique"}),
        )
        .await
        .expect("search ok");
    let head_hits = head_hits.as_array().expect("array");
    assert!(
        head_hits.iter().any(|h| h["id"] == items[0]["id"]),
        "the head term must resolve the truncated commit note via search: {head_hits:?}"
    );

    let tail_hits = registry
        .dispatch(
            "search",
            json!({"kind": "commit", "query": "tail term unique"}),
        )
        .await
        .expect("search ok");
    let tail_hits = tail_hits.as_array().expect("array");
    assert!(
        tail_hits.iter().any(|h| h["id"] == items[0]["id"]),
        "the tail term, beyond the embedding cap, must still resolve via FTS on the \
         complete stored content: {tail_hits:?}"
    );

    // A duplicate digest pass that re-walks the same over-cap commit (its
    // natural-key sha already has a note) must count it as a skip, and must
    // NOT re-increment the truncation counter — that counter only ever moves
    // on the successful-CREATE arm, which a skip never reaches. Re-walking
    // requires clearing the persisted cursor directly (the `{sha}..HEAD`
    // range `walk_commits` uses is exclusive of an already-advanced cursor,
    // so an ordinary duplicate `run_ingest` call would simply see zero
    // commits and never exercise the skip path at all).
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("sql writer");
    w.execute(SqlStatement {
        sql: "DELETE FROM git_mirror_cursor WHERE project_id=?1 AND kind='commits'".into(),
        params: vec![SqlValue::Text(project_id.to_string())],
        label: Some("test_reset_commits_cursor".into()),
    })
    .await
    .expect("reset cursor");
    drop(w);

    let second_report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("second ingest ok");
    assert_eq!(
        second_report.commits_ingested, 0,
        "no new commits on the re-walked pass"
    );
    assert_eq!(
        second_report.commits_skipped_existing, 1,
        "the already-ingested over-cap commit must be a natural-key skip"
    );
    assert_eq!(
        second_report.commit_embeddings_truncated, 0,
        "a re-walked pass must not re-report truncation for an existing commit"
    );
}

struct FailingEmbedService;

#[async_trait]
impl EmbeddingService for FailingEmbedService {
    async fn embed(
        &self,
        _texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::InferenceFailed(
            "simulated inference failure (issue #764 regression)".into(),
        ))
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "failing-test-embedder"
    }
}

struct FailingEmbedProvider;

#[async_trait]
impl EmbedderProvider for FailingEmbedProvider {
    fn name(&self) -> &str {
        "failing-test-embedder"
    }

    fn dimensions(&self) -> usize {
        8
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(FailingEmbedService))
    }
}

/// An over-cap commit whose vector embedding genuinely fails (a real
/// `EmbedError::InferenceFailed` from the registered embedder -- the existing
/// test-embedder seam, not a production fault-injection flag) must not leave
/// a partial record behind: `create_note_inner`'s existing compensation
/// removes the note row and its FTS document, the pass reports the failure
/// as a create-commit warning, and neither `commits_ingested` nor
/// `commit_embeddings_truncated` moves for that commit, since both only ever
/// advance on the successful-create arm (a prior fix).
#[tokio::test]
async fn ingest_over_cap_commit_with_failing_embedder_creates_nothing_and_warns() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "failing-embed-repo"}),
    )
    .await;

    rt.register_embedder(FailingEmbedProvider);

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

    let filler = "x".repeat(51_648 - "head-term-unique ".len() - " tail-term-unique".len());
    let message = format!("head-term-unique {filler} tail-term-unique");
    assert!(message.len() > 32_768, "fixture must exceed the cap");
    commit(repo, &["README.md"], &message);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok -- a per-commit create failure is a warning, not a hard pass error");

    assert_eq!(
        report.commits_ingested, 0,
        "the failed create must not be counted as ingested"
    );
    assert_eq!(
        report.commit_embeddings_truncated, 0,
        "truncation is only reported on the successful-create arm, which a failed \
         embed never reaches"
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("create commit")
            && w.contains("embedding inference failed")
            && w.contains("simulated inference failure")),
        "the embed failure must surface as an explicit create-commit warning: {:?}",
        report.warnings
    );

    // No note row survives the compensated create.
    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    assert_eq!(
        list.as_array().expect("array").len(),
        0,
        "a compensated create must leave no commit note row behind"
    );

    // No FTS hit either -- the head term never resolves anything, since the
    // FTS document was compensated away along with the note row.
    let head_hits = registry
        .dispatch(
            "search",
            json!({"kind": "commit", "query": "head term unique"}),
        )
        .await
        .expect("search ok");
    assert_eq!(
        head_hits.as_array().expect("array").len(),
        0,
        "no FTS document must survive the compensated create"
    );
}

/// Semantic-only retrieval: an explicit
/// deterministic query vector matching a real registered embedder's
/// constant output, paired with query text that is lexically absent from
/// the commit message, can only resolve the note through the vector leg --
/// the FTS leg cannot contribute a hit. This proves the stored vector
/// genuinely retrieves the created commit note by ID/kind through the
/// public `KhiveRuntime::search_notes` surface, not merely that a vector
/// row of some count exists or that FTS happens to also work.
///
/// `search_notes`'s vector leg always searches the store keyed by the
/// runtime's *configured default* embedding model
/// (`config().embedding_model`), so unlike the sibling truncation test's
/// custom-named `CapturingEmbedProvider` (whose vectors live in a
/// per-provider store `search_notes` cannot reach), this test registers its
/// deterministic fixture embedder under a real `EmbeddingModel` variant's
/// canonical name -- `EmbedderRegistry::register` is last-writer-wins, so
/// this replaces the default `LatticeEmbedderProvider` khive-runtime
/// auto-registers for a configured model.
#[tokio::test]
async fn ingest_over_cap_commit_embedding_is_semantically_retrievable() {
    const MODEL: EmbeddingModel = EmbeddingModel::BgeSmallEnV15;
    let dims = MODEL.dimensions();

    struct FixtureEmbedService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for FixtureEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "fixture-semantic-embedder"
        }
    }

    struct FixtureEmbedProvider {
        dims: usize,
    }

    #[async_trait]
    impl EmbedderProvider for FixtureEmbedProvider {
        fn name(&self) -> &str {
            "bge-small-en-v1.5"
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(FixtureEmbedService { dims: self.dims }))
        }
    }

    let _guard = ENV_MUTEX.lock().await;
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: Some(MODEL),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    })
    .expect("runtime with a configured default model");
    rt.register_embedder(FixtureEmbedProvider { dims });

    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(rt.backend());
    let token = rt.authorize(Namespace::local()).expect("authorize local");

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "semantic-retrieval-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

    let filler = "x".repeat(51_648 - "head-term-unique ".len() - " tail-term-unique".len());
    let message = format!("head-term-unique {filler} tail-term-unique");
    assert!(message.len() > 32_768, "fixture must exceed the cap");
    commit(repo, &["README.md"], &message);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);
    assert_eq!(report.commit_embeddings_truncated, 1);

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let commit_note_id =
        Uuid::parse_str(items[0]["id"].as_str().expect("commit id")).expect("commit id is uuid");

    let semantic_hits = rt
        .search_notes(
            &token,
            "lexically-absent-query-term-zzz",
            Some(vec![1.0_f32; dims]),
            10,
            Some("commit"),
            false,
            &[],
            None,
        )
        .await
        .expect("semantic search ok");
    assert!(
        semantic_hits.iter().any(|h| h.note_id == commit_note_id),
        "an explicit deterministic query vector matching the embedded head must \
         retrieve the commit note by ID even though the query text is lexically \
         absent from its content: {semantic_hits:?}"
    );
}

/// Under-cap commit content must not be truncated: the embedder sees the
/// full text and the report's truncation counter stays at zero. See
/// [`ingest_does_not_truncate_exact_cap_commit_embedding`] for the
/// exact-cap-boundary sibling of this test.
#[tokio::test]
async fn ingest_does_not_truncate_under_cap_commit_embedding() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    // Create the project anchor entity BEFORE registering the capturing
    // embedder, so only the commit note's embed call is captured below.
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "small-commit-repo"}),
    )
    .await;

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    rt.register_embedder(CapturingEmbedProvider {
        captured: Arc::clone(&captured),
        dims: 8,
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "A short, unremarkable commit message");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);
    assert_eq!(
        report.commit_embeddings_truncated, 0,
        "an under-cap commit must not be reported as truncated"
    );

    let seen = captured.lock().expect("captured mutex").clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains("A short, unremarkable commit message"),
        "the embedder must receive the full message unchanged: {:?}",
        seen[0]
    );
}

/// A commit message whose masked content lands EXACTLY on the 32,768-byte
/// cap (not one byte over) must not be truncated either: the design draws
/// the line at "at or below the cap", and `truncated_embedding_head`'s own
/// exact-cap unit test only proves the pure helper's behavior, not this
/// full `run_ingest` pipeline's counter and embedder-input wiring.
#[tokio::test]
async fn ingest_does_not_truncate_exact_cap_commit_embedding() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "exact-cap-commit-repo"}),
    )
    .await;

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    rt.register_embedder(CapturingEmbedProvider {
        captured: Arc::clone(&captured),
        dims: 8,
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

    // A single-line, body-less subject of exactly 32,768 ASCII bytes: `git`'s
    // `%s`/`%b` split leaves `body` empty, so `raw_content == subject` and
    // `content.len()` (post-mask, a no-op here) is exactly the cap.
    let message = "x".repeat(32_768);
    assert_eq!(
        message.len(),
        32_768,
        "fixture must land exactly on the cap"
    );
    commit(repo, &["README.md"], &message);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);
    assert_eq!(
        report.commit_embeddings_truncated, 0,
        "an exact-cap commit must not be reported as truncated"
    );

    let seen = captured.lock().expect("captured mutex").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].len(),
        32_768,
        "the embedder must receive the full, untruncated exact-cap message"
    );
}

/// With no embedder registered at all (the acceptance fixture's default),
/// an over-cap commit must still store the full note and report the
/// truncation — the counter reflects the capped candidate input regardless
/// of whether any embedder is configured to consume it.
#[tokio::test]
async fn ingest_reports_truncation_even_with_no_embedder_configured() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "no-embedder-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    let message = format!("head-of-message {}", "y".repeat(40_000));
    commit(repo, &["README.md"], &message);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);
    assert_eq!(report.commit_embeddings_truncated, 1);

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let stored_content = list[0]["content"].as_str().expect("content is string");
    assert!(stored_content.contains(&message));
}

/// A `Closes #N` reference placed in the tail of an over-cap commit message —
/// past the 32,768-byte embedding cap — must still resolve to a
/// `reference_edges_created` annotates edge. `link_references` reads
/// `NewRecordForRef.text`, which is the complete masked content (not the
/// capped embedding head), so a beyond-cap reference must be exactly as
/// resolvable as one in the head.
#[tokio::test]
async fn ingest_resolves_over_cap_commit_reference_beyond_the_embedding_cap() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Initial commit");

    let first = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 1)");
    let project_id = first["project_id"]
        .as_str()
        .expect("project_id present")
        .to_string();

    let issue_id = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 4242, "title": "Tail-referenced bug", "project_id": project_id},
            "annotates": [project_id],
        }),
    )
    .await;

    // The `Closes #4242` reference sits well past the 32,768-byte cap: the
    // filler alone already exceeds the cap before the reference paragraph.
    let filler = "z".repeat(40_000);
    let message = format!("Fix the tail bug\n\n{filler}\n\nCloses #4242");
    assert!(
        message.rfind("Closes #4242").unwrap() > 32_768,
        "fixture must place the reference beyond the embedding cap"
    );
    write(repo, "src/lib.rs", "// fix\n");
    commit(repo, &["src/lib.rs"], &message);

    let second = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "project": project_id, "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 2)");
    assert_eq!(second["commits_ingested"], 1, "{second}");
    assert_eq!(
        second["commit_embeddings_truncated"], 1,
        "the referencing commit is itself over-cap: {second}"
    );
    assert_eq!(
        second["reference_edges_created"], 1,
        "the beyond-cap Closes #4242 reference must still resolve: {second}"
    );

    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = issue_neighbors.as_array().expect("array");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["kind"], "commit");
}

// ── ENTITY_TYPES pack composition (dead-letter fix) ─────────────────────────
//
// The git pack declares `adr` as a `Document` entity-type subtype
// (`vocab::GIT_ENTITY_TYPES`) — see `find_document_for_path`'s doc comment
// above for why `document` entities matched by git-tracked file path are the
// motivating case. These tests exercise the real `create` verb end-to-end
// (dispatch -> handle_create -> validate_entity_type -> the boot-time
// composed registry), not just the isolated unit-level composition covered
// in `khive-runtime::pack`'s tests.

/// The git-pack-declared `adr` Document subtype validates through the real
/// `create` handler once `GitPack` is loaded, and its alias normalises to
/// the canonical name.
#[tokio::test]
async fn git_pack_adr_entity_type_validates_through_create() {
    let (_rt, _token, registry) = fixture().await;

    let resp = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "adr", "name": "ADR-001: example"}),
        )
        .await
        .expect("create must accept the git-pack-declared adr Document subtype");
    assert_eq!(resp["entity_type"], "adr", "{resp}");

    let resp_alias = registry
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "entity_type": "architecture_decision_record",
                "name": "ADR-002: example",
            }),
        )
        .await
        .expect("create must accept the adr alias");
    assert_eq!(resp_alias["entity_type"], "adr", "{resp_alias}");

    // Builtin Document subtypes remain resolvable alongside the pack extension.
    let resp_builtin = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "paper", "name": "Some paper"}),
        )
        .await
        .expect("builtin paper subtype must remain resolvable when git pack adds adr");
    assert_eq!(resp_builtin["entity_type"], "paper", "{resp_builtin}");
}

/// Without the git pack loaded, `adr` is not a registered Document subtype —
/// proving the acceptance above is genuine pack composition, not a builtin.
#[tokio::test]
async fn adr_entity_type_rejected_without_git_pack_loaded() {
    let rt = rt();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(rt.backend());

    let err = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "adr", "name": "ADR-001: example"}),
        )
        .await
        .expect_err("adr must be rejected when the declaring pack is not loaded");
    assert!(
        err.to_string().contains("adr"),
        "error must name the rejected value: {err}"
    );
}

/// Pins the RUNTIME-layer validator, not just the handler-layer check above.
///
/// `fixture()` now runs the same `call_register_entity_type_validators` boot
/// step production runs in `serve.rs` before the first dispatch. This test
/// calls `KhiveRuntime::create_many` directly (the defense-in-depth path for
/// Rust callers that bypass the `create` verb handler) and would fail if the
/// boot-time composed aggregate were ever absent or builtin-only — the gap
/// PR #925 flagged: the handler-layer test above would stay
/// green even if that wiring were silently dropped, since it never exercises
/// `create_many` directly.
///
/// The positive `adr` assertion alone cannot prove the aggregate validator is
/// actually installed: `KhiveRuntime::validate_entity_type_for_kind` returns
/// its input unchanged when no validator is installed at all, so a
/// positive-only test also passes with the boot wiring deleted entirely
/// (PR #925). The negative case below — an unregistered
/// Document subtype rejected as `RuntimeError::InvalidInput` — is what
/// actually distinguishes "composed from pack `ENTITY_TYPES`" from
/// "builtin-only" from "absent".
#[tokio::test]
async fn git_pack_adr_entity_type_validates_through_runtime_create_many() {
    let (rt, token, _registry) = fixture().await;

    let created = rt
        .create_many(
            &token,
            vec![EntityCreateSpec {
                kind: "document".to_string(),
                entity_type: Some("adr".to_string()),
                name: "ADR-003: runtime layer".to_string(),
                description: None,
                properties: None,
                tags: vec![],
            }],
        )
        .await
        .expect("runtime-layer create_many must accept the git-pack-declared adr subtype");
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(
        created[0].entity_type.as_deref(),
        Some("adr"),
        "{created:?}"
    );

    // Negative companion (PR #925): the positive `adr`
    // assertion above passes whether the aggregate validator is wired up
    // OR entirely absent, since `KhiveRuntime::validate_entity_type_for_kind`
    // returns the input unchanged when no validator is installed. Only a
    // rejection of an UNREGISTERED Document subtype proves a validator is
    // actually composed and consulted — this call fails closed if the boot
    // wiring (`call_register_entity_type_validators`) were ever dropped.
    let err = rt
        .create_many(
            &token,
            vec![EntityCreateSpec {
                kind: "document".to_string(),
                entity_type: Some("not_a_registered_subtype".to_string()),
                name: "not an ADR".to_string(),
                description: None,
                properties: None,
                tags: vec![],
            }],
        )
        .await
        .expect_err(
            "runtime-layer create_many must reject an unregistered Document subtype when the \
             aggregate entity-type validator is composed",
        );
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "unregistered entity_type must fail as InvalidInput, got: {err:?}"
    );
    assert!(
        err.to_string().contains("not_a_registered_subtype"),
        "error must name the rejected value: {err}"
    );
}

// ── Issue #841: residual unmasked ingest fields ─────────────────────────────
//
// PR #835's `ingest_masks_secrets_in_commit_message` and the PR-title/body
// tests above already cover `content`/`title`
// masking. These tests cover the fields #841 found still passed raw into
// gated `properties`/the note `name`: the commit note `name` (built from the
// raw subject), commit `author`/`author_email`, and PR `author`/`base_ref`/
// `head_ref`. Each fix pairs a credential-shaped positive (record must not
// be dropped, field must be masked) with a clean-input regression guard
// (field must survive byte-for-byte unchanged).

/// The commit note `name` was built from the raw (unmasked) commit subject
/// even though `content` already masked the same text — a credential-shaped
/// subject line tripped the runtime's `secret_gate::check(name)` call and
/// silently dropped the whole commit, despite `content` being safe.
#[tokio::test]
async fn ingest_masks_credential_shaped_commit_subject_in_name_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-subject-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], fake_token);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("create commit")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_name = items[0]["name"].as_str().expect("name is string");
    assert!(
        !stored_name.contains(fake_token),
        "raw token must not survive into the stored name: {stored_name:?}"
    );
    assert!(
        stored_name.contains("***MASKED***"),
        "masked marker must be present in name: {stored_name:?}"
    );
}

/// The commit `author` field entered gated `properties` raw — a
/// credential-shaped `git config user.name` silently dropped the commit via
/// the runtime's recursive `properties` secret scan.
#[tokio::test]
async fn ingest_masks_credential_shaped_commit_author_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-author-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF";
    git(repo, &["config", "user.name", fake_token]);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Normal commit message");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("create commit")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_author = items[0]["properties"]["author"]
        .as_str()
        .expect("properties.author is string");
    assert!(
        !stored_author.contains(fake_token),
        "raw token must not survive into properties.author: {stored_author:?}"
    );
    assert!(
        stored_author.contains("***MASKED***"),
        "masked marker must be present in properties.author: {stored_author:?}"
    );
}

/// The commit `author_email` field entered gated `properties` raw — a
/// credential-shaped `git config user.email` silently dropped the commit via
/// the runtime's recursive `properties` secret scan.
#[tokio::test]
async fn ingest_masks_credential_shaped_commit_author_email_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-author-email-credential-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);

    let fake_token = "ghp_GGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG";
    git(
        repo,
        &["config", "user.email", &format!("{fake_token}@example.com")],
    );
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Normal commit message");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.commits_ingested, 1,
        "the commit must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("create commit")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_email = items[0]["properties"]["author_email"]
        .as_str()
        .expect("properties.author_email is string");
    assert!(
        !stored_email.contains(fake_token),
        "raw token must not survive into properties.author_email: {stored_email:?}"
    );
    assert!(
        stored_email.contains("***MASKED***"),
        "masked marker must be present in properties.author_email: {stored_email:?}"
    );
}

/// Regression guard: clean (non-credential-shaped) commit author, email, and
/// subject must survive byte-for-byte unchanged — the fix above must not
/// become an over-aggressive masking regression.
#[tokio::test]
async fn ingest_leaves_clean_commit_author_and_subject_unmasked() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "commit-clean-fields-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    git(repo, &["config", "user.name", "Ada Lovelace"]);
    git(repo, &["config", "user.email", "ada@example.com"]);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Add README with usage notes");

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1, "{report:?}");

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_author = item["properties"]["author"]
        .as_str()
        .expect("author is string");
    let stored_email = item["properties"]["author_email"]
        .as_str()
        .expect("author_email is string");
    let stored_name = item["name"].as_str().expect("name is string");
    assert_eq!(
        stored_author, "Ada Lovelace",
        "clean author must be stored unchanged"
    );
    assert_eq!(
        stored_email, "ada@example.com",
        "clean author_email must be stored unchanged"
    );
    assert!(
        stored_name.ends_with("Add README with usage notes"),
        "clean subject must survive unmodified in name: {stored_name:?}"
    );
    assert!(
        !stored_author.contains("***MASKED***")
            && !stored_email.contains("***MASKED***")
            && !stored_name.contains("***MASKED***"),
        "no masking marker may appear on clean input: author={stored_author:?} email={stored_email:?} name={stored_name:?}"
    );
}

/// The PR `author` field entered gated `properties` raw — a credential-shaped
/// GitHub login silently dropped the PR via the runtime's recursive
/// `properties` secret scan (the sibling of the already-fixed issue
/// `author_login` bug).
#[tokio::test]
async fn ingest_masks_credential_shaped_pr_author_login_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-author-credential-repo"}),
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

    let fake_token = "ghp_HHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHHH";
    let pr_json = json!([{
        "number": 55,
        "title": "Fix pagination edge case",
        "author": {"login": fake_token},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "fix/pagination",
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_author = items[0]["properties"]["author"]
        .as_str()
        .expect("properties.author is string");
    assert!(
        !stored_author.contains(fake_token),
        "raw credential must not survive into properties.author: {stored_author:?}"
    );
    assert!(
        stored_author.contains("***MASKED***"),
        "masked marker must be present in properties.author: {stored_author:?}"
    );
}

/// The PR `base_ref` field entered gated `properties` raw — a
/// credential-shaped base branch name silently dropped the PR.
#[tokio::test]
async fn ingest_masks_credential_shaped_pr_base_ref_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-base-ref-credential-repo"}),
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

    let fake_token = "ghp_IIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIII";
    let pr_json = json!([{
        "number": 56,
        "title": "Retarget release branch",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": fake_token,
        "headRefName": "release/next",
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_base_ref = items[0]["properties"]["base_ref"]
        .as_str()
        .expect("properties.base_ref is string");
    assert!(
        !stored_base_ref.contains(fake_token),
        "raw credential must not survive into properties.base_ref: {stored_base_ref:?}"
    );
    assert!(
        stored_base_ref.contains("***MASKED***"),
        "masked marker must be present in properties.base_ref: {stored_base_ref:?}"
    );
}

/// The PR `head_ref` field entered gated `properties` raw — a
/// credential-shaped head branch name (realistic for a fork PR, where the
/// contributor fully controls the branch name) silently dropped the PR.
#[tokio::test]
async fn ingest_masks_credential_shaped_pr_head_ref_without_dropping_note() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-head-ref-credential-repo"}),
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

    let fake_token = "ghp_JJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJJ";
    let pr_json = json!([{
        "number": 57,
        "title": "Fork contribution",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": fake_token,
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");

    assert_eq!(
        report.prs_ingested, 1,
        "the PR must not be dropped: {report:?}"
    );
    assert!(
        report.warnings.iter().all(|w| !w.contains("pull_request")),
        "no silent-drop warning may be reported: {:?}",
        report.warnings
    );

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let stored_head_ref = items[0]["properties"]["head_ref"]
        .as_str()
        .expect("properties.head_ref is string");
    assert!(
        !stored_head_ref.contains(fake_token),
        "raw credential must not survive into properties.head_ref: {stored_head_ref:?}"
    );
    assert!(
        stored_head_ref.contains("***MASKED***"),
        "masked marker must be present in properties.head_ref: {stored_head_ref:?}"
    );
}

/// Regression guard: clean (non-credential-shaped) PR author, base ref, and
/// head ref must survive byte-for-byte unchanged.
#[tokio::test]
async fn ingest_leaves_clean_pr_author_and_refs_unmasked() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "pr-clean-author-refs-repo"}),
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

    let pr_json = json!([{
        "number": 58,
        "title": "Improve README clarity",
        "author": {"login": "octocat"},
        "createdAt": "2026-01-01T00:00:00Z",
        "mergedAt": null,
        "closedAt": null,
        "updatedAt": "2026-01-01T00:00:00Z",
        "baseRefName": "main",
        "headRefName": "docs/readme-clarity",
        "mergeCommit": null,
        "body": ""
    }])
    .to_string();

    write_fake_gh(&bin_dir, &log_dir, &pr_json, "[]");
    let _path_guard = PathGuard::install(&bin_dir);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.clone(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.prs_ingested, 1, "{report:?}");

    let prs_list = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs ok");
    let items = prs_list.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let item = &items[0];
    let stored_author = item["properties"]["author"]
        .as_str()
        .expect("author is string");
    let stored_base_ref = item["properties"]["base_ref"]
        .as_str()
        .expect("base_ref is string");
    let stored_head_ref = item["properties"]["head_ref"]
        .as_str()
        .expect("head_ref is string");
    assert_eq!(stored_author, "octocat", "clean author must be unchanged");
    assert_eq!(stored_base_ref, "main", "clean base_ref must be unchanged");
    assert_eq!(
        stored_head_ref, "docs/readme-clarity",
        "clean head_ref must be unchanged"
    );
    assert!(
        !stored_author.contains("***MASKED***")
            && !stored_base_ref.contains("***MASKED***")
            && !stored_head_ref.contains("***MASKED***"),
        "no masking marker may appear on clean input: author={stored_author:?} base_ref={stored_base_ref:?} head_ref={stored_head_ref:?}"
    );
}
