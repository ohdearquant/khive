// The remote-head and platform-account mutation controls are declared in the
// implementation plan before execution. Native Git refs and SQL receipt reads
// are independent of the recording platform's request ledger.

use crate::receipts::{self, Disposition, Receipt};
use crate::remote_transport::{ApiRequest, PushRequest, RemoteError, RemoteTransport};
use crate::GitPack;
use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{
    GitWriteActorConfig, GitWriteEntryConfig, GitWriteRepositoryConfig, GitWriteSectionConfig,
};
use khive_runtime::{
    KhiveRuntime, RequestIdentity, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const SLUG: &str = "fixture/project";
const REMOTE: &str = "https://github.com/fixture/project.git";
const SECRET: &str = "synthetic-author-secret";
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "native Git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn remote_head(remote: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(remote)
        .args(["rev-parse", "--verify", "refs/heads/work"])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).unwrap().trim().into())
}
fn quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', "'\\''"))
}

struct Recording {
    bare: PathBuf,
    state: Mutex<State>,
}
struct State {
    head: String,
    author: String,
    fork: bool,
    mismatch: Option<&'static str>,
    prs: BTreeMap<u64, Value>,
    reviews: Vec<Value>,
    calls: Vec<Value>,
    push_calls: usize,
    writes: usize,
    remote_race: Option<String>,
    merge_race: Option<String>,
    lost_ack: bool,
    api_failure: bool,
    malformed_merge_reply: bool,
}
impl Recording {
    fn login(token: &str) -> &str {
        if token.contains("reviewer") {
            "reviewer"
        } else {
            "author"
        }
    }
    fn pr(&self, n: u64) -> Value {
        self.state.lock().unwrap().prs[&n].clone()
    }
    fn writes(&self) -> usize {
        self.state.lock().unwrap().writes
    }
}
#[async_trait]
impl RemoteTransport for Recording {
    async fn remote_ref(
        &self,
        token: &str,
        _remote: &str,
        _branch: &str,
    ) -> Result<Option<String>, RemoteError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(json!({"op":"remote_ref","token_hash":blake3::hash(token.as_bytes()).to_hex().to_string()}));
        if s.api_failure {
            return Err(RemoteError::Unavailable);
        }
        Ok(remote_head(&self.bare))
    }
    async fn push(&self, token: &str, mut request: PushRequest) -> Result<(), RemoteError> {
        let (race, lost) = {
            let mut s = self.state.lock().unwrap();
            s.push_calls += 1;
            s.calls.push(json!({"op":"push","sha":request.expected_local,"expected_remote":request.expected_remote}));
            (s.remote_race.take(), s.lost_ack)
        };
        if let Some(sha) = race {
            git(&self.bare, &["update-ref", "refs/heads/work", &sha]);
        }
        request.remote = self.bare.display().to_string();
        crate::remote_transport::push_native(token, request, true).await?;
        self.state.lock().unwrap().writes += 1;
        if lost {
            Err(RemoteError::Unknown)
        } else {
            Ok(())
        }
    }
    async fn api(&self, token: &str, request: ApiRequest) -> Result<Value, RemoteError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(json!({"method":request.method,"path":request.path,"body":request.body,"token_hash":blake3::hash(token.as_bytes()).to_hex().to_string()}));
        if s.api_failure {
            return Err(RemoteError::Unavailable);
        }
        let path = request.path.as_str();
        if path == "user" {
            return Ok(json!({"login":Self::login(token)}));
        }
        if path == format!("repos/{SLUG}") {
            return Ok(
                json!({"full_name":if s.mismatch==Some("slug"){"wrong/repo"}else{SLUG},"visibility":if s.mismatch==Some("visibility"){"public"}else{"private"}}),
            );
        }
        if path.contains("/git/ref/heads/") {
            return Ok(json!({"object":{"sha":s.head}}));
        }
        if path == format!("repos/{SLUG}/pulls") && request.method == "POST" {
            let n = s.prs.len() as u64 + 1;
            let pr = json!({"number":n,"html_url":format!("https://github.com/{SLUG}/pull/{n}"),"state":"open","merged":false,"head":{"sha":s.head,"repo":{"full_name":if s.fork{"fork/project"}else{SLUG}}},"base":{"repo":{"full_name":SLUG}},"user":{"login":s.author}});
            s.prs.insert(n, pr.clone());
            s.writes += 1;
            return Ok(pr);
        }
        if path.contains("/reviews?") {
            return Ok(json!(s.reviews));
        }
        let part = path
            .strip_prefix(&format!("repos/{SLUG}/pulls/"))
            .ok_or(RemoteError::InvalidResponse)?;
        let n = part
            .split('/')
            .next()
            .unwrap()
            .parse::<u64>()
            .map_err(|_| RemoteError::InvalidResponse)?;
        if part.ends_with("/reviews") && request.method == "POST" {
            let b = request.body.unwrap();
            let state = match b["event"].as_str().unwrap() {
                "APPROVE" => "APPROVED",
                "REQUEST_CHANGES" => "CHANGES_REQUESTED",
                _ => "COMMENTED",
            };
            let review = json!({"id":s.reviews.len()+1,"commit_id":b["commit_id"],"state":state,"user":{"login":Self::login(token)}});
            s.reviews.push(review.clone());
            s.writes += 1;
            return Ok(review);
        }
        if part.ends_with("/merge") {
            if let Some(sha) = s.merge_race.take() {
                s.prs.get_mut(&n).unwrap()["head"]["sha"] = json!(sha);
            }
            let body = request.body.unwrap();
            let pr = s.prs.get_mut(&n).ok_or(RemoteError::Refused)?;
            if pr["head"]["sha"] != body["sha"] {
                return Err(RemoteError::Refused);
            }
            if pr["merged"] == true {
                return Err(RemoteError::Refused);
            }
            pr["merged"] = json!(true);
            pr["state"] = json!("closed");
            pr["merge_commit_sha"] = json!("d".repeat(40));
            s.writes += 1;
            if s.lost_ack {
                return Err(RemoteError::Unknown);
            }
            if s.malformed_merge_reply {
                return Ok(json!({"sha":"d".repeat(40)}));
            }
            return Ok(json!({"merged":true,"sha":"d".repeat(40)}));
        }
        s.prs.get(&n).cloned().ok_or(RemoteError::Refused)
    }
}

