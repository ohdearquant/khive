//! Issue #765 acceptance/regression tests: recovery ordering (refetch before
//! reclone, bounded to one of each) and failure handling (unclassified
//! errors and an exhausted repair never produce a false "healthy" report),
//! driven through the real `pub(crate)` recovery surface
//! (`ingest::run_ingest_with_commit_recovery`,
//! `handlers::RemoteCommitRecovery`) rather than a re-implementation of it.
//!
//! A real corrupt-promisor-cache is finicky to construct deterministically
//! (git's own on-demand lazy-fetch silently repairs most local-remote
//! corruption before this code ever sees an error) -- instead this uses a
//! PATH-shadowing `git` shim (the same technique `tests/acceptance.rs` uses
//! for a fake `gh`) that scripts exactly the failure sequence issue #765
//! describes: `git log --name-only` fails with the reported promisor
//! diagnostic, and a subsequent `git fetch --refetch`/reclone repairs it.
//! Every command the shim does not specifically script falls through to the
//! real `git` binary, so origin setup, `ensure_clone`'s initial clone, and
//! everything else in the ingest pass runs for real.

use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, VerbRegistry, VerbRegistryBuilder};

use crate::cache;
use crate::handlers::RemoteCommitRecovery;
use crate::ingest::{run_ingest_with_commit_recovery, IngestInclude, IngestOptions};
use crate::GitPack;

/// Every test here mutates process-global state (`PATH`,
/// `KHIVE_GIT_DIGEST_SCRATCH_ROOT`) -- share `cache`'s lock so these tests
/// never race against `cache::tests` or each other within the same `cargo
/// test` binary.
async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    cache::ENV_MUTEX.lock().await
}

/// Sync counterpart of [`env_guard`] for this module's one plain `#[test]`.
fn env_guard_sync() -> tokio::sync::MutexGuard<'static, ()> {
    cache::ENV_MUTEX.blocking_lock()
}

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

fn resolve_real_git() -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg("command -v git")
        .output()
        .expect("resolve real git");
    assert!(out.status.success(), "could not resolve real git on PATH");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Write a PATH-shadowing `git` shim into `bin_dir`: it logs every
