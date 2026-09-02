//! Regression coverage for `knowledge.import` identity and integrity (#1758, #1984).

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
async fn frontmatter_id_maps_metadata_and_strips_body_in_atom_mode() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    let body = "# Body Heading\n\nThis body contains the searchable prose that should be indexed without YAML metadata syntax while retaining enough meaningful words for deterministic atom validation and retrieval behavior.\n";
    let source = format!(
        "---\nid: Canonical.Doc-42\nname: Frontmatter Name\ntags:\n  - retrieval\n  - ingestion\nproperties:\n  category: procedure\n  revision: 2\nowner: research\n---\n{body}"
    );
    let path = root.path().join("fallback-name.md");
    std::fs::write(&path, source).expect("frontmatter markdown");

    let response = f
        .dispatch(
            "knowledge.import",
            json!({
                "path": path.to_str().expect("utf-8 path"),
                "chunk_strategy": "atom"
            }),
        )
        .await
        .expect("frontmatter import");
    assert_eq!(response["imported_atoms"], 1);

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "canonical-doc-42" }))
        .await
        .expect("canonical atom");
    assert_eq!(atom["slug"], "canonical-doc-42");
    assert_eq!(atom["name"], "Frontmatter Name");
    assert_eq!(atom["content"], body);
    assert_eq!(atom["tags"], json!(["retrieval", "ingestion"]));
    assert_eq!(atom["properties"]["category"], "procedure");
    assert_eq!(atom["properties"]["revision"], 2);
    assert_eq!(atom["properties"]["owner"], "research");
    assert_eq!(atom["properties"]["source_path"], "fallback-name.md");
    assert_eq!(atom["properties"]["atlas_id"], "Canonical.Doc-42");
    assert_eq!(atom["source_uri"], "atlas:Canonical.Doc-42");
}

#[tokio::test]
async fn frontmatter_canonical_id_updates_existing_slug_and_reimports_idempotently() {
    let f = fixture();
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "canonical-doc-42",
            "name": "Existing Atom",
            "content": "existing dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "properties": {"atlas_id": "Canonical.Doc-42"},
            "source_uri": "atlas:Canonical.Doc-42",
            "source_type": "manual",
            "finalized": true
        }] }),
    )
    .await
    .expect("seed canonical atom");
    let original = f
        .dispatch("knowledge.get", json!({ "id": "canonical-doc-42" }))
        .await
        .expect("seed read");

    let root = TempDir::new().expect("temp root");
    let path = root.path().join("different-filename.md");
    let first_body = "# Imported Name\n\nThis first imported body contains enough useful prose to update the existing canonical atom without creating a second identity or losing deterministic source provenance.\n";
    std::fs::write(
        &path,
        format!("---\nid: Canonical.Doc-42\nname: Imported Name\n---\n{first_body}"),
    )
    .expect("first source");
    f.dispatch(
        "knowledge.import",
        json!({
            "path": path.to_str().expect("utf-8 path"),
            "chunk_strategy": "atom"
        }),
    )
    .await
    .expect("update existing canonical atom");

    let updated = f
        .dispatch("knowledge.get", json!({ "id": "canonical-doc-42" }))
        .await
        .expect("updated read");
    assert_eq!(updated["id"], original["id"]);
    assert_eq!(updated["name"], "Imported Name");
    assert_eq!(updated["content"], first_body);
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        1
    );

    let second_body = "# Imported Name V2\n\nThis second imported body proves a repeated canonical import updates the same atom identifier and leaves exactly one durable corpus row after completion.\n";
    std::fs::write(
        &path,
        format!("---\nid: Canonical.Doc-42\nname: Imported Name V2\n---\n{second_body}"),
    )
    .expect("second source");
    f.dispatch(
        "knowledge.import",
        json!({
            "path": path.to_str().expect("utf-8 path"),
            "chunk_strategy": "atom"
        }),
    )
    .await
    .expect("repeat canonical import");
    let repeated = f
        .dispatch("knowledge.get", json!({ "id": "canonical-doc-42" }))
        .await
        .expect("repeat read");
    assert_eq!(repeated["id"], original["id"]);
    assert_eq!(repeated["content"], second_body);
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        1
    );
}

#[tokio::test]
async fn canonical_identity_collision_is_rejected_before_any_write() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    for (directory, filename) in [("alpha", "one.md"), ("beta", "two.md")] {
        let nested = root.path().join(directory);
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(
            nested.join(filename),
            format!("---\nid: Shared.Canonical-ID\n---\n{}", markdown(directory)),
        )
        .expect("canonical collision source");
    }

    let error = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect_err("canonical collision must fail closed");
    let message = error.to_string();
    assert!(message.contains("normalized slug collision"), "{message}");
    assert!(message.contains("alpha/one.md"), "{message}");
    assert!(message.contains("beta/two.md"), "{message}");
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        0,
        "canonical collision validation must precede every write"
    );
}

#[tokio::test]
async fn malformed_frontmatter_is_rejected_before_any_write() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    std::fs::write(root.path().join("a-valid.md"), markdown("Valid First")).expect("valid source");
    std::fs::write(
        root.path().join("z-invalid.md"),
        "---\nid: unterminated-frontmatter\n# This never closes\n\nThe remaining source has enough words to pass ordinary atom validation but must fail frontmatter preflight before any earlier document is written to storage.\n",
    )
    .expect("invalid source");

    let error = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect_err("unterminated frontmatter must fail closed");
    let message = error.to_string();
    assert!(message.contains("frontmatter"), "{message}");
    assert!(message.contains("z-invalid.md"), "{message}");
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        0,
        "frontmatter parsing must finish before the first write"
    );
}

#[tokio::test]
async fn existing_identity_on_a_different_slug_is_refused_without_duplicate() {
    let f = fixture();
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{
            "slug": "legacy-slug",
            "name": "Legacy Canonical Atom",
            "content": "existing dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
            "properties": {"atlas_id": "Canonical.Doc-42"},
            "source_uri": "atlas:Canonical.Doc-42",
            "source_type": "manual",
            "finalized": true
        }] }),
    )
    .await
    .expect("seed legacy identity");

    let root = TempDir::new().expect("temp root");
    let path = root.path().join("canonical.md");
    std::fs::write(
        &path,
        "---\nid: Canonical.Doc-42\n---\n# Canonical\n\nThis body contains enough meaningful prose for import validation while exercising duplicate prevention against a differently slugged existing canonical identity.\n",
    )
    .expect("canonical source");

    let error = f
        .dispatch(
            "knowledge.import",
            json!({ "path": path.to_str().expect("utf-8 path") }),
        )
        .await
        .expect_err("ambiguous existing identity must be refused");
    let message = error.to_string();
    assert!(message.contains("legacy-slug"), "{message}");
    assert!(message.contains("canonical-doc-42"), "{message}");
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        1,
        "refusal must not create a near-duplicate"
    );
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

#[tokio::test]
async fn clean_directory_import_still_succeeds_under_the_byte_caps() {
    let f = fixture();
    let root = TempDir::new().expect("temp root");
    for (dir, title) in [("alpha", "Alpha Topic"), ("beta", "Beta Topic")] {
        let nested = root.path().join(dir);
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(nested.join("topic.md"), markdown(title)).expect("markdown fixture");
    }

    let response = f
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect("a clean, well-under-cap import must still succeed");

    assert_eq!(response["imported_atoms"], 2);
    assert_eq!(response["files_processed"], 2);
    assert_eq!(
        f.count("SELECT COUNT(*) AS count FROM knowledge_atoms")
            .await,
        2,
        "both well-under-cap atoms must be persisted"
    );
}