struct Fixture {
    _env_guard: tokio::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    repo: PathBuf,
    base: String,
    middle: String,
    head: String,
    actor: String,
    reviewer: String,
    alias: String,
    rt: KhiveRuntime,
    registry: VerbRegistry,
    remote: Arc<Recording>,
}
impl Fixture {
    async fn new(allowed: bool, fault: Option<&str>) -> Self {
        let env_guard = crate::cache::ENV_MUTEX.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap();
        git(&repo, &["init", "-q", "-b", "work"]);
        git(&repo, &["commit", "--allow-empty", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["commit", "--allow-empty", "-qm", "middle"]);
        let middle = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["commit", "--allow-empty", "-qm", "head"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let bare = dir.path().join("remote.git");
        git(
            &repo,
            &[
                "clone",
                "-q",
                "--bare",
                repo.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        git(&bare, &["update-ref", "refs/heads/work", &base]);
        let resolver = dir.path().join("resolver");
        std::fs::write(
            &resolver,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" >> {}\nexec /bin/cat {}/\"$1\"\n",
                quote(&dir.path().join("reads")),
                quote(dir.path())
            ),
        )
        .unwrap();
        std::fs::set_permissions(&resolver, std::fs::Permissions::from_mode(0o755)).unwrap();
        for (reference, value) in [
            ("author-ref", SECRET),
            ("reviewer-ref", "synthetic-reviewer-secret"),
            ("alias-ref", "synthetic-alias-secret"),
        ] {
            std::fs::write(dir.path().join(reference), value).unwrap();
        }
        let actor = format!("remote:{}", uuid::Uuid::new_v4());
        let reviewer = format!("{actor}:reviewer");
        let alias = format!("{actor}:alias");
        let mut actors = BTreeMap::new();
        for (actor, reference, login) in [
            (&actor, "author-ref", "author"),
            (&reviewer, "reviewer-ref", "reviewer"),
            (&alias, "alias-ref", "author"),
        ] {
            actors.insert(
                actor.clone(),
                GitWriteActorConfig {
                    name: "Fixture".into(),
                    email: "fixture@example.invalid".into(),
                    credential_ref: reference.into(),
                    platform_identity: login.into(),
                },
            );
        }
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            git_write: GitWriteSectionConfig {
                allowed: if allowed {
                    vec![GitWriteEntryConfig {
                        repo: repo.display().to_string(),
                        branches: vec!["work".into()],
                    }]
                } else {
                    vec![]
                },
                actors,
                repositories: BTreeMap::from([(
                    repo.display().to_string(),
                    GitWriteRepositoryConfig {
                        remote: REMOTE.into(),
                        slug: SLUG.into(),
                        visibility: "private".into(),
                    },
                )]),
                credential_resolver: vec![resolver.display().to_string(), "{ref}".into()],
                contract_faults: fault.is_some(),
                fault: fault.map(str::to_string),
            },
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let remote = Arc::new(Recording {
            bare,
            state: Mutex::new(State {
                head: head.clone(),
                author: "author".into(),
                fork: false,
                mismatch: None,
                prs: BTreeMap::new(),
                reviews: vec![],
                calls: vec![],
                push_calls: 0,
                writes: 0,
                remote_race: None,
                merge_race: None,
                lost_ack: false,
                api_failure: false,
                malformed_merge_reply: false,
            }),
        });
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(actor.clone()));
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        builder.register(GitPack::with_remote_transport(rt.clone(), remote.clone()));
        builder.with_runtime_event_store(&rt).unwrap();
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        let f = Self {
            _env_guard: env_guard,
            dir,
            repo,
            base,
            middle,
            head,
            actor,
            reviewer,
            alias,
            rt,
            registry,
            remote,
        };
        for actor in [&f.actor, &f.reviewer, &f.alias] {
            for verb in [
                "git.push",
                "git.pr_open",
                "git.pr_review",
                "git.pr_merge",
                "git.reconcile",
            ] {
                f.policy(actor, verb, "allow").await;
            }
        }
        f
    }
    async fn call(&self, actor: &str, verb: &str, mut p: Value) -> Result<Value, RuntimeError> {
        if verb != "git.reconcile" {
            p["repo"] = json!(self.repo);
        }
        self.registry
            .dispatch_with_identity(
                verb,
                p,
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: Some(actor.into()),
                    ..Default::default()
                }),
            )
            .await
    }
    async fn policy(&self, actor: &str, verb: &str, decision: &str) -> String {
        let mut writer = self.rt.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: "DELETE FROM tool_policy WHERE namespace='local' AND actor=?1 AND tool=?2"
                    .into(),
                params: vec![SqlValue::Text(actor.into()), SqlValue::Text(verb.into())],
                label: Some("fixture_replace_policy".into()),
            })
            .await
            .unwrap();
        drop(writer);
        self.registry
            .dispatch(
                "tool.policy",
                json!({"actor":actor,"tool":verb,"decision":decision}),
            )
            .await
            .unwrap()["policy"]["id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn push(&self) -> Value {
        json!({"branch":"work","expected_local":self.head,"expected_remote":self.base})
    }
    fn merge(&self, n: u64) -> Value {
        json!({"number":n,"method":"merge","subject":"Fixture merge","body":"","expected_head":self.head})
    }
    async fn open(&self) -> u64 {
        self.call(&self.actor,"git.pr_open",json!({"head":"work","base":"main","title":"Fixture","body":"","expected_head":self.head})).await.unwrap()["number"].as_u64().unwrap()
    }
    async fn review(&self, actor: &str, n: u64) -> Result<Value, RuntimeError> {
        self.call(
            actor,
            "git.pr_review",
            json!({"number":n,"verdict":"approve","body":"","expected_head":self.head}),
        )
        .await
    }
    async fn last(&self, actor: &str) -> Receipt {
        receipts::list_owned(&self.rt, "local", actor, None, None, 500, 0)
            .await
            .unwrap()
            .receipts
            .pop()
            .unwrap()
    }
    async fn refusal(&self, actor: &str, verb: &str, p: Value, reason: &str) -> Receipt {
        let before = self.remote.writes();
        let local = git(&self.repo, &["show-ref"]);
        let remote = remote_head(&self.remote.bare);
        let error = self.call(actor, verb, p).await.unwrap_err().to_string();
        assert!(error.contains(reason), "{error}");
        let receipt = self.last(actor).await;
        assert_eq!(receipt.disposition, Disposition::NotCommitted);
        assert!(error.contains(&receipt.id));
        assert_eq!(self.remote.writes(), before);
        assert_eq!(git(&self.repo, &["show-ref"]), local);
        assert_eq!(remote_head(&self.remote.bare), remote);
        receipt
    }
}