/// invocation's argv into `log_dir/git_args.log`, fails the first
/// `KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL` invocations of `git log
/// --name-only` with the exact diagnostic issue #765 quotes, fails the
/// first `KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL` invocations of `git fetch
/// --refetch`, and delegates every other invocation (including all of
/// origin setup, `ensure_clone`'s clone, and `reclone`'s fresh clone) to the
/// real `git` binary resolved before PATH was shadowed.
fn write_fake_git(bin_dir: &Path, log_dir: &Path) {
    let real_git = resolve_real_git();
    let script = format!(
        r#"#!/bin/sh
REAL_GIT="{real_git}"
LOG_DIR="{log_dir}"
printf '%s\n' "$*" >> "$LOG_DIR/git_args.log"

case " $* " in
  *" --name-only "*)
    COUNT_FILE="$LOG_DIR/name_only.count"
    n=$(cat "$COUNT_FILE" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$COUNT_FILE"
    limit="${{KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL:-0}}"
    if [ "$n" -le "$limit" ]; then
      echo "fatal: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef is in the commit graph file, but not in the object database" 1>&2
      echo "fatal: could not fetch from promisor remote" 1>&2
      exit 1
    fi
    ;;
esac

case " $* " in
  *" --refetch "*)
    COUNT_FILE="$LOG_DIR/refetch.count"
    n=$(cat "$COUNT_FILE" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$COUNT_FILE"
    limit="${{KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL:-0}}"
    if [ "$n" -le "$limit" ]; then
      echo "fatal: unable to access 'origin': simulated refetch failure" 1>&2
      exit 1
    fi
    ;;
esac

exec "$REAL_GIT" "$@"
"#,
        real_git = real_git,
        log_dir = log_dir.display(),
    );
    let script_path = bin_dir.join("git");
    std::fs::write(&script_path, script).expect("write fake git script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("fake git metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod fake git");
    }
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

fn init_origin_with_one_commit(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test User"]);
    std::fs::write(repo.join("a.txt"), b"hello").unwrap();
    git(repo, &["add", "a.txt"]);
    git(repo, &["commit", "-q", "-m", "initial"]);
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

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

async fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let rt = rt();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
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

fn commits_only_opts(
    repo: std::path::PathBuf,
    project: String,
    max_items: Option<u64>,
) -> IngestOptions {
    IngestOptions {
        repo,
        project,
        max_items,
        include: IngestInclude {
            commits: true,
            issues: false,
            pull_requests: false,
        },
    }
}

fn count_lines(path: &Path, needle: &str) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

/// Same scripted promisor-corruption/refetch-failure behavior as
/// [`write_fake_git`], plus one addition the mandatory public-verb fixture
/// needs: `git clone`/`fetch` invocations naming `fake_url` (the synthetic
/// `https://github.com/...` source `git.digest` is given) are rewritten to
/// `local_origin` before delegating to the real `git` binary, so `ensure_clone`'s
/// very first clone -- and everything downstream, since the resulting slot's
/// `origin` remote is then the real local path -- runs against a real local
/// repository instead of the network. `#!/bin/bash` (not `/bin/sh`) so the
/// argv rewrite can use a real array rather than fragile word-splitting.
fn write_fake_git_redirecting_clone(
    bin_dir: &Path,
    log_dir: &Path,
    fake_url: &str,
    local_origin: &Path,
) {
    let real_git = resolve_real_git();
    let script = format!(
        r#"#!/bin/bash
REAL_GIT="{real_git}"
LOG_DIR="{log_dir}"
FAKE_URL="{fake_url}"
LOCAL_ORIGIN="{local_origin}"
printf '%s\n' "$*" >> "$LOG_DIR/git_args.log"

case " $* " in
  *" --name-only "*)
    COUNT_FILE="$LOG_DIR/name_only.count"
    n=$(cat "$COUNT_FILE" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$COUNT_FILE"
    limit="${{KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL:-0}}"
    if [ "$n" -le "$limit" ]; then
      echo "fatal: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef is in the commit graph file, but not in the object database" 1>&2
      echo "fatal: could not fetch from promisor remote" 1>&2
      exit 1
    fi
    ;;
esac

case " $* " in
  *" --refetch "*)
    COUNT_FILE="$LOG_DIR/refetch.count"
    n=$(cat "$COUNT_FILE" 2>/dev/null || echo 0)
    n=$((n + 1))
    echo "$n" > "$COUNT_FILE"
    limit="${{KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL:-0}}"
    if [ "$n" -le "$limit" ]; then
      echo "fatal: unable to access 'origin': simulated refetch failure" 1>&2
      exit 1
    fi
    ;;
esac

args=()
for a in "$@"; do
  if [ "$a" = "$FAKE_URL" ]; then
    args+=("$LOCAL_ORIGIN")
  else
    args+=("$a")
  fi
done
exec "$REAL_GIT" "${{args[@]}}"
"#,
        real_git = real_git,
        log_dir = log_dir.display(),
        fake_url = fake_url,
        local_origin = local_origin.display(),
    );
    let script_path = bin_dir.join("git");
    std::fs::write(&script_path, script).expect("write fake git script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)
            .expect("fake git metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).expect("chmod fake git");
    }
}

/// Minimal fake `gh` (mirrors `tests/acceptance.rs`'s `write_fake_gh`, not
/// reusable directly since it lives in a separate integration-test binary
/// target): logs argv, answers `--version`/`pr`/`issue` with canned JSON.
fn write_fake_gh(bin_dir: &Path, log_dir: &Path, pr_json: &str, issue_json: &str) {
    std::fs::write(log_dir.join("pr_response.json"), pr_json).expect("write pr fixture");
    std::fs::write(log_dir.join("issue_response.json"), issue_json).expect("write issue fixture");
    let script = format!(
        r#"#!/bin/sh
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
        args_log = log_dir.join("gh_args.log").display(),
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

fn add_commit(repo: &Path, rel: &str, contents: &str, message: &str) {
    std::fs::write(repo.join(rel), contents).unwrap();
    git(repo, &["add", rel]);
    git(repo, &["commit", "-q", "-m", message]);
}

/// The literal #765 acceptance criterion: a corrupt promisor cache digests
/// successfully on the *first* caller-visible request. The first `git log
/// --name-only` fails with the reported diagnostic; `RemoteCommitRecovery`
/// repairs it with exactly one `git fetch --refetch`; the same invocation's
/// commit phase then succeeds, and the caller never sees the corrupt-cache
/// error.
#[tokio::test]
async fn corrupt_promisor_cache_self_heals_via_refetch_on_first_call() {
    let _env = env_guard().await;
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let log_dir = tempfile::tempdir().expect("log dir");
    write_fake_git(bin_dir.path(), log_dir.path());
    let _path_guard = PathGuard::install(bin_dir.path());
    std::env::set_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL", "1");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL");

    let scratch = tempfile::tempdir().expect("scratch root");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let origin = tempfile::tempdir().expect("origin dir");
    init_origin_with_one_commit(origin.path());
    let canonical = origin.path().to_str().unwrap().to_string();

    // Prime a real cache slot (mirrors `handle_digest`'s remote path).
    let repo_path = cache::ensure_clone(&canonical).expect("ensure_clone");

    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "corrupt-cache-repo"}),
    )
    .await;

    let mut recovery = RemoteCommitRecovery::new(canonical.clone());
    let report = run_ingest_with_commit_recovery(
        &rt,
        &token,
        &registry,
        commits_only_opts(repo_path, project_id.to_string(), Some(10)),
        move |repo, err| recovery.repair(repo, err),
    )
    .await
    .expect("first digest call must self-heal, not error");

    assert_eq!(report.commits_ingested, 1);
    assert_eq!(
        report.warnings,
        vec!["repaired corrupt remote git cache by refetching missing promisor objects"]
    );
    assert!(report.done);

    // Exactly one refetch, and `--name-only` retried exactly once after the
    // scripted failure (two invocations total: the failing one, then the
    // post-repair success) -- proves the bounded, single-repair ordering
    // rather than a retry loop.
    let args_log = log_dir.path().join("git_args.log");
    assert_eq!(count_lines(&args_log, "--refetch"), 1);
    assert_eq!(count_lines(&args_log, "--name-only"), 2);

    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL");
}

