//! Real dispatch coverage for every registered git verb, with owned mutation
//! witnesses. The fixture uses no live credentials, repository or network.

use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{GitWriteEntryConfig, GitWriteSectionConfig};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::types::{PageRequest, SqlStatement};
use khive_types::{EventKind, EventOutcome, Pack};
use serde_json::{json, Value};

use crate::receipts::{self, Disposition};
use crate::remote_transport::{ApiRequest, PushRequest, RemoteError, RemoteTransport};
use crate::GitPack;

#[derive(Default)]
struct ObservedRemote(AtomicUsize);

#[async_trait]
impl RemoteTransport for ObservedRemote {
    async fn api(&self, _: &str, _: ApiRequest) -> Result<Value, RemoteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(RemoteError::Unavailable)
    }
    async fn review_decision(&self, _: &str, _: &str, _: u64) -> Result<Value, RemoteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(RemoteError::Unavailable)
    }
    async fn remote_ref(
        &self,
        _: Option<&str>,
        _: &str,
        _: &str,
    ) -> Result<Option<String>, RemoteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(RemoteError::Unavailable)
    }
    async fn push(&self, _: Option<&str>, _: PushRequest) -> Result<(), RemoteError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(RemoteError::Unavailable)
    }
}

fn native_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .and_then(|path| std::fs::canonicalize(path).ok())
        .expect("native git fixture dependency")
}

fn git(program: &Path, repo: &Path, args: &[&str]) -> String {
    let output = Command::new(program)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").expect("PATH"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(repo)
        .args(args)
        .output()
        .expect("owned fixture git process");
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("fixture git output")
        .trim()
        .into()
}

fn quoted(path: &Path) -> String {
    format!(
        "'{}'",
        path.to_str().expect("fixture path").replace('\'', "'\\''")
    )
}

async fn domain_snapshot(rt: &KhiveRuntime) -> Value {
    let rows = rt.sql().reader().await.expect("reader").query_all(SqlStatement {
        sql: "SELECT (SELECT count(*) FROM entities) AS entities, (SELECT count(*) FROM notes) AS notes".into(),
        params: vec![],
        label: Some("git_strict_arguments_domain_witness".into()),
    }).await.expect("domain snapshot");
    serde_json::to_value(rows).expect("snapshot JSON")
}

struct RefusalEvidence {
    receipts: BTreeSet<String>,
    audits: BTreeSet<uuid::Uuid>,
}

async fn refusal_evidence(rt: &KhiveRuntime, actor: &str, verb: &str) -> RefusalEvidence {
    let receipts = receipts::list_owned(rt, "local", actor, None, None, 500, 0)
        .await
        .expect("owned receipts")
        .receipts
        .into_iter()
        .map(|receipt| receipt.id)
        .collect();
    let token = rt
        .authorize(Namespace::local())
        .expect("event witness namespace");
    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            khive_storage::event::EventFilter {
                verbs: vec![verb.into()],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 500,
            },
        )
        .await
        .expect("write audit witness");
    let audits = events
        .items
        .into_iter()
        .filter(|event| {
            event.kind == EventKind::Audit
                && event.payload.get("decision").and_then(Value::as_str) == Some("deny")
        })
        .map(|event| {
            assert_eq!(
                event.outcome,
                EventOutcome::Denied,
                "REFUSAL_EVIDENCE: denial audit outcome"
            );
            event.id
        })
        .collect();
    RefusalEvidence { receipts, audits }
}

async fn assert_refusal_evidence(
    rt: &KhiveRuntime,
    actor: &str,
    verb: &str,
    params: &Value,
    before: RefusalEvidence,
    error: &str,
) {
    let has_receipt = matches!(
        verb,
        "git.init"
            | "git.checkout"
            | "git.diff"
            | "git.branch"
            | "git.update_ref"
            | "git.reconcile"
            | "git.push"
            | "git.pr_open"
            | "git.pr_review"
            | "git.pr_merge"
    ) || (verb == "git.commit" && params.get("tree").is_some());
    let has_audit = matches!(
        verb,
        "git.commit"
            | "git.branch"
            | "git.update_ref"
            | "git.push"
            | "git.pr_open"
            | "git.pr_review"
            | "git.pr_merge"
    );
    let after = refusal_evidence(rt, actor, verb).await;
    assert!(
        before.receipts.is_subset(&after.receipts),
        "REFUSAL_EVIDENCE: prior receipts changed"
    );
    let added: Vec<_> = after.receipts.difference(&before.receipts).collect();
    assert_eq!(
        added.len(),
        usize::from(has_receipt),
        "REFUSAL_EVIDENCE: {verb} exact receipt count"
    );
    if let Some(id) = added.first() {
        let receipt = receipts::load_owned(rt, "local", actor, id)
            .await
            .expect("new refusal receipt");
        assert_eq!(receipt.verb, verb);
        assert_eq!(
            receipt.disposition,
            Disposition::NotCommitted,
            "REFUSAL_EVIDENCE: uncommitted refusal"
        );
        assert_eq!(receipt.reason.as_deref(), Some("invalid_params"));
        assert!(
            error.contains(receipt.id.as_str()),
            "REFUSAL_EVIDENCE: error must name its receipt"
        );
        assert!(receipt.finished_at.is_some());
        assert!(
            receipt.policy.is_null(),
            "policy must not run for invalid parameters"
        );
        assert!(
            receipt.credential.is_null(),
            "credentials must not resolve for invalid parameters"
        );
        assert!(!receipt
            .to_value()
            .to_string()
            .contains("unknown-field-secret-witness"));
    }
    assert_eq!(
        after.audits.difference(&before.audits).count(),
        usize::from(has_audit),
        "REFUSAL_EVIDENCE: {verb} exact write-audit count"
    );
    assert!(
        !error.contains("unknown-field-secret-witness"),
        "unknown argument values must not be echoed"
    );
}