#[tokio::test]
async fn remote_push_gate_policy_and_positive_control() {
    for (gate, decision) in [(false, "allow"), (true, "deny"), (true, "allow")] {
        let f = Fixture::new(gate, None).await;
        let id = f.policy(&f.actor, "git.push", decision).await;
        let before = crate::local_handlers::policy_check_count(&f.actor);
        if !gate || decision == "deny" {
            f.refusal(
                &f.actor,
                "git.push",
                f.push(),
                if gate {
                    "policy_denied"
                } else {
                    "not_configured"
                },
            )
            .await;
            assert_eq!(f.remote.state.lock().unwrap().push_calls, 0);
            assert!(f.remote.state.lock().unwrap().calls.is_empty());
        } else {
            let result = f.call(&f.actor, "git.push", f.push()).await.unwrap();
            assert_eq!(result["sha"], f.head);
            assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
        }
        assert_eq!(
            crate::local_handlers::policy_check_count(&f.actor) - before,
            usize::from(gate)
        );
        let receipt = f.last(&f.actor).await;
        assert_eq!(
            receipt.gate["decision"],
            if gate { "allow" } else { "deny" }
        );
        if gate {
            assert_eq!(receipt.policy["id"], id);
            assert_eq!(receipt.policy["decision"], decision);
        } else {
            assert!(receipt.policy.is_null());
        }
    }
}

