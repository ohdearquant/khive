use super::{policy_check_count, set_update_ref_race_hook, validate_operation, UpdateRefRaceHook};
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{GitWriteEntryConfig, GitWriteSectionConfig};
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn registry(repo: Option<&Path>, actor: &str) -> (KhiveRuntime, VerbRegistry) {
    let git_write = GitWriteSectionConfig {
        allowed: repo
            .map(|repo| {
                vec![GitWriteEntryConfig {
                    repo: repo.display().to_string(),
                    branches: vec!["*".into()],
                }]
            })
            .unwrap_or_default(),
        ..Default::default()
    };
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        git_write,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.into()));
    builder.register(KgPack::new(rt.clone()));
    builder.register(ToolPack::new(rt.clone()));
    builder.register(crate::GitPack::new(rt.clone()));
    builder.with_runtime_event_store(&rt).expect("audit store");
    let registry = builder.build().expect("registry");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    (rt, registry)
}

fn git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("start git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output UTF-8")
        .trim()
        .to_string()
}

#[test]
fn update_ref_expected_requires_lowercase_sha_before_repo_validation() {
    let repo = "/path/that/is/not/a/repository";
    for expected in ["abc".to_string(), "A".repeat(40), "g".repeat(40)] {
        let failure = validate_operation(
            "git.update_ref",
            &json!({
                "repo":repo,
                "branch":"main",
                "to":"a".repeat(40),
                "expected":expected,
            }),
        )
        .expect_err("invalid expected value must be rejected before repository validation");
        assert_eq!(failure.reason, "invalid_params");
        assert_eq!(
            failure.detail.as_deref(),
            Some("expected must be a 40-character lowercase hexadecimal object id")
        );
    }
}

#[tokio::test]
async fn update_ref_checks_use_policy_before_repository_validation() {
    let directory = tempfile::tempdir().expect("repository path");
    let actor = format!("update-ref-policy:{}", uuid::Uuid::new_v4());
    let (_rt, registry) = registry(Some(directory.path()), &actor);
    let before = policy_check_count(&actor);
    let error = registry
        .dispatch(
            "git.update_ref",
            json!({
                "repo":directory.path(),
                "branch":"main",
                "to":"a".repeat(40),
                "expected":"b".repeat(40),
            }),
        )
        .await
        .expect_err("an absent use policy must deny the operation")
        .to_string();
    assert!(error.contains("policy_denied"), "{error}");
    assert_eq!(policy_check_count(&actor), before + 1);
}

#[tokio::test]
async fn update_ref_ancestry_and_compare_swap_share_the_repo_lock() {
    let directory = tempfile::tempdir().expect("fixture directory");
    let repo = directory.path().join("repo");
    std::fs::create_dir(&repo).expect("repository directory");
    git(&repo, &["init", "-q", "-b", "work"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(repo.join("file.txt"), "initial\n").expect("fixture file");
    git(&repo, &["add", "--", "file.txt"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    let expected = git(&repo, &["rev-parse", "HEAD"]);
    let tree = git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let to = git(
        &repo,
        &["commit-tree", &tree, "-p", &expected, "-m", "candidate"],
    );
    let rival = git(
        &repo,
        &[
            "commit-tree",
            &tree,
            "-p",
            &expected,
            "-m",
            "concurrent update",
        ],
    );

    let actor = format!("update-ref-race:{}", uuid::Uuid::new_v4());
    let (_rt, registry) = registry(Some(&repo), &actor);
    registry
        .dispatch(
            "tool.policy",
            json!({"actor":actor,"tool":"git.update_ref","decision":"allow"}),
        )
        .await
        .expect("allow the write operation");

    let race_repo = std::fs::canonicalize(&repo).expect("canonical repository");
    let moved_to = rival.clone();
    let compare_from = expected.clone();
    let hook: UpdateRefRaceHook = std::sync::Arc::new(move |hook_repo, branch| {
        if hook_repo != race_repo {
            return;
        }
        assert_eq!(branch, "work");
        let output = Command::new("git")
            .current_dir(hook_repo)
            .args(["update-ref", "refs/heads/work", &moved_to, &compare_from])
            .output()
            .expect("start competing ref update");
        assert!(
            output.status.success(),
            "competing ref update failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    });
    set_update_ref_race_hook(Some(hook));
    struct ResetRaceHook;
    impl Drop for ResetRaceHook {
        fn drop(&mut self) {
            set_update_ref_race_hook(None);
        }
    }
    let _reset_hook = ResetRaceHook;

    let error = registry
        .dispatch(
            "git.update_ref",
            json!({
                "repo":repo,
                "branch":"work",
                "to":to,
                "expected":expected,
            }),
        )
        .await
        .expect_err("the compare-and-swap must reject the intervening ref move")
        .to_string();
    assert!(error.contains("expected_head_mismatch"), "{error}");
    assert_eq!(git(&repo, &["rev-parse", "refs/heads/work"]), rival);
}