/// When the refetch command itself fails at the git level, the bounded
/// state machine falls through to exactly one owned reclone in the *same*
/// invocation -- still no corrupt-cache error reaches the caller, and the
/// warning names the strategy that actually succeeded (reclone), not the
/// one that was tried first.
#[tokio::test]
async fn refetch_failure_falls_through_to_one_reclone_and_still_self_heals() {
    let _env = env_guard().await;
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let log_dir = tempfile::tempdir().expect("log dir");
    write_fake_git(bin_dir.path(), log_dir.path());
    let _path_guard = PathGuard::install(bin_dir.path());
    std::env::set_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL", "1");
    std::env::set_var("KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL", "1");

    let scratch = tempfile::tempdir().expect("scratch root");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let origin = tempfile::tempdir().expect("origin dir");
    init_origin_with_one_commit(origin.path());
    let canonical = origin.path().to_str().unwrap().to_string();
    let repo_path = cache::ensure_clone(&canonical).expect("ensure_clone");

    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "refetch-fails-repo"}),
    )
    .await;

    let mut recovery = RemoteCommitRecovery::new(canonical.clone());
    let report = run_ingest_with_commit_recovery(
        &rt,
        &token,
        &registry,
        commits_only_opts(repo_path, project_id.to_string(), Some(10)),
        move |repo, err| recovery.repair(repo, err),
    )
    .await
    .expect("refetch failure must still self-heal via reclone");

    assert_eq!(report.commits_ingested, 1);
    assert_eq!(
        report.warnings,
        vec!["repaired corrupt remote git cache by replacing the owned clone"]
    );

    let args_log = log_dir.path().join("git_args.log");
    assert_eq!(
        count_lines(&args_log, "--refetch"),
        1,
        "exactly one refetch must be attempted before falling through to reclone"
    );
    // One clone for the initial `ensure_clone`, one more for the reclone.
    assert_eq!(
        std::fs::read_to_string(&args_log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("-c core.hooksPath=/dev/null clone "))
            .count(),
        2,
        "exactly one reclone (plus the initial ensure_clone) must occur"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL");
}