#[tokio::test]
async fn remote_push_compares_at_local_remote_and_native_effect_seams() {
    for seam in ["local", "remote", "effect"] {
        let f = Fixture::new(true, None).await;
        if seam == "local" {
            git(&f.repo, &["update-ref", "refs/heads/work", &f.middle]);
        }
        if seam == "remote" {
            git(
                &f.remote.bare,
                &["update-ref", "refs/heads/work", &f.middle],
            );
        }
        if seam == "effect" {
            f.remote.state.lock().unwrap().remote_race = Some(f.middle.clone());
        }
        let result = f.call(&f.actor, "git.push", f.push()).await;
        assert!(result.is_err(), "{seam}: rival remote was overwritten");
        assert_eq!(
            f.last(&f.actor).await.disposition,
            Disposition::NotCommitted
        );
        assert_eq!(
            remote_head(&f.remote.bare),
            Some(if seam == "local" {
                f.base.clone()
            } else {
                f.middle.clone()
            })
        );
        assert_eq!(
            git(&f.repo, &["rev-parse", "work"]),
            if seam == "local" {
                f.middle.as_str()
            } else {
                f.head.as_str()
            }
        );
    }
}

#[tokio::test]
async fn remote_push_rejects_every_force_and_omission_before_network() {
    let f = Fixture::new(true, None).await;
    for extra in [
        json!({"force":true}),
        json!({"force":1}),
        json!({"force":"true"}),
        json!({"force":[]}),
        json!({"force_with_lease":true}),
        json!({"force_with_lease":false}),
        json!({"refspec":"+HEAD:refs/heads/main"}),
        json!({"argv":["--force"]}),
        json!({"remote":"elsewhere"}),
        json!({"actor":"spoof"}),
        json!({"credential":SECRET}),
    ] {
        let mut p = f.push();
        p.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        f.refusal(&f.actor, "git.push", p, "invalid_params").await;
    }
    let mut p = f.push();
    p.as_object_mut().unwrap().remove("expected_remote");
    f.refusal(&f.actor, "git.push", p, "expected_remote is required")
        .await;
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
}

