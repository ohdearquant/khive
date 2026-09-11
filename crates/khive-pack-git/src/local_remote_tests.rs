use crate::receipts::{self, Disposition, Receipt};
use crate::GitPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{
    GitWriteActorConfig, GitWriteEntryConfig, GitWriteRepositoryConfig, GitWriteSectionConfig,
};
use khive_runtime::{
    KhiveRuntime, RequestIdentity, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
        .replace('%', "%25")
        .replace(' ', "%20")
}

struct Fixture {
    _env_guard: tokio::sync::MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    repo: PathBuf,
    platform_repo: PathBuf,
    bare: PathBuf,
    base: String,
    head: String,
    rival: String,
    actor: String,
    rt: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    async fn new(remote: impl FnOnce(&Path) -> String, slug: &str) -> Self {
        let env_guard = crate::cache::ENV_MUTEX.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let platform_repo = dir.path().join("platform");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&platform_repo).unwrap();
        let repo = std::fs::canonicalize(repo).unwrap();
        let platform_repo = std::fs::canonicalize(platform_repo).unwrap();
        git(&repo, &["init", "-q", "--template=", "-b", "work"]);
        git(&repo, &["commit", "--allow-empty", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["commit", "--allow-empty", "-qm", "head"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
        let rival = git(&repo, &["commit-tree", &tree, "-p", &base, "-m", "rival"]);
        git(&repo, &["update-ref", "refs/heads/rival", &rival]);
        let bare = dir.path().join("remote % space.git");
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
        let bare = std::fs::canonicalize(bare).unwrap();
        git(&bare, &["update-ref", "refs/heads/work", &base]);

        // Any accidental resolver use both leaves evidence and refuses the operation.
        let resolver = dir.path().join("resolver");
        let marker = dir.path().join("credential-read");
        let quoted = format!("'{}'", marker.display().to_string().replace('\'', "'\\''"));
        std::fs::write(
            &resolver,
            format!("#!/bin/sh\nprintf invoked > {quoted}\nexit 91\n"),
        )
        .unwrap();
        std::fs::set_permissions(&resolver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let actor = format!("local-remote:{}", uuid::Uuid::new_v4());
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            git_write: GitWriteSectionConfig {
                allowed: [&repo, &platform_repo]
                    .into_iter()
                    .map(|repo| GitWriteEntryConfig {
                        repo: repo.display().to_string(),
                        branches: vec!["work".into(), "new".into()],
                    })
                    .collect(),
                actors: BTreeMap::from([(
                    actor.clone(),
                    GitWriteActorConfig {
                        name: "Fixture".into(),
                        email: "fixture@example.invalid".into(),
                        credential_ref: "must-not-resolve".into(),
                        platform_identity: "fixture".into(),
                    },
                )]),
                repositories: BTreeMap::from([
                    (
                        repo.display().to_string(),
                        GitWriteRepositoryConfig {
                            remote: remote(&bare),
                            slug: slug.into(),
                            visibility: "private".into(),
                            merge_refusals: vec![],
                        },
                    ),
                    (
                        platform_repo.display().to_string(),
                        GitWriteRepositoryConfig {
                            remote: "https://github.com/fixture/project.git".into(),
                            slug: "fixture/project".into(),
                            visibility: "private".into(),
                            merge_refusals: vec![],
                        },
                    ),
                ]),
                credential_resolver: vec![resolver.display().to_string(), "{ref}".into()],
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(actor.clone()));
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        builder.register(GitPack::new(rt.clone()));
        builder.with_runtime_event_store(&rt).unwrap();
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        for caller in [&actor, &format!("{actor}:unmapped")] {
            for verb in [
                "git.push",
                "git.pr_open",
                "git.pr_review",
                "git.pr_merge",
                "git.gates",
                "git.reconcile",
            ] {
                registry
                    .dispatch(
                        "tool.policy",
                        json!({"actor":caller,"tool":verb,"decision":"allow"}),
                    )
                    .await
                    .unwrap();
            }
        }
        Self {
            _env_guard: env_guard,
            dir,
            repo,
            platform_repo,
            bare,
            base,
            head,
            rival,
            actor,
            rt,
            registry,
        }
    }
    async fn file() -> Self {
        Self::new(file_url, "").await
    }
    async fn call(
        &self,
        actor: &str,
        verb: &str,
        mut params: Value,
    ) -> Result<Value, RuntimeError> {
        if verb != "git.reconcile" && params.get("repo").is_none() {
            params["repo"] = json!(self.repo);
        }
        self.registry
            .dispatch_with_identity(
                verb,
                params,
                Some(RequestIdentity {
                    namespace: "local".into(),
                    actor_id: Some(actor.into()),
                    ..Default::default()
                }),
            )
            .await
    }
    fn push(&self) -> Value {
        json!({"branch":"work","expected_local":self.head,"expected_remote":self.base})
    }
    fn remote_head(&self) -> String {
        let out = Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("--git-dir")
            .arg(&self.bare)
            .args(["rev-parse", "--verify", "refs/heads/work"])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn no_credential_read(&self) {
        assert!(!self.dir.path().join("credential-read").exists());
    }
    async fn last(&self, actor: &str) -> Receipt {
        receipts::list_owned(&self.rt, "local", actor, None, None, 500, 0)
            .await
            .unwrap()
            .receipts
            .pop()
            .unwrap()
    }
    async fn refusal(&self, verb: &str, params: Value, reason: &str) -> Receipt {
        let before = self.remote_head();
        let local = git(&self.repo, &["show-ref"]);
        let error = self
            .call(&self.actor, verb, params)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{error}");
        let receipt = self.last(&self.actor).await;
        assert!(error.contains(&receipt.id));
        assert_eq!(receipt.reason.as_deref(), Some(reason));
        assert_eq!(receipt.disposition, Disposition::NotCommitted);
        assert_eq!(self.remote_head(), before);
        assert_eq!(git(&self.repo, &["show-ref"]), local);
        self.no_credential_read();
        receipt
    }
}

#[tokio::test]
async fn local_file_push_moves_native_ref_without_credentials() {
    let f = Fixture::file().await;
    assert_eq!(f.remote_head(), f.base);
    let reply = f.call(&f.actor, "git.push", f.push()).await.unwrap();
    assert_eq!(f.remote_head(), f.head);
    assert_eq!(reply["sha"], f.head);
    let receipt = f.last(&f.actor).await;
    assert_eq!(receipt.disposition, Disposition::Committed);
    assert_eq!(receipt.credential, json!({"source":"none"}));
    assert_eq!(receipt.actor, f.actor);
    assert_eq!(receipt.gate["decision"], "allow");
    assert_eq!(receipt.policy["decision"], "allow");
    assert!(
        crate::local_git::operation_recorded(&f.repo, "work", &f.head, &receipt.id)
            .await
            .unwrap()
    );
    f.no_credential_read();
}

#[tokio::test]
async fn local_push_leaves_destination_hooks_unrun() {
    let f = Fixture::file().await;
    let marker = f.dir.path().join("hook-ran");
    let quoted = format!("'{}'", marker.display().to_string().replace('\'', "'\\''"));
    let hook = f.bare.join("hooks").join("update");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, format!("#!/bin/sh\nprintf ran > {quoted}\nexit 0\n")).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    f.call(&f.actor, "git.push", f.push()).await.unwrap();
    assert_eq!(f.remote_head(), f.head);
    assert!(
        !marker.exists(),
        "the destination update hook ran during the daemon's push"
    );
    // Control: a plain push with the same client-side hooks path runs the destination hook,
    // so its silence above is the transport's doing and not the hook's.
    git(
        &f.repo,
        &[
            "push",
            "-q",
            f.bare.to_str().unwrap(),
            &format!("{}:refs/heads/control", f.head),
        ],
    );
    assert!(
        marker.exists(),
        "control: a plain push must run the destination update hook"
    );
    f.no_credential_read();
}

#[tokio::test]
async fn local_path_push_allows_unmapped_actor_and_absent_remote_branch() {
    let f = Fixture::new(|bare| bare.display().to_string(), "").await;
    let actor = format!("{}:unmapped", f.actor);
    git(&f.repo, &["branch", "new", &f.head]);
    let params = json!({"branch":"new","expected_local":f.head,"expected_remote":null});
    f.call(&actor, "git.push", params).await.unwrap();
    assert_eq!(git(&f.bare, &["rev-parse", "refs/heads/new"]), f.head);
    assert_eq!(f.last(&actor).await.credential, json!({"source":"none"}));
    f.no_credential_read();
}

#[tokio::test]
async fn local_push_stale_expected_remote_preserves_refs() {
    let f = Fixture::file().await;
    let receipt = f
        .refusal(
            "git.push",
            json!({"branch":"work","expected_local":f.head,"expected_remote":f.rival}),
            "expected_remote_mismatch",
        )
        .await;
    assert_eq!(receipt.credential, json!({"source":"none"}));
    f.refusal(
        "git.push",
        json!({"branch":"work","expected_local":f.head,"expected_remote":null}),
        "expected_remote_mismatch",
    )
    .await;
}

#[tokio::test]
async fn local_push_divergent_remote_preserves_refs() {
    let f = Fixture::file().await;
    git(&f.bare, &["update-ref", "refs/heads/work", &f.rival]);
    f.refusal(
        "git.push",
        json!({"branch":"work","expected_local":f.head,"expected_remote":f.rival}),
        "non_fast_forward",
    )
    .await;
}

#[tokio::test]
async fn local_pr_verbs_refuse_remote_scheme_before_credentials() {
    let f = Fixture::file().await;
    for (verb, params) in [
        (
            "git.pr_open",
            json!({"head":"work","base":"main","title":"Fixture","body":"","expected_head":f.head}),
        ),
        (
            "git.pr_review",
            json!({"number":1,"verdict":"approve","body":"","expected_head":f.head}),
        ),
        (
            "git.pr_merge",
            json!({"number":1,"method":"merge","subject":"Fixture","body":"","expected_head":f.head}),
        ),
    ] {
        let receipt = f.refusal(verb, params, "remote_scheme").await;
        assert_eq!(receipt.credential, json!({"source":"none"}));
    }
}

#[tokio::test]
async fn local_gates_distinguish_https_platform_in_same_config() {
    let f = Fixture::file().await;
    let before = receipts::list_owned(&f.rt, "local", &f.actor, None, None, 500, 0)
        .await
        .unwrap()
        .receipts
        .len();
    let local = f.call(&f.actor, "git.gates", json!({})).await.unwrap();
    let platform = f
        .call(&f.actor, "git.gates", json!({"repo":f.platform_repo}))
        .await
        .unwrap();
    assert_eq!(
        local["target"],
        json!({"kind":"local","remote":file_url(&f.bare),"slug":"","visibility":"private"})
    );
    assert_eq!(
        platform["target"],
        json!({"kind":"platform","remote":"https://github.com/fixture/project.git","slug":"fixture/project","visibility":"private"})
    );
    assert_eq!(local["gates"][0]["branches"], json!(["work", "new"]));
    assert_eq!(
        receipts::list_owned(&f.rt, "local", &f.actor, None, None, 500, 0)
            .await
            .unwrap()
            .receipts
            .len(),
        before
    );
    f.no_credential_read();
}

#[tokio::test]
async fn local_mapping_rejects_unknown_scheme_and_missing_opt_in() {
    for (remote, slug) in [
        ("ssh://host/repo", ""),
        ("ssh://host/repo", "owner/repo"),
        ("file://host/remote.git", ""),
        ("relative.git", ""),
        ("//host/repo", ""),
        ("file:////host/repo", ""),
        ("/tmp/bad\npath", ""),
        ("/tmp/remote.git", "owner/repo"),
    ] {
        let f = Fixture::new(|_| remote.into(), slug).await;
        f.refusal("git.push", f.push(), "remote_scheme").await;
        let gates = f.call(&f.actor, "git.gates", json!({})).await.unwrap();
        assert_eq!(
            gates["target"],
            json!({"kind":"unavailable","reason":"remote_scheme"})
        );
    }
}

#[tokio::test]
async fn local_push_reconcile_requires_marker_and_exact_remote_without_credentials() {
    let f = Fixture::file().await;
    f.call(&f.actor, "git.push", f.push()).await.unwrap();
    let committed = f.last(&f.actor).await;
    assert_eq!(committed.disposition, Disposition::Committed);
    // Seed an unfinished observation of the completed native effect. Terminal
    // receipts cannot be downgraded; this fixture needs its own row and marker.
    let mut receipt = committed.clone();
    receipt.id = uuid::Uuid::new_v4().to_string();
    receipt.disposition = Disposition::Unknown;
    receipt.finished_at = None;
    receipts::insert(&f.rt, &receipt).await.unwrap();
    crate::local_git::record_push_marker(&f.repo, "work", &f.head, &receipt.id)
        .await
        .unwrap();
    git(&f.bare, &["update-ref", "refs/heads/work", &f.rival]);
    f.call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(
        receipts::load_owned(&f.rt, "local", &f.actor, &receipt.id)
            .await
            .unwrap()
            .disposition,
        Disposition::Unknown
    );
    git(&f.bare, &["update-ref", "refs/heads/work", &f.head]);
    f.call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(
        receipts::load_owned(&f.rt, "local", &f.actor, &receipt.id)
            .await
            .unwrap()
            .disposition,
        Disposition::Committed
    );
    assert_eq!(
        receipts::load_owned(&f.rt, "local", &f.actor, &committed.id)
            .await
            .unwrap()
            .disposition,
        Disposition::Committed,
        "the original terminal receipt is unchanged"
    );
    // This still-unfinished in-memory copy gets a new ID with no matching marker.
    receipt.id = uuid::Uuid::new_v4().to_string();
    receipts::insert(&f.rt, &receipt).await.unwrap();
    f.call(&f.actor, "git.reconcile", json!({"receipt":receipt.id}))
        .await
        .unwrap();
    assert_eq!(
        receipts::load_owned(&f.rt, "local", &f.actor, &receipt.id)
            .await
            .unwrap()
            .disposition,
        Disposition::Unknown
    );
    f.no_credential_read();
}

#[tokio::test]
async fn local_file_url_rejects_decoded_controls_slashes_and_invalid_encoding() {
    for remote in [
        "file:///tmp/line%0Abreak.git",
        "file:///tmp/line%0dbreak.git",
        "file:///tmp/nul%00.git",
        "file:///%2Fprivate/tmp/remote.git",
        "file:///%2fprivate/tmp/remote.git",
        "file://%2Fprivate/tmp/remote.git",
        "file:///tmp/bad%",
        "file:///tmp/bad%2",
        "file:///tmp/bad%ZZ",
        "file:///tmp/bad%FF",
    ] {
        let f = Fixture::new(|_| remote.into(), "").await;
        f.refusal("git.push", f.push(), "remote_scheme").await;
    }
}