/// A persistent classified failure (refetch repairs the transport but the
/// same corruption reappears, and reclone doesn't help either in this
/// scripted scenario) is terminal: no third repair is attempted, the
/// original classified error surfaces to the caller, and no success warning
/// is ever emitted for a call that did not actually self-heal.
#[tokio::test]
async fn persistent_corruption_is_bounded_and_never_reports_false_success() {
    let _env = env_guard().await;
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let log_dir = tempfile::tempdir().expect("log dir");
    write_fake_git(bin_dir.path(), log_dir.path());
    let _path_guard = PathGuard::install(bin_dir.path());
    // Every `--name-only` call fails, no matter how many recovery attempts.
    std::env::set_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL", "999");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL");

    let scratch = tempfile::tempdir().expect("scratch root");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let origin = tempfile::tempdir().expect("origin dir");
    init_origin_with_one_commit(origin.path());
    let canonical = origin.path().to_str().unwrap().to_string();
    let repo_path = cache::ensure_clone(&canonical).expect("ensure_clone");

    let (rt, token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "persistent-corruption-repo"}),
    )
    .await;

    let mut recovery = RemoteCommitRecovery::new(canonical.clone());
    let err = run_ingest_with_commit_recovery(
        &rt,
        &token,
        &registry,
        commits_only_opts(repo_path, project_id.to_string(), Some(10)),
        move |repo, err| recovery.repair(repo, err),
    )
    .await
    .expect_err("a failure neither refetch nor reclone can fix must surface");

    assert!(
        err.to_string().contains("promisor"),
        "the terminal error must be the underlying classified diagnostic: {err}"
    );

    let args_log = log_dir.path().join("git_args.log");
    assert_eq!(
        count_lines(&args_log, "--refetch"),
        1,
        "bounded to exactly one refetch attempt even though corruption persists"
    );
    assert_eq!(
        count_lines(&args_log, "--name-only"),
        3,
        "three snapshot attempts: initial, post-refetch, post-reclone -- no fourth"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL");
}

/// Local-path sources never repair: `run_ingest` (the CLI/local entry point)
/// passes a no-op recovery callback, so a classified failure surfaces
/// immediately even though the exact same corruption would self-heal for a
/// remote source. This is a deliberate ADR-088 Amendment 1 boundary (the
/// disposable cache is remote-URL-mode only) -- a local path is the
/// caller's own working copy, never a candidate for eviction/reclone.
#[tokio::test]
async fn local_source_never_repairs_even_when_recovery_would_succeed() {
    let _env = env_guard().await;
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let log_dir = tempfile::tempdir().expect("log dir");
    write_fake_git(bin_dir.path(), log_dir.path());
    let _path_guard = PathGuard::install(bin_dir.path());
    std::env::set_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL", "1");

    let repo = tempfile::tempdir().expect("local repo");
    init_origin_with_one_commit(repo.path());

    let (rt, token, registry) = fixture().await;
    let project_id = create(&registry, json!({"kind": "project", "name": "local-repo"})).await;

    let err = crate::ingest::run_ingest(
        &rt,
        &token,
        &registry,
        commits_only_opts(repo.path().to_path_buf(), project_id.to_string(), Some(10)),
    )
    .await
    .expect_err("local run_ingest must never repair, even a repairable failure");
    assert!(err.to_string().contains("promisor"));

    std::env::remove_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL");
}

/// The literal `head_sha` helper is exercised indirectly by the scenarios
/// above (via `ensure_clone`/`reclone` producing a real working clone) --
/// this direct unit guards the helper itself so a future refactor of it
/// can't silently break the assertions that depend on it.
#[test]
fn head_sha_reads_the_real_current_commit() {
    let _env = env_guard_sync();
    let dir = tempfile::tempdir().expect("tempdir");
    init_origin_with_one_commit(dir.path());
    let sha = head_sha(dir.path());
    assert_eq!(sha.len(), 40);
    assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
}