#[tokio::test]
async fn remote_push_null_is_create_only_and_equal_sha_is_not_an_effect() {
    let f = Fixture::new(true, None).await;
    let mut p = f.push();
    p["expected_remote"] = Value::Null;
    f.refusal(&f.actor, "git.push", p.clone(), "expected_remote_mismatch")
        .await;
    git(&f.remote.bare, &["update-ref", "-d", "refs/heads/work"]);
    let refs = git(&f.repo, &["show-ref"]);
    f.call(&f.actor, "git.push", p.clone()).await.unwrap();
    assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
    assert_eq!(git(&f.repo, &["show-ref"]), refs);
    f.refusal(&f.actor, "git.push", p, "expected_remote_mismatch")
        .await;
    let mut p = f.push();
    p["expected_remote"] = json!(f.head);
    f.refusal(&f.actor, "git.push", p, "already_at_target")
        .await;
}

#[tokio::test]
async fn remote_pr_identity_mismatch_has_no_effect_and_open_binds_head() {
    let f = Fixture::new(true, None).await;
    let p = json!({"head":"work","base":"main","title":"Fixture","body":"","expected_head":f.head});
    for field in ["slug", "visibility"] {
        f.remote.state.lock().unwrap().mismatch = Some(field);
        f.refusal(
            &f.actor,
            "git.pr_open",
            p.clone(),
            "repository_identity_mismatch",
        )
        .await;
    }
    f.remote.state.lock().unwrap().mismatch = None;
    let result = f.call(&f.actor, "git.pr_open", p).await.unwrap();
    assert_eq!(result["head_sha"], f.head);
    assert_eq!(result["url"], f.remote.pr(1)["html_url"]);
}

#[tokio::test]
async fn remote_pr_self_actor_and_same_account_refuse_then_second_account_approves() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    for actor in [&f.actor, &f.alias] {
        let before = f.remote.writes();
        let review = f.review(actor, n).await;
        assert_eq!(
            f.remote.writes(),
            before,
            "unexpected platform review: {}",
            json!(f.remote.state.lock().unwrap().reviews)
        );
        assert!(review.unwrap_err().to_string().contains("self_approval"));
        assert_eq!(f.last(actor).await.disposition, Disposition::NotCommitted);
    }
    let result = f.review(&f.reviewer, n).await.unwrap();
    assert_eq!(result["head_sha"], f.head);
    assert_eq!(result["state"], "approved");
    let s = f.remote.state.lock().unwrap();
    assert_eq!(s.reviews[0]["commit_id"], f.head);
    assert_eq!(s.reviews[0]["user"]["login"], "reviewer");
}

#[tokio::test]
async fn remote_review_and_merge_head_races_preserve_rival() {
    for seam in ["review", "before-merge", "during-merge"] {
        let f = Fixture::new(true, None).await;
        let n = f.open().await;
        if seam != "review" {
            f.review(&f.reviewer, n).await.unwrap();
        }
        if seam == "during-merge" {
            f.remote.state.lock().unwrap().merge_race = Some(f.middle.clone());
        } else {
            f.remote.state.lock().unwrap().prs.get_mut(&n).unwrap()["head"]["sha"] =
                json!(f.middle);
        }
        let result = if seam == "review" {
            f.review(&f.reviewer, n).await
        } else {
            f.call(&f.actor, "git.pr_merge", f.merge(n)).await
        };
        assert!(result.is_err());
        assert_eq!(f.remote.pr(n)["head"]["sha"], f.middle);
        assert_eq!(f.remote.pr(n)["merged"], false);
    }
}