#[tokio::test]
async fn every_registered_git_verb_rejects_named_unknowns_preserving_refusal_evidence() {
    if crate::test_process::run_in_child() {
        return;
    }
    let directory = tempfile::tempdir().expect("owned fixture");
    let repo = directory.path().join("repo");
    let blank = directory.path().join("blank");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&blank).unwrap();
    let native = native_git();
    git(&native, &repo, &["init", "-b", "main"]);
    git(
        &native,
        &repo,
        &["config", "user.name", "Strict Arguments Fixture"],
    );
    git(
        &native,
        &repo,
        &["config", "user.email", "strict@example.invalid"],
    );
    git(&native, &repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("note.txt"), "initial\n").unwrap();
    git(&native, &repo, &["add", "note.txt"]);
    git(&native, &repo, &["commit", "-m", "fixture base"]);
    let head = git(&native, &repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("note.txt"), "pending mutation\n").unwrap();
    git(&native, &repo, &["add", "note.txt"]);
    let status = git(&native, &repo, &["status", "--porcelain"]);

    let process_log = directory.path().join("git-processes");
    let program = directory.path().join("observed-git");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf 'invoked\\n' >> {}\nexec {} \"$@\"\n",
            quoted(&process_log),
            quoted(&native)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let actor = format!("strict-git:{}", uuid::Uuid::new_v4());
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        git_write: GitWriteSectionConfig {
            program: Some(program),
            allowed: [&repo, &blank]
                .into_iter()
                .map(|path| GitWriteEntryConfig {
                    repo: path.display().to_string(),
                    branches: vec!["*".into()],
                })
                .collect(),
            ..Default::default()
        },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("memory runtime");
    let remote = Arc::new(ObservedRemote::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.clone()));
    builder.register(KgPack::new(rt.clone()));
    builder.register(ToolPack::new(rt.clone()));
    builder.register(GitPack::with_remote_transport(rt.clone(), remote.clone()));
    builder.with_runtime_event_store(&rt).expect("audit store");
    let registry = builder.build().expect("registry");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());

    let cases = vec![
        (
            "git.digest",
            json!({"source":repo,"include":[],"max_items":null,"project":null}),
            "source",
        ),
        (
            "git.ingest_cursor",
            json!({"project":uuid::Uuid::new_v4(),"source_kind":"commits"}),
            "source_kind",
        ),
        (
            "git.commit",
            json!({"repo":repo,"message":"must not commit","paths":null,"author":null}),
            "message",
        ),
        (
            "git.commit",
            json!({"repo":repo,"message":"tree form","tree":"fixture-tree","branch":"main","expected_head":head}),
            "tree",
        ),
        (
            "git.branch",
            json!({"repo":repo,"name":"candidate"}),
            "name",
        ),
        (
            "git.update_ref",
            json!({"repo":repo,"branch":"main","to":head}),
            "expected",
        ),
        (
            "git.update_ref",
            json!({"repo":repo,"branch":"main","to":head,"expected":null}),
            "expected",
        ),
        (
            "git.push",
            json!({"repo":repo,"branch":"main","expected_local":head,"expected_remote":null}),
            "expected_remote",
        ),
        (
            "git.pr_open",
            json!({"repo":repo,"head":"work","base":"main","title":"fixture","body":"","expected_head":head}),
            "title",
        ),
        (
            "git.pr_review",
            json!({"repo":repo,"number":1,"verdict":"comment","body":"","expected_head":head}),
            "verdict",
        ),
        (
            "git.pr_merge",
            json!({"repo":repo,"number":1,"method":"squash","subject":"fixture","body":"","expected_head":head}),
            "method",
        ),
        ("git.init", json!({"repo":blank,"branch":"main"}), "branch"),
        ("git.checkout", json!({"repo":repo,"ref":"HEAD"}), "ref"),
        (
            "git.diff",
            json!({"repo":repo,"input_kind":"commits","base":head,"head":head}),
            "input_kind",
        ),
        (
            "git.reconcile",
            json!({"receipt":uuid::Uuid::new_v4()}),
            "receipt",
        ),
        ("git.receipts", json!({"repo":repo,"limit":1}), "offset"),
        ("git.gates", json!({"repo":repo}), "repo"),
        (
            "git.status",
            json!({"repo":repo,"untracked":"normal","limit":1}),
            "untracked",
        ),
        (
            "git.log",
            json!({"repo":repo,"ref":"HEAD","limit":1}),
            "path",
        ),
    ];
    let covered: BTreeSet<_> = cases.iter().map(|(verb, _, _)| *verb).collect();
    let registered: BTreeSet<_> = <GitPack as Pack>::HANDLERS
        .iter()
        .map(|handler| handler.name)
        .collect();
    assert_eq!(
        covered, registered,
        "every registered verb needs a dispatch witness"
    );
    for verb in &registered {
        registry
            .dispatch(
                "tool.policy",
                json!({"actor":actor,"tool":verb,"decision":"allow"}),
            )
            .await
            .expect("fixture policy");
    }
    let before = domain_snapshot(&rt).await;
    let policy_before = crate::local_handlers::policy_check_count(&actor);
    // Runtime construction may inspect its configured program. Only requests
    // below belong to the measured boundary.
    if process_log.exists() {
        std::fs::remove_file(&process_log).unwrap();
    }
    for (verb, params, allowed) in &cases {
        for unknown in ["zzz_not_a_real_param", "program"] {
            let mut supplied = params.clone();
            supplied[unknown] = json!("unknown-field-secret-witness");
            let evidence = refusal_evidence(&rt, &actor, verb).await;
            let error = registry
                .dispatch(verb, supplied)
                .await
                .expect_err("UNKNOWN_ARGUMENT_BOUNDARY: unknown argument must refuse")
                .to_string();
            assert!(
                error.contains(&format!("unknown field `{unknown}`")),
                "UNKNOWN_ARGUMENT_BOUNDARY: {verb}: {error}"
            );
            assert!(
                error.contains(&format!("`{allowed}`")),
                "allowed fields missing for {verb}: {error}"
            );
            assert_refusal_evidence(&rt, &actor, verb, params, evidence, &error).await;
            assert_eq!(
                domain_snapshot(&rt).await,
                before,
                "{verb} wrote provenance rows"
            );
            assert_eq!(
                crate::local_handlers::policy_check_count(&actor),
                policy_before,
                "{verb} reached tool policy"
            );
            assert_eq!(
                remote.0.load(Ordering::SeqCst),
                0,
                "{verb} reached remote transport"
            );
            assert!(!process_log.exists(), "{verb} spawned configured git");
        }
        let evidence = refusal_evidence(&rt, &actor, verb).await;
        let error = registry
            .dispatch(verb, json!([]))
            .await
            .expect_err("NAMED_ARGUMENTS_ONLY: positional array accepted")
            .to_string();
        assert!(
            error.contains("object with named fields"),
            "NAMED_ARGUMENTS_ONLY: {verb}: {error}"
        );
        assert_refusal_evidence(&rt, &actor, verb, &json!([]), evidence, &error).await;
        assert_eq!(
            domain_snapshot(&rt).await,
            before,
            "{verb} positional arguments wrote domain rows"
        );
    }
    assert!(
        !blank.join(".git").exists(),
        "invalid init created a repository"
    );
    assert_eq!(git(&native, &repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&native, &repo, &["status", "--porcelain"]), status);
    assert_eq!(git(&native, &repo, &["branch", "--list", "candidate"]), "");

    // The same pending change really can commit. Unknown-argument refusals
    // above must not be explained by a dead fixture or an impossible mutation.
    let committed = registry
        .dispatch(
            "git.commit",
            json!({"repo":repo,"message":"accepted fixture mutation","paths":null,"author":null}),
        )
        .await
        .expect("valid legacy commit");
    assert_ne!(committed["sha"], head);
    assert_eq!(
        git(&native, &repo, &["rev-parse", "HEAD"]),
        committed["sha"].as_str().unwrap()
    );
    assert!(
        process_log.exists(),
        "positive control must reach the observed process"
    );

    // Digest retains its default/null/negative/high clamp behavior and its
    // historical non-string project fallback, through the real registry.
    for (budget, expected) in [(Value::Null, 500), (json!(-1), 1), (json!(9000), 2000)] {
        let result = registry
            .dispatch(
                "git.digest",
                json!({"source":repo,"include":[],"max_items":budget,"project":false}),
            )
            .await
            .expect("valid digest compatibility control");
        assert_eq!(result["max_items_effective"], expected);
    }
    assert_ne!(
        domain_snapshot(&rt).await,
        before,
        "valid digest must create provenance anchor"
    );
}