/// Review round-1 [Medium]-3 remediation: the approved design's mandatory
/// `max_items = 4` partial-side-effect fixture (`architect-2/approved_design.md`
/// §5), driven through the real public `git.digest` verb
/// (`registry.dispatch`, an HTTPS remote source, and a fake `gh`) rather than
/// the internal `run_ingest_with_commit_recovery` surface the other tests in
/// this module use directly. Proves request-wide state (`Budget`,
/// `IngestReport`, PR/merge maps, reference candidates) truly survives a
/// commit-snapshot repair when driven end-to-end: a pre-recovery PR create
/// failure's warning is retained, the recovered commit still carries its
/// merge annotation to the PR discovered before the repair, and the PR's
/// `Fixes #1` body still resolves to a `closes` edge onto the issue -- none
/// of which the commits-only internal-surface tests above can observe.
#[tokio::test]
async fn public_verb_partial_side_effects_survive_commit_snapshot_recovery() {
    use async_trait::async_trait;
    use khive_runtime::{arm_vector_fail, EmbedderProvider};
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const FIXTURE_MODEL: &str = "mandatory-765-const-vec";
    const FIXTURE_DIMS: usize = 4;

    struct FixtureEmbeddingService;
    #[async_trait]
    impl EmbeddingService for FixtureEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; FIXTURE_DIMS]).collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            FIXTURE_MODEL
        }
    }

    struct FixtureEmbedderProvider;
    #[async_trait]
    impl EmbedderProvider for FixtureEmbedderProvider {
        fn name(&self) -> &str {
            FIXTURE_MODEL
        }
        fn dimensions(&self) -> usize {
            FIXTURE_DIMS
        }
        async fn build(
            &self,
        ) -> khive_runtime::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(FixtureEmbeddingService))
        }
    }

    let _env = env_guard().await;
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let log_dir = tempfile::tempdir().expect("log dir");
    std::env::set_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL", "1");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_REFETCH_UNTIL");

    let scratch = tempfile::tempdir().expect("scratch root");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    // Two-commit origin: the first (fixture) commit is the one PR #2's
    // `mergeCommit.oid` names, so a successful merge-commit annotation
    // proves the PR map survived into the recovered commit phase; the
    // second commit deliberately remains unfetched (`max_items = 4` is
    // exhausted by then) so `done: false` is meaningful, not vacuous.
    let origin = tempfile::tempdir().expect("origin dir");
    init_origin_with_one_commit(origin.path());
    let fixture_commit_sha = head_sha(origin.path());
    add_commit(origin.path(), "b.txt", "second\n", "second commit");

    let fake_url = "https://github.com/khive-fixture/mandatory-765";
    write_fake_git_redirecting_clone(bin_dir.path(), log_dir.path(), fake_url, origin.path());

    // PR #3 has an earlier `updatedAt` than PR #2, so `ingest_prs`'s
    // `sort:updated-asc` paging processes it FIRST -- letting one process-
    // global one-shot `arm_vector_fail` injection land on exactly this PR's
    // create, not PR #2's, without needing per-call count-targeting.
    let pr_json = json!([
        {
            "number": 3, "title": "chore: sentinel", "author": {"login": "bot"},
            "createdAt": "2026-01-01T00:00:00Z", "mergedAt": null, "closedAt": null,
            "updatedAt": "2026-01-01T00:00:00Z", "baseRefName": "main",
            "headRefName": "chore/sentinel", "mergeCommit": null, "body": "warning-sentinel"
        },
        {
            "number": 2, "title": "Fix the bug", "author": {"login": "bot"},
            "createdAt": "2026-01-01T00:00:00Z", "mergedAt": "2026-01-02T00:00:00Z",
            "closedAt": "2026-01-02T00:00:00Z", "updatedAt": "2026-01-02T00:00:00Z",
            "baseRefName": "main", "headRefName": "fix/bug",
            "mergeCommit": {"oid": fixture_commit_sha}, "body": "Fixes #1"
        }
    ])
    .to_string();
    let issue_json = json!([
        {
            "number": 1, "title": "Some bug", "author": {"login": "bot"},
            "createdAt": "2026-01-01T00:00:00Z", "closedAt": null,
            "updatedAt": "2026-01-01T00:00:00Z", "labels": [], "stateReason": "", "body": ""
        }
    ])
    .to_string();
    write_fake_gh(bin_dir.path(), log_dir.path(), &pr_json, &issue_json);

    let _path_guard = PathGuard::install(bin_dir.path());

    let (rt, _token, registry) = fixture().await;
    rt.register_embedder(FixtureEmbedderProvider);
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "mandatory-765-repo"}),
    )
    .await;

    // Fires on the next note create in the `local` namespace (the namespace
    // every `registry.dispatch` call is pinned to) -- that is PR #3's,
    // processed first per the `updatedAt` ordering above -- then disarms.
    arm_vector_fail("local");

    let resp = registry
        .dispatch(
            "git.digest",
            json!({"source": fake_url, "project": project_id.to_string(), "max_items": 4}),
        )
        .await
        .expect("the public verb call must self-heal, not error");

    assert_eq!(resp["prs_ingested"], 1, "{resp}");
    assert_eq!(resp["issues_ingested"], 1, "{resp}");
    assert_eq!(resp["commits_ingested"], 1, "{resp}");
    assert_eq!(
        resp["done"], false,
        "the second fixture commit must remain after max_items=4 exhausts: {resp}"
    );
    assert_eq!(resp["reference_edges_created"], 1, "{resp}");

    let warnings: Vec<&str> = resp["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .map(|w| w.as_str().expect("warning is a string"))
        .collect();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("create pull_request #3")),
        "the pre-recovery PR #3 create failure warning must be retained: {warnings:?}"
    );
    assert!(
        warnings
            .contains(&"repaired corrupt remote git cache by refetching missing promisor objects"),
        "the recovery warning must be reported: {warnings:?}"
    );

    // Bounded to one refetch, no reclone, two snapshot attempts, one GH
    // phase (approved design §5 mandatory fixture, assertion 6).
    let args_log = log_dir.path().join("git_args.log");
    assert_eq!(
        count_lines(&args_log, "--refetch"),
        1,
        "exactly one refetch"
    );
    assert_eq!(
        count_lines(&args_log, "--name-only"),
        2,
        "two snapshot attempts: the scripted failure, then the post-repair success"
    );
    assert_eq!(
        std::fs::read_to_string(&args_log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("-c core.hooksPath=/dev/null clone "))
            .count(),
        1,
        "no reclone: only the initial ensure_clone's own clone"
    );
    let gh_args_log = log_dir.path().join("gh_args.log");
    assert_eq!(
        count_lines(&gh_args_log, "pr "),
        1,
        "one GH phase for pull requests, not re-run by recovery"
    );
    assert_eq!(
        count_lines(&gh_args_log, "issue "),
        1,
        "one GH phase for issues, not re-run by recovery"
    );

    // No duplicate notes by natural key: exactly the successful records
    // exist (PR #3's compensated create left nothing behind).
    let prs = registry
        .dispatch("list", json!({"kind": "pull_request", "limit": 10}))
        .await
        .expect("list prs");
    let pr_items = prs.as_array().expect("array");
    assert_eq!(
        pr_items.len(),
        1,
        "PR #3's compensated create must leave no note behind: {pr_items:?}"
    );
    let pr2 = &pr_items[0];
    assert_eq!(pr2["properties"]["number"], 2, "{pr2}");
    let pr2_id = pr2["id"].as_str().expect("pr2 id").to_string();

    let issues = registry
        .dispatch("list", json!({"kind": "issue", "limit": 10}))
        .await
        .expect("list issues");
    assert_eq!(issues.as_array().expect("array").len(), 1, "{issues:?}");
    let issue1_id = issues[0]["id"].as_str().expect("issue1 id").to_string();

    let commits = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list commits");
    let commit_items = commits.as_array().expect("array");
    assert_eq!(commit_items.len(), 1, "{commit_items:?}");
    assert_eq!(
        commit_items[0]["properties"]["sha"], fixture_commit_sha,
        "the recovered commit phase must have ingested exactly the fixture commit"
    );
    let commit_id = commit_items[0]["id"]
        .as_str()
        .expect("commit id")
        .to_string();

    // One commit-to-PR merge annotation: the recovered commit phase's
    // `merge_sha_to_pr` map (populated during the PR phase, before the
    // repair) resolved the fixture commit's merging PR.
    let commit_annotates = registry
        .dispatch(
            "neighbors",
            json!({"id": commit_id, "direction": "outgoing", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let commit_annotates_ids: Vec<&str> = commit_annotates
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().expect("id"))
        .collect();
    assert!(
        commit_annotates_ids.contains(&pr2_id.as_str()),
        "the recovered commit must still annotate the PR discovered before the repair: {commit_annotates:?}"
    );

    // One PR-to-issue annotates edge, ref_kind=closes, from PR #2's `Fixes
    // #1` body (materialized by the post-recovery `link_references` sweep,
    // proving `new_records` survived the repair too).
    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue1_id, "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let issue_hits = issue_neighbors.as_array().expect("array");
    assert_eq!(issue_hits.len(), 1, "{issue_hits:?}");
    assert_eq!(issue_hits[0]["id"].as_str().unwrap(), pr2_id);

    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
    std::env::remove_var("KHIVE_TEST_GIT_FAIL_NAME_ONLY_UNTIL");
}

/// Review round-2 [High]-1 remediation (issue #765): the public `git.digest`
/// verb must refuse a markerless, cache-key-shaped real Git directory
/// sitting at the scratch-cache slot path rather than fetching into it or
/// making it eligible for later deletion by the recovery path -- driven
/// through the real public verb surface (`registry.dispatch`), not the
/// internal `cache::ensure_clone` primitive `cache.rs`'s own unit tests
/// exercise directly. Sentinel operator data inside the lookalike directory
/// must survive completely untouched, and no ownership marker is written.
#[tokio::test]
async fn public_verb_refuses_a_markerless_lookalike_at_the_cache_key_path() {
    let _env = env_guard().await;
    let scratch = tempfile::tempdir().expect("scratch root");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let fake_url = "https://github.com/khive-fixture/lookalike-repo";
    let key = crate::source::cache_key(fake_url);
    let lookalike = scratch.path().join(&key);
    std::fs::create_dir_all(&lookalike).unwrap();
    init_origin_with_one_commit(&lookalike);
    std::fs::write(lookalike.join("sentinel.txt"), b"do not delete me").unwrap();
    let sentinel_sha = head_sha(&lookalike);

    let (_rt, _token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "lookalike-repo"}),
    )
    .await;

    let err = registry
        .dispatch(
            "git.digest",
            json!({"source": fake_url, "project": project_id.to_string(), "max_items": 4}),
        )
        .await
        .expect_err("a markerless lookalike at the cache-key path must be refused");
    assert!(
        err.to_string()
            .contains("does not prove itself an owned cache slot"),
        "the refusal must surface the ownership-guard reason: {err}"
    );

    assert!(
        lookalike.join("sentinel.txt").exists(),
        "sentinel operator data must survive a refused git.digest request"
    );
    assert_eq!(
        head_sha(&lookalike),
        sentinel_sha,
        "the lookalike repository's own history must be untouched (no fetch ran)"
    );
    assert!(
        !lookalike.join(".khive-last-used").exists(),
        "a refused request must never write the ownership marker"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
}