#[tokio::test]
async fn remote_merge_policy_and_second_account_review_are_both_required() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "missing_review")
        .await;
    f.review(&f.reviewer, n).await.unwrap();
    for decision in ["ask", "deny"] {
        f.policy(&f.actor, "git.pr_merge", decision).await;
        f.refusal(&f.actor, "git.pr_merge", f.merge(n), "policy_denied")
            .await;
    }
    let id = f.policy(&f.actor, "git.pr_merge", "allow").await;
    let result = f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(result["merged_head_sha"], f.head);
    assert_eq!(result["merged_sha"], f.remote.pr(n)["merge_commit_sha"]);
    assert_eq!(f.last(&f.actor).await.policy["id"], id);
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "pull_request_closed")
        .await;
}

#[tokio::test]
async fn remote_fork_approval_and_merge_require_named_rows() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().fork = true;
    let n = f.open().await;
    f.policy(&f.actor, "git.pr_merge.admin", "allow").await;
    for decision in ["ask", "deny"] {
        f.policy(&f.reviewer, "git.pr_review.fork", decision).await;
        assert!(f
            .review(&f.reviewer, n)
            .await
            .unwrap_err()
            .to_string()
            .contains("fork_policy_denied"));
    }
    let id = f.policy(&f.reviewer, "git.pr_review.fork", "allow").await;
    f.review(&f.reviewer, n).await.unwrap();
    assert_eq!(f.last(&f.reviewer).await.fork_policy["id"], id);
    for decision in ["ask", "deny"] {
        f.policy(&f.actor, "git.pr_merge.fork", decision).await;
        f.refusal(&f.actor, "git.pr_merge", f.merge(n), "fork_policy_denied")
            .await;
    }
    let id = f.policy(&f.actor, "git.pr_merge.fork", "allow").await;
    f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
    assert_eq!(f.last(&f.actor).await.fork_policy["id"], id);
    assert!(!json!(f.remote.state.lock().unwrap().calls)
        .to_string()
        .contains("admin"));
}

#[tokio::test]
async fn remote_rotated_credential_is_resolved_once_and_never_stored() {
    for error in [false, true] {
        let f = Fixture::new(true, None).await;
        let rotated = format!("{SECRET}-rotated");
        std::fs::write(f.dir.path().join("author-ref"), &rotated).unwrap();
        f.remote.state.lock().unwrap().api_failure = error;
        let reply = f.call(&f.actor, "git.push", f.push()).await;
        if error {
            assert!(reply.is_err());
        } else {
            assert!(reply.is_ok());
        }
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("reads")).unwrap(),
            "author-ref\n"
        );
        let mut reader = f.rt.sql().reader().await.unwrap();
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM git_receipts".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        let receipt = f.last(&f.actor).await;
        let calls = f.remote.state.lock().unwrap().calls.clone();
        let raw = format!("{reply:?} {rows:?} {} {calls:?}", receipt.to_value());
        assert!(!raw.contains(SECRET));
        assert!(!raw.contains("GH_TOKEN="));
        assert!(!raw.contains("Authorization: Bearer "));
        assert_eq!(
            calls[0]["token_hash"],
            blake3::hash(rotated.as_bytes()).to_hex().to_string()
        );
    }
}

#[tokio::test]
async fn remote_lost_ack_without_marker_stays_unknown_and_never_retries() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().lost_ack = true;
    assert!(f.call(&f.actor, "git.push", f.push()).await.is_err());
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(remote_head(&f.remote.bare), Some(f.head.clone()));
    let result = f
        .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(result["receipt"]["disposition"], "unknown");
    assert_eq!(f.remote.state.lock().unwrap().push_calls, 1);
    f.refusal(&f.actor, "git.push", f.push(), "expected_remote_mismatch")
        .await;
}

