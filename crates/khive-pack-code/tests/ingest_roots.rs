//! `code.ingest` reads sources and writes map databases only under the
//! configured `[code_ingest].roots`.

use std::path::{Path, PathBuf};

use khive_pack_code::CodePack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    CodeIngestSectionConfig, KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn physical_tempdir() -> TempDir {
    let physical = std::env::temp_dir()
        .canonicalize()
        .expect("physical temporary directory");
    tempfile::tempdir_in(physical).expect("temporary directory")
}

fn registry(roots: Vec<PathBuf>) -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        code_ingest: CodeIngestSectionConfig { roots },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CodePack::new(runtime.clone()));
    let registry = builder.build().expect("code registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn source_tree(path: &Path) {
    std::fs::create_dir_all(path).expect("source directory");
    std::fs::write(path.join("lib.rs"), "pub fn canary() {}\n").expect("source file");
}

async fn ingest(
    registry: &VerbRegistry,
    path: &str,
    db: Option<&str>,
) -> Result<Value, RuntimeError> {
    let mut args = json!({ "path": path, "tiers": [] });
    if let Some(db) = db {
        args["db"] = json!(db);
    }
    registry.dispatch("code.ingest", args).await
}

fn refusal(field: &str, raw: &str) -> String {
    format!("invalid input: {field} {raw:?} is not under a configured code.ingest root")
}

fn path_str(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_string()
}

#[tokio::test]
async fn a_source_under_a_root_is_ingested_beside_it() {
    let allowed = physical_tempdir();
    let source = allowed.path().join("source");
    source_tree(&source);
    let registry = registry(vec![allowed.path().to_path_buf()]);

    ingest(&registry, &path_str(&source), None)
        .await
        .expect("a source under a root is ingested");
    let target = allowed.path().join("map.db");
    ingest(&registry, &path_str(&source), Some(&path_str(&target)))
        .await
        .expect("a missing target under a root is created");
    assert!(target.exists(), "the map database was written");
}

#[tokio::test]
async fn every_source_outside_the_roots_gets_one_message() {
    use std::os::unix::fs::symlink;

    let allowed = physical_tempdir();
    let denied = physical_tempdir();
    let canary = denied.path().join("canary");
    source_tree(&canary);
    let link = allowed.path().join("link");
    symlink(denied.path(), &link).expect("directory symlink");
    let registry = registry(vec![allowed.path().to_path_buf()]);

    for source in [
        path_str(&canary),
        path_str(denied.path()),
        path_str(&denied.path().join("missing")),
        path_str(&link.join("canary")),
    ] {
        let error = ingest(&registry, &source, None)
            .await
            .expect_err("a source outside every root is refused");
        assert_eq!(
            error.to_string(),
            refusal("path", &source),
            "one message for {source}"
        );
    }
    assert!(
        !canary.join(".khive").exists(),
        "nothing was written beside the refused source"
    );
}

#[tokio::test]
async fn a_target_outside_the_roots_is_refused_before_it_is_created() {
    let allowed = physical_tempdir();
    let denied = physical_tempdir();
    let source = allowed.path().join("source");
    source_tree(&source);
    let registry = registry(vec![allowed.path().to_path_buf()]);

    for target in [
        denied.path().join("map.db"),
        denied.path().join("nested").join("map.db"),
    ] {
        let raw = path_str(&target);
        let error = ingest(&registry, &path_str(&source), Some(&raw))
            .await
            .expect_err("a target outside every root is refused");
        assert_eq!(error.to_string(), refusal("db", &raw));
    }
    assert_eq!(
        std::fs::read_dir(denied.path())
            .expect("denied listing")
            .count(),
        0,
        "nothing was created outside the roots"
    );
}

#[tokio::test]
async fn an_empty_policy_refuses_every_source() {
    let dir = physical_tempdir();
    let source = dir.path().join("source");
    source_tree(&source);
    let registry = registry(Vec::new());

    let raw = path_str(&source);
    let error = ingest(&registry, &raw, None)
        .await
        .expect_err("an empty policy refuses");
    assert_eq!(error.to_string(), refusal("path", &raw));
}