/// Same guard, symlink variant, at the public verb surface: a symlink placed
/// at the cache-key path pointing at an unrelated directory must not be
/// treated as an owned slot either -- `git.digest` must refuse rather than
/// following the symlink into a fetch or eviction.
#[cfg(unix)]
#[tokio::test]
async fn public_verb_refuses_a_symlink_at_the_cache_key_path() {
    let _env = env_guard().await;
    let scratch = tempfile::tempdir().expect("scratch root");
    std::env::set_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT", scratch.path());

    let fake_url = "https://github.com/khive-fixture/symlink-lookalike-repo";
    let key = crate::source::cache_key(fake_url);
    let link_path = scratch.path().join(&key);

    let target_root = tempfile::tempdir().expect("symlink target root");
    let target = target_root.path().join("unrelated-repo");
    std::fs::create_dir_all(&target).unwrap();
    init_origin_with_one_commit(&target);
    std::fs::write(target.join("sentinel.txt"), b"do not delete me").unwrap();
    std::os::unix::fs::symlink(&target, &link_path).expect("create symlink");

    let (_rt, _token, registry) = fixture().await;
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "symlink-lookalike-repo"}),
    )
    .await;

    let err = registry
        .dispatch(
            "git.digest",
            json!({"source": fake_url, "project": project_id.to_string(), "max_items": 4}),
        )
        .await
        .expect_err("a symlink at the cache-key path must be refused");
    assert!(
        err.to_string()
            .contains("does not prove itself an owned cache slot"),
        "the refusal must surface the ownership-guard reason: {err}"
    );
    assert!(
        target.join("sentinel.txt").exists(),
        "the symlink target's sentinel data must survive a refused git.digest request"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_SCRATCH_ROOT");
}