#[tokio::test]
async fn remote_unsupported_git_is_receipted_before_credentials_or_network() {
    struct RestorePath(std::ffi::OsString);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            std::env::set_var("PATH", &self.0);
        }
    }
    let f = Fixture::new(true, None).await;
    let old_path = std::env::var_os("PATH").unwrap();
    let git_path = Command::new("/usr/bin/which").arg("git").output().unwrap();
    assert!(git_path.status.success());
    let git_path = PathBuf::from(String::from_utf8(git_path.stdout).unwrap().trim());
    let bin = f.dir.path().join("old-git");
    std::fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    std::fs::write(&wrapper, format!("#!/bin/sh\ncase \"$*\" in\n*' --version') echo 'git version 2.40.0'; exit 0;;\n*' reflog -h') echo 'usage: git reflog show'; exit 129;;\nesac\nexec {} \"$@\"\n", quote(&git_path))).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&old_path));
    let _restore = RestorePath(old_path);
    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
    f.refusal(&f.actor, "git.push", f.push(), "unsupported_toolchain")
        .await;
    let receipt = f.last(&f.actor).await;
    assert_eq!(
        receipt.result["toolchain"]["git_version"],
        "git version 2.40.0"
    );
    assert_eq!(
        receipt.result["toolchain"]["missing_capability"],
        "reflog write"
    );
    assert!(receipt.credential.is_null());
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
    assert_eq!(remote_head(&f.remote.bare), Some(f.base.clone()));
}

#[tokio::test]
async fn remote_malformed_merge_reply_stays_unknown_until_readback() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    f.remote.state.lock().unwrap().malformed_merge_reply = true;
    assert!(f.call(&f.actor, "git.pr_merge", f.merge(n)).await.is_err());
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Unknown);
    assert_eq!(f.remote.pr(n)["merged"], true);
    let writes = f.remote.writes();
    let result = f
        .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(result["receipt"]["disposition"], "committed");
    assert_eq!(f.remote.writes(), writes);
}

#[cfg(feature = "contract-faults")]
#[tokio::test]
async fn remote_post_effect_faults_reconcile_without_repeating_native_write() {
    for verb in ["git.push", "git.pr_merge"] {
        for point in ["reply-lost-after-effect", "audit-fails-after-effect"] {
            let fault = format!("{verb}:{point}");
            let f = Fixture::new(true, Some(&fault)).await;
            let mut params = if verb == "git.push" {
                f.push()
            } else {
                let n = f.open().await;
                f.review(&f.reviewer, n).await.unwrap();
                f.merge(n)
            };
            // Accepted hexadecimal spelling must not obstruct later reconciliation.
            let compare = if verb == "git.push" {
                "expected_local"
            } else {
                "expected_head"
            };
            params[compare] = json!(f.head.to_ascii_uppercase());
            assert!(f.call(&f.actor, verb, params.clone()).await.is_err());
            let receipt = f.last(&f.actor).await;
            assert_eq!(
                receipt.disposition,
                if point == "audit-fails-after-effect" {
                    Disposition::Committed
                } else {
                    Disposition::Unknown
                }
            );
            let writes = f.remote.writes();
            let result = f
                .call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
                .await
                .unwrap();
            assert_eq!(
                result["receipt"]["disposition"], "committed",
                "{verb}:{point}: {result}"
            );
            assert_eq!(f.remote.writes(), writes);
            assert!(f.call(&f.actor, verb, params).await.is_err());
            assert_eq!(f.remote.writes(), writes);
        }
    }
}

#[tokio::test]
async fn remote_pack_schema_covers_all_verbs_with_required_nullable_compare() {
    let f = Fixture::new(true, None).await;
    for verb in [
        "git.digest",
        "git.branch",
        "git.commit",
        "git.push",
        "git.checkout",
        "git.diff",
        "git.receipts",
        "git.gates",
        "git.reconcile",
        "git.pr_open",
        "git.pr_review",
        "git.pr_merge",
    ] {
        let help = f.registry.describe_verb(verb).unwrap();
        assert_eq!(help["input_schema"]["type"], "object");
        assert_eq!(help["input_schema"]["additionalProperties"], false);
    }
    let help = f.registry.describe_verb("git.push").unwrap();
    let schema = &help["input_schema"];
    assert!(schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("expected_remote")));
    assert_eq!(
        schema["properties"]["expected_remote"]["type"],
        json!(["string", "null"])
    );
    assert!(help["params"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["name"] == "expected_remote"
            && p["type"] == "string|null"
            && p["required"] == true));
}

