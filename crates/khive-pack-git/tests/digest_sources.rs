//! `git.digest` reads only the sources `[git_digest]` names.

use std::path::{Path, PathBuf};
use std::process::Command;

use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    GitDigestSectionConfig, KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::json;
use tempfile::TempDir;

const LISTED: &str = "https://example.invalid/listed/repo";

fn registry(local_roots: Vec<PathBuf>, remotes: Vec<String>) -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        git_digest: GitDigestSectionConfig {
            local_roots,
            remotes,
        },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    builder
        .with_runtime_event_store(&runtime)
        .expect("runtime event store");
    let registry = builder.build().expect("git registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("repo directory");
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .expect("run git init");
    assert!(status.success(), "git init");
}

async fn digest(registry: &VerbRegistry, source: &str) -> Result<serde_json::Value, RuntimeError> {
    registry
        .dispatch("git.digest", json!({ "source": source, "include": [] }))
        .await
}

fn refusal(source: &str) -> String {
    format!("invalid input: source {source:?} is not a configured git.digest source")
}

fn path_str(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_string()
}

#[tokio::test]
async fn listed_sources_are_digested() {
    let allowed = TempDir::new().expect("allowed root");
    let repo = allowed.path().join("repo");
    init_repo(&repo);
    let registry = registry(vec![allowed.path().to_path_buf()], vec![LISTED.to_string()]);

    digest(&registry, &path_str(&repo))
        .await
        .expect("a repository under a local root is digested");
    for spelling in [
        LISTED,
        "https://example.invalid/listed/repo.git/",
        "https://EXAMPLE.invalid/listed/repo",
    ] {
        digest(&registry, spelling)
            .await
            .unwrap_or_else(|error| panic!("{spelling} names a listed remote: {error}"));
    }
}

#[tokio::test]
async fn every_source_outside_the_lists_gets_one_message() {
    use std::os::unix::fs::symlink;

    let allowed = TempDir::new().expect("allowed root");
    let denied = TempDir::new().expect("denied root");
    let canary = denied.path().join("canary");
    init_repo(&canary);
    let link = allowed.path().join("link");
    symlink(denied.path(), &link).expect("directory symlink");
    let registry = registry(vec![allowed.path().to_path_buf()], vec![LISTED.to_string()]);

    for source in [
        path_str(&canary),
        path_str(denied.path()),
        path_str(&denied.path().join("missing")),
        path_str(&link.join("canary")),
        "https://example.invalid/listed/other".to_string(),
        "https://example.invalid/unlisted/repo".to_string(),
    ] {
        let error = digest(&registry, &source)
            .await
            .expect_err("a source outside the lists is refused");
        assert_eq!(
            error.to_string(),
            refusal(&source),
            "one message for {source}"
        );
    }
}

#[tokio::test]
async fn a_directory_under_a_root_without_a_repository_keeps_its_message() {
    let allowed = TempDir::new().expect("allowed root");
    let plain = allowed.path().join("plain");
    std::fs::create_dir_all(&plain).expect("plain directory");
    let registry = registry(vec![allowed.path().to_path_buf()], Vec::new());

    let error = digest(&registry, &path_str(&plain))
        .await
        .expect_err("a directory with no .git entry is refused");
    assert!(
        error.to_string().contains("does not contain a .git entry"),
        "{error}"
    );
}

#[tokio::test]
async fn an_empty_policy_refuses_every_source() {
    let dir = TempDir::new().expect("temp dir");
    let repo = dir.path().join("repo");
    init_repo(&repo);
    let registry = registry(Vec::new(), Vec::new());

    for source in [path_str(&repo), LISTED.to_string()] {
        let error = digest(&registry, &source)
            .await
            .expect_err("an empty policy refuses");
        assert_eq!(error.to_string(), refusal(&source));
    }
}
