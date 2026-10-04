//! `knowledge.import` section type markers: a trailing `{type}` on a `##` heading.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;
use tempfile::TempDir;

const BODY: &str = "This section body carries enough text to clear the minimum section \
    length so the import keeps it.";

#[tokio::test]
async fn import_types_sections_by_marker_and_counts_unknown_markers() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());

    let root = TempDir::new().expect("temp root");
    let markdown = format!(
        "# Marker Atom\n\nThis preamble has enough meaningful words for a valid searchable \
         atom so the sections below decide what the import stores and counts.\n\n\
         ## Why it terminates {{core_model}}\n\n{BODY}\n\n\
         ## Why it is fast {{core_model}}\n\n{BODY} Second.\n\n\
         ## Where it applies {{scope}}\n\n{BODY} Third.\n\n\
         ## Mechanism\n\n{BODY} Fourth.\n"
    );
    std::fs::write(root.path().join("marker-atom.md"), markdown).expect("markdown");

    let response = registry
        .dispatch(
            "knowledge.import",
            json!({ "path": root.path().to_str().expect("utf-8 root") }),
        )
        .await
        .expect("import");
    assert_eq!(response["sections_discovered"], 4);
    assert_eq!(response["sections_unknown_type"], 1);
    assert_eq!(response["sections_skipped"], 0);
    assert_eq!(response["imported_sections"], 3);

    let access = rt.sql();
    let mut reader = access.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT section_type, heading FROM knowledge_sections ORDER BY heading"
                .to_string(),
            params: vec![],
            label: None,
        })
        .await
        .expect("section rows");
    let stored = rows
        .iter()
        .map(|row| {
            let text = |column: &str| match row.get(column) {
                Some(SqlValue::Text(value)) => value.clone(),
                other => panic!("expected text {column}, got {other:?}"),
            };
            (text("section_type"), text("heading"))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stored,
        vec![
            ("core_model".to_string(), "Mechanism".to_string()),
            ("core_model".to_string(), "Why it is fast".to_string()),
            ("core_model".to_string(), "Why it terminates".to_string()),
        ]
    );
}