#[tokio::test]
async fn remote_push_refuses_remote_move_at_effect_control() {
    let f = Fixture::new(true, None).await;
    f.remote.state.lock().unwrap().remote_race = Some(f.middle.clone());
    let result = f.call(&f.actor, "git.push", f.push()).await;
    assert_eq!(
        remote_head(&f.remote.bare),
        Some(f.middle.clone()),
        "candidate={} result={result:?}",
        f.head
    );
    assert!(
        result.is_err(),
        "removed compare permitted a push over the rival ref"
    );
    assert_eq!(
        f.last(&f.actor).await.disposition,
        Disposition::NotCommitted
    );
}

#[tokio::test]
async fn remote_merge_grants_bind_actor_tool_and_expiry() {
    for condition in ["expired", "foreign", "wrong-tool", "valid"] {
        let f = Fixture::new(true, None).await;
        let n = f.open().await;
        f.review(&f.reviewer, n).await.unwrap();
        f.policy(&f.actor, "git.pr_merge", "ask").await;
        let actor = if condition == "foreign" {
            &f.alias
        } else {
            &f.actor
        };
        let tool = if condition == "wrong-tool" {
            "git.pr_open"
        } else {
            "git.pr_merge"
        };
        f.policy(actor, tool, "ask").await;
        let request=f.registry.dispatch("tool.request",json!({"actor":actor,"tool":tool,"scope":"free text, not an authorization constraint"})).await.unwrap();
        let id = request["request_id"].as_str().unwrap();
        let mut grant = json!({"id":id});
        if condition == "expired" {
            grant["expires_in_s"] = json!(0);
        }
        f.registry
            .dispatch_with_identity(
                "tool.grant",
                grant,
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: Some(format!("{}:operator", f.actor)),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        if condition == "valid" {
            f.call(&f.actor, "git.pr_merge", f.merge(n)).await.unwrap();
            let receipt = f.last(&f.actor).await;
            assert_eq!(receipt.policy["source"], "grant");
            assert_eq!(receipt.policy["id"], id);
        } else {
            f.refusal(&f.actor, "git.pr_merge", f.merge(n), "policy_denied")
                .await;
        }
    }
}

#[tokio::test]
async fn remote_push_non_fast_forward_and_unmapped_actor_refuse() {
    let f = Fixture::new(true, None).await;
    let unmapped = format!("{}:unmapped", f.actor);
    f.policy(&unmapped, "git.push", "allow").await;
    f.refusal(&unmapped, "git.push", f.push(), "actor_unmapped")
        .await;
    assert!(f.remote.state.lock().unwrap().calls.is_empty());
    git(&f.remote.bare, &["update-ref", "refs/heads/work", &f.head]);
    git(&f.repo, &["update-ref", "refs/heads/work", &f.base]);
    f.refusal(
        &f.actor,
        "git.push",
        json!({"branch":"work","expected_local":f.base,"expected_remote":f.head}),
        "non_fast_forward",
    )
    .await;
    assert_eq!(f.remote.state.lock().unwrap().push_calls, 0);
}

#[tokio::test]
async fn remote_stale_or_dismissed_approvals_do_not_authorize_merge() {
    let f = Fixture::new(true, None).await;
    let n = f.open().await;
    f.review(&f.reviewer, n).await.unwrap();
    f.remote.state.lock().unwrap().reviews[0]["commit_id"] = json!(f.middle);
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "missing_review")
        .await;
    f.remote.state.lock().unwrap().reviews[0]["commit_id"] = json!(f.head);
    f.remote
        .state
        .lock()
        .unwrap()
        .reviews
        .push(json!({"id":2,"commit_id":f.head,"state":"DISMISSED","user":{"login":"reviewer"}}));
    f.refusal(&f.actor, "git.pr_merge", f.merge(n), "missing_review")
        .await;
}
