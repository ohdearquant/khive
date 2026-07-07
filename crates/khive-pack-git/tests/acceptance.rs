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
use std::sync::LazyLock;

use khive_pack_git::ingest::{run_ingest, IngestOptions};
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, VerbRegistry, VerbRegistryBuilder};
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

// ── project-scoped idempotency (review round-1 [High] #1) ──────────────────

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
        IngestOptions {
            repo: repo_b.clone(),
            project: project_b.to_string(),
        },
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
        IngestOptions {
            repo: repo_b.clone(),
            project: project_b.to_string(),
        },
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

// ── gh boundary contract + per-record warning aggregation (review round-1
//    [High] #2, [Medium] #3, [Medium] #4) ───────────────────────────────────

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
        IngestOptions {
            repo: repo.clone(),
            project: project_id.to_string(),
        },
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

    // (b)/(c)/(d) + Finding 2: pull_request properties use base_ref/head_ref.
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
        IngestOptions {
            repo: repo.clone(),
            project: project_id.to_string(),
        },
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
