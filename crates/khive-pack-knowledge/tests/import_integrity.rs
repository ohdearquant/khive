//! Regression coverage for issue #1758's `knowledge.import` integrity slice.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture {
    registry: VerbRegistry,
    rt: KhiveRuntime,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    async fn count(&self, sql: &str) -> i64 {
        let access = self.rt.sql();
        let mut reader = access.reader().await.expect("reader");
        let row = reader
            .query_row(SqlStatement {
                sql: sql.to_string(),
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

fn fixture() -> Fixture {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry, rt }
}

fn markdown(title: &str) -> String {
    format!(
        "# {title}\n\nThis imported markdown document contains enough meaningful words to satisfy the atom content validation while preserving deterministic knowledge corpus identity and source provenance across nested directories."
    )
}

#[cfg(unix)]
async fn assert_directory_symlink_root_rejected(trailing_slash: bool) {
    use std::os::unix::fs::symlink;

    let f = fixture();
    let root = TempDir::new().expect("temp root");
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).expect("target directory");
    std::fs::write(target.join("topic.md"), markdown("Symlink Target")).expect("target markdown");
    let alias = root.path().join("alias");
    symlink(&target, &alias).expect("directory symlink");
    let mut import_path = alias.to_str().expect("utf-8 symlink path").to_string();
    if trailing_slash {
        import_path.push('/');
    }

    let error = f
        .dispatch("knowledge.import", json!({ "path": import_path }))
        .await
        .expect_err("a directory symlink root must fail closed");
    assert!(
        error.to_string().contains("must not be a symbolic link"),
        "{error}"
    );
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        0,
        "a rejected root symlink must not import its target"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn directory_symlink_root_is_rejected_without_trailing_slash() {
    assert_directory_symlink_root_rejected(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn directory_symlink_root_is_rejected_with_trailing_slash() {
    assert_directory_symlink_root_rejected(true).await;
}

#[tokio::test]
async fn directory_import_uses_root_relative_slugs_and_provenance() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    let alpha = root.path().join("alpha");
    let beta = root.path().join("beta");
    std::fs::create_dir_all(&alpha).expect("alpha dir");
    std::fs::create_dir_all(&beta).expect("beta dir");
    std::fs::write(alpha.join("topic.md"), markdown("Alpha Topic")).expect("alpha markdown");
    std::fs::write(beta.join("topic.md"), markdown("Beta Topic")).expect("beta markdown");

    let response = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect("directory import");

    assert_eq!(response["imported_atoms"], 2);
    assert_eq!(response["files_discovered"], 2);
    assert_eq!(response["files_processed"], 2);
    for (slug, source_path) in [
        ("alpha--topic", "alpha/topic.md"),
        ("beta--topic", "beta/topic.md"),
    ] {
        let atom = f
            .dispatch("knowledge.get", json!({ "id": slug }))
            .await
            .expect("root-relative atom");
        assert_eq!(atom["slug"], slug);
        assert_eq!(atom["properties"]["source_path"], source_path);
        assert_eq!(atom["source_uri"], format!("file:{source_path}"));
    }
}

#[tokio::test]
async fn normalized_slug_collision_is_rejected_before_any_write() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    for directory in ["alpha beta", "alpha-beta"] {
        let nested = root.path().join(directory);
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(nested.join("topic.md"), markdown(directory)).expect("markdown");
    }

    let error = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect_err("normalization collision must fail closed");
    let message = error.to_string();
    assert!(message.contains("normalized slug collision"), "{message}");
    assert!(message.contains("alpha beta/topic.md"), "{message}");
    assert!(message.contains("alpha-beta/topic.md"), "{message}");
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        0,
        "collision validation must finish before the first atom write"
    );
}

#[tokio::test]
async fn later_invalid_source_is_rejected_before_any_write() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    std::fs::write(root.path().join("a-valid.md"), markdown("Valid First"))
        .expect("valid markdown");
    std::fs::write(root.path().join("z-invalid.md"), "# Too Short\n").expect("invalid markdown");

    let error = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect_err("all source content must validate before writes");
    assert!(error.to_string().contains("atom content must be at least"));
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        0,
        "a later invalid source must not leave an earlier atom behind"
    );
}

#[tokio::test]
async fn atom_mode_preserves_full_markdown_without_section_rows() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    let markdown = "\n# Complete Atom\n\nThis preamble deliberately contains more than twenty words so the previous implementation selected it alone and silently discarded every structured section that followed from atom content.\n\n## Overview\n\nThe overview remains part of the whole markdown document and must be preserved with its heading in atom mode.\n\n## Formalism\n\nThe formalism also remains part of the whole markdown document with equations, definitions, and explanatory context intact.  \n";
    let path = root.path().join("complete-atom.md");
    std::fs::write(&path, markdown).expect("markdown");

    let response = f
        .dispatch(
            "knowledge.import",
            json!({
                "path": path.to_str().expect("utf-8 path"),
                "chunk_strategy": "atom"
            }),
        )
        .await
        .expect("atom import");
    assert_eq!(response["imported_atoms"], 1);
    assert_eq!(response["imported_sections"], 0);
    assert_eq!(response["sections_discovered"], 2);
    assert_eq!(response["sections_skipped"], 0);

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "complete-atom" }))
        .await
        .expect("atom");
    assert_eq!(atom["content"], markdown);
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_sections")
            .await,
        0,
        "atom mode must not create section rows"
    );
}

#[tokio::test]
async fn import_report_counts_discovery_and_intentional_skips() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    let markdown = "# Counted Atom\n\nThis preamble has enough meaningful words for a valid searchable atom while the deliberately short section below is counted honestly instead of disappearing without explanation.\n\n## Overview\n\nshort section";
    std::fs::write(root.path().join("counted.md"), markdown).expect("markdown");
    std::fs::write(root.path().join("ignored.txt"), "not markdown").expect("ignored file");

    let response = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect("import");

    assert_eq!(response["entries_visited"], 2);
    assert_eq!(response["files_discovered"], 1);
    assert_eq!(response["files_processed"], 1);
    assert_eq!(response["files_skipped"], 1);
    assert_eq!(response["traversal_errors"], 0);
    assert_eq!(response["sections_discovered"], 1);
    assert_eq!(response["imported_sections"], 0);
    assert_eq!(response["sections_skipped"], 1);
}
