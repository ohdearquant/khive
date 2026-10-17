fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

fn list_items(response: &Value) -> &[Value] {
    response["items"]
        .as_array()
        .expect("list response must contain an items array")
}

/// The exact value a logged `gh` invocation line passed after `--repo`, or
/// `None` if the line has no `--repo` flag. Whitespace-tokenized rather than
/// substring-matched: `line.contains("--repo fixture/repository")` also
/// matches `--repo fixture/repository-evil` or `--repo
/// not-fixture/repository`, so a probe that silently widened past the
/// pinned repository would still read as passing.
fn repo_flag_value(line: &str) -> Option<&str> {
    let mut tokens = line.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--repo" {
            return tokens.next();
        }
    }
    None
}

#[test]
fn repo_flag_value_rejects_a_value_that_merely_contains_the_pinned_repo() {
    assert_eq!(
        repo_flag_value("pr list --repo fixture/repository --state all"),
        Some("fixture/repository")
    );
    assert_ne!(
        repo_flag_value("pr list --repo fixture/repository-evil --state all"),
        Some("fixture/repository"),
        "a --repo value that merely contains the pinned repo string must not read as a match"
    );
    assert_ne!(
        repo_flag_value("pr list --repo not-fixture/repository --state all"),
        Some("fixture/repository"),
        "a --repo value containing the pinned repo string as a suffix must not read as a match"
    );
    assert_eq!(repo_flag_value("pr list --state all"), None);
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
/// `issue list` with the given canned JSON bodies. `repo view` returns the
/// stable repository identity used by the source-bound capability probe.
fn write_fake_gh(repo: &Path, bin_dir: &Path, log_dir: &Path, pr_json: &str, issue_json: &str) {
    // The production probe derives its only admissible repository target from
    // `origin`; test fixtures must therefore carry the same identity returned
    // by the fake `gh`. A PATH stub alone must never make a remote-less checkout
    // look GitHub-capable.
    git(
        repo,
        &[
            "config",
            "remote.origin.url",
            "https://github.com/fixture/repository.git",
        ],
    );
    std::fs::write(log_dir.join("pr_response.json"), pr_json).expect("write pr fixture");
    std::fs::write(log_dir.join("issue_response.json"), issue_json).expect("write issue fixture");

    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$(pwd)" >> '{cwd_log}'
printf '%s\n' "$*" >> '{args_log}'
case "$1" in
  repo)
    if [ "$2" != "view" ] || [ "$3" != "fixture/repository" ]; then
      echo "fake gh: repository probe was not explicitly source-bound: $*" 1>&2
      exit 2
    fi
    echo '{{"nameWithOwner":"fixture/repository","url":"https://github.com/fixture/repository"}}'
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
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    builder
        .with_runtime_event_store(&rt)
        .expect("configure trusted runtime audit store");
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    // Mirrors the production boot sequence (`serve.rs`): without this call,
    // `KhiveRuntime`'s pack-installed entity-type validator (the
    // `create_many` defense-in-depth layer) is never wired, so a test built
    // on a fixture that skips it would still pass even if that runtime-layer
    // aggregate were absent or builtin-only (PR #925).
    registry.call_register_entity_type_validators(&rt);
    registry.apply_schema_plans(rt.backend());
    (rt, token, registry)
}

async fn create(registry: &VerbRegistry, body: Value) -> Uuid {
    let resp = registry.dispatch("create", body).await.expect("create ok");
    Uuid::parse_str(resp["id"].as_str().expect("id present")).expect("id is uuid")
}

async fn incoming_annotating_ids(
    registry: &VerbRegistry,
    target_id: Uuid,
) -> std::collections::BTreeSet<String> {
    registry
        .dispatch(
            "neighbors",
            json!({
                "id": target_id.to_string(),
                "direction": "incoming",
                "relations": ["annotates"]
            }),
        )
        .await
        .expect("target neighbors")
        .as_array()
        .expect("neighbor array")
        .iter()
        .map(|neighbor| {
            assert_eq!(neighbor["kind"], "commit");
            neighbor["id"].as_str().expect("commit id").to_string()
        })
        .collect()
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

/// Absolute path of the real `git` binary, resolved BEFORE any test shadows
/// `PATH` — shims delegate every invocation they do not script to it (same
/// technique as `src/recovery_tests.rs`).
fn resolve_real_git() -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg("command -v git")
        .output()
        .expect("resolve real git");
    assert!(out.status.success(), "could not resolve real git on PATH");
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

struct HeldFailCommitEmbedService {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

#[async_trait]
impl EmbeddingService for HeldFailCommitEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts
            .iter()
            .any(|text| text.contains("held-failing-commit"))
        {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                let _ = release.await;
            }
            return Err(EmbedError::InferenceFailed(
                "held commit embedding failed".into(),
            ));
        }
        Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "held-failing-commit-embedder"
    }
}

struct HeldFailCommitEmbedderProvider(Arc<HeldFailCommitEmbedService>);

#[async_trait]
impl EmbedderProvider for HeldFailCommitEmbedderProvider {
    fn name(&self) -> &str {
        "held-failing-commit-embedder"
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.0.clone())
    }
}

struct BothLookupsCommitEmbedService {
    before_create: tokio::sync::Barrier,
    arrivals: AtomicUsize,
}

#[async_trait]
impl EmbeddingService for BothLookupsCommitEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts
            .iter()
            .any(|text| text.contains("Shared overlapping commit"))
        {
            // Ingest invokes the embedder only after its SHA lookup, but
            // before the keyed create. Both passes must reach this point.
            self.arrivals.fetch_add(1, Ordering::SeqCst);
            tokio::time::timeout(std::time::Duration::from_secs(5), self.before_create.wait())
                .await
                .map_err(|_| EmbedError::Internal("second digest did not reach create".into()))?;
        }
        Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "both-lookups-commit-embedder"
    }
}

struct BothLookupsCommitEmbedderProvider(Arc<BothLookupsCommitEmbedService>);

#[async_trait]
impl EmbedderProvider for BothLookupsCommitEmbedderProvider {
    fn name(&self) -> &str {
        "both-lookups-commit-embedder"
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.0.clone())
    }
}
