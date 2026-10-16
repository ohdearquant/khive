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
    review_decision: Value,
    decision_head: Option<String>,
}
impl Recording {
    fn login(token: &str) -> &str {
        if token.contains("third") {
            "third"
        } else if token.contains("reviewer") {
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
    async fn review_decision(
        &self,
        token: &str,
        slug: &str,
        number: u64,
    ) -> Result<Value, RemoteError> {
        assert_eq!(slug, SLUG);
        let mut s = self.state.lock().unwrap();
        s.calls.push(json!({"op":"review_decision","number":number,
            "token_hash":blake3::hash(token.as_bytes()).to_hex().to_string()}));
        if s.api_failure {
            return Err(RemoteError::Unavailable);
        }
        // The separate field models a head moving between REST and GraphQL.
        let head = s
            .decision_head
            .as_ref()
            .map_or_else(|| s.prs[&number]["head"]["sha"].clone(), |head| json!(head));
        Ok(json!({"reviewDecision":s.review_decision,"headRefOid":head}))
    }
    async fn remote_ref(
        &self,
        token: Option<&str>,
        _remote: &str,
        _branch: &str,
    ) -> Result<Option<String>, RemoteError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(json!({"op":"remote_ref","token_hash":blake3::hash(token.expect("platform credential").as_bytes()).to_hex().to_string()}));
        if s.api_failure {
            return Err(RemoteError::Unavailable);
        }
        Ok(remote_head(&self.bare))
    }
    async fn push(&self, token: Option<&str>, mut request: PushRequest) -> Result<(), RemoteError> {
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
        crate::remote_transport::push_native(Path::new("git"), token, request, true).await?;
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
            let pr = json!({"number":n,"html_url":format!("https://github.com/{SLUG}/pull/{n}"),"state":"open","merged":false,"head":{"sha":s.head,"ref":"work","repo":{"full_name":if s.fork{"fork/project"}else{SLUG}}},"base":{"repo":{"full_name":SLUG}},"user":{"login":s.author}});
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
    third: String,
    rt: KhiveRuntime,
    registry: VerbRegistry,
    remote: Arc<Recording>,
}
impl Fixture {
    async fn new(allowed: bool, fault: Option<&str>) -> Self {
        Self::with_merge_refusals(allowed, fault, &[]).await
    }
    async fn with_merge_refusals(
        allowed: bool,
        fault: Option<&str>,
        merge_refusals: &[&str],
    ) -> Self {
        Self::with_program(allowed, fault, merge_refusals, || None).await
    }

    async fn with_program(
        allowed: bool,
        fault: Option<&str>,
        merge_refusals: &[&str],
        program: impl FnOnce() -> Option<PathBuf>,
    ) -> Self {
        let env_guard = crate::cache::ENV_MUTEX.lock().await;
        let program = program();
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
            ("third-ref", "synthetic-third-secret"),
        ] {
            std::fs::write(dir.path().join(reference), value).unwrap();
        }
        let actor = format!("remote:{}", uuid::Uuid::new_v4());
        let reviewer = format!("{actor}:reviewer");
        let alias = format!("{actor}:alias");
        let third = format!("{actor}:third");
        let mut actors = BTreeMap::new();
        for (actor, reference, login) in [
            (&actor, "author-ref", "author"),
            (&reviewer, "reviewer-ref", "reviewer"),
            (&alias, "alias-ref", "author"),
            (&third, "third-ref", "third"),
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
                program,
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
                        merge_refusals: merge_refusals.iter().map(|e| e.to_string()).collect(),
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
                review_decision: json!("APPROVED"),
                decision_head: None,
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
            third,
            rt,
            registry,
            remote,
        };
        for actor in [&f.actor, &f.reviewer, &f.alias, &f.third] {
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
        let reply = self
            .call(
                &self.actor,
                "git.pr_open",
                json!({"head":"work","base":"main","title":"Fixture","body":"","expected_head":self.head}),
            )
            .await;
        if let Err(error) = &reply {
            let receipt = self.last(&self.actor).await;
            eprintln!(
                "remote fixture refusal: {error}; receipt_actor={:?} reason={:?} table={:?} key={:?} cause={:?}",
                receipt.actor,
                receipt.reason,
                receipt.result["refusal"]["table"],
                receipt.result["refusal"]["key"],
                receipt.result["refusal"]["cause"],
            );
        }
        reply.unwrap()["number"].as_u64().unwrap()
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

include!("remote_tests/review_provenance_tests.rs");

include!("remote_tests/push_admission_tests.rs");

include!("remote_tests/pull_request_decisions_tests.rs");

include!("remote_tests/outcome_reconciliation_tests.rs");

include!("remote_tests/schema_and_effect_controls_tests.rs");
