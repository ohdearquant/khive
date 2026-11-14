//! `knowledge.import` reads only under the configured `[knowledge].import_roots`.

use std::path::PathBuf;

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    KhiveRuntime, KnowledgeSectionConfig, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;
use tempfile::TempDir;

const CANARY: &str = "import-roots-canary-7c1e";

struct Fixture {
    registry: VerbRegistry,
    rt: KhiveRuntime,
}

impl Fixture {
    fn new(import_roots: Vec<PathBuf>) -> Self {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            brain_profile: None,
            actor_id: None,
            knowledge: KnowledgeSectionConfig { import_roots },
            ..RuntimeConfig::no_embeddings()
        })
        .expect("memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(KnowledgePack::new(rt.clone()));
        let registry = builder.build().expect("registry builds");
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        Self { registry, rt }
    }

    async fn import(&self, path: &str) -> Result<serde_json::Value, RuntimeError> {
        self.registry
            .dispatch("knowledge.import", json!({ "path": path }))
            .await
    }

    async fn atoms(&self) -> i64 {
        let access = self.rt.sql();
        let mut reader = access.reader().await.expect("reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT COUNT(*) AS count FROM knowledge_atoms".to_string(),
                params: vec![],
                label: None,
            })
            .await
            .expect("count query")
            .expect("count row");
        match row.get("count") {
            Some(SqlValue::Integer(count)) => *count,
            other => panic!("expected integer count, got {other:?}"),
        }
    }
}

fn markdown(title: &str) -> String {
    format!(
        "# {title}\n\n{CANARY} This markdown document carries enough meaningful words to satisfy the atom content validation rule for the import root confinement tests."
    )
}

fn refusal(path: &str) -> String {
    format!("invalid input: import path is not under a configured import root: {path:?}")
}

fn path_str(path: &std::path::Path) -> String {
    path.to_str().expect("utf-8 path").to_string()
}

#[tokio::test]
async fn a_file_under_a_configured_root_imports() {
    let allowed = TempDir::new().expect("allowed root");
    let file = allowed.path().join("topic.md");
    std::fs::write(&file, markdown("Allowed Topic")).expect("allowed markdown");
    let f = Fixture::new(vec![allowed.path().to_path_buf()]);

    f.import(&path_str(&file))
        .await
        .expect("a file under a configured root imports");
    f.import(&path_str(allowed.path()))
        .await
        .expect("the root directory itself imports");
    assert_eq!(f.atoms().await, 1);
}

#[tokio::test]
async fn every_path_outside_the_roots_gets_one_message() {
    use std::os::unix::fs::symlink;

    let allowed = TempDir::new().expect("allowed root");
    let denied = TempDir::new().expect("denied root");
    let canary = denied.path().join("canary.md");
    std::fs::write(&canary, markdown("Denied Canary")).expect("denied markdown");
    let missing = denied.path().join("missing.md");
    let link = allowed.path().join("link");
    symlink(denied.path(), &link).expect("directory symlink");
    let through_link = link.join("canary.md");
    let f = Fixture::new(vec![allowed.path().to_path_buf()]);

    for path in [
        path_str(&canary),
        path_str(denied.path()),
        path_str(&missing),
        path_str(&through_link),
    ] {
        let error = f
            .import(&path)
            .await
            .expect_err("a path outside every root is refused");
        let message = error.to_string();
        assert_eq!(message, refusal(&path), "one message for {path}");
        assert!(!message.contains(CANARY), "{message}");
    }
    assert_eq!(f.atoms().await, 0, "nothing outside the roots was read");
}

#[tokio::test]
async fn no_configured_roots_refuses_every_path() {
    let dir = TempDir::new().expect("temp dir");
    let file = dir.path().join("topic.md");
    std::fs::write(&file, markdown("Unconfigured Topic")).expect("markdown");
    let f = Fixture::new(Vec::new());

    let path = path_str(&file);
    let error = f
        .import(&path)
        .await
        .expect_err("no configured roots refuses every path");
    assert_eq!(error.to_string(), refusal(&path));
    assert_eq!(f.atoms().await, 0);
}
