//! Import option errors retain useful operands without exposing credentials.
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use tempfile::TempDir;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private memory runtime");
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("knowledge schema");
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            runtime,
            registry,
            root: TempDir::new().unwrap(),
        }
    }

    async fn counts(&self) -> Vec<i64> {
        let access = self.runtime.sql();
        let mut reader = access.reader().await.unwrap();
        let mut counts = Vec::new();
        for table in ["knowledge_atoms", "knowledge_sections", "knowledge_domains"] {
            let row = reader
                .query_row(SqlStatement {
                    sql: format!("SELECT COUNT(*) AS count FROM {table}"),
                    params: vec![],
                    label: None,
                })
                .await
                .unwrap()
                .unwrap();
            let Some(SqlValue::Integer(count)) = row.get("count") else {
                panic!("expected count");
            };
            counts.push(*count);
        }
        counts
    }

    async fn refuse(&self, field: &str, raw: &str, expected: &str, credential: bool) {
        // A missing path makes an accidental fall-through to filesystem validation observable.
        let path = self.root.path().join("does-not-exist.md");
        let mut params = json!({"path": path});
        params[field] = json!(raw);
        let before = self.counts().await;
        let error = self
            .registry
            .dispatch("knowledge.import", params)
            .await
            .expect_err("invalid import option");
        let RuntimeError::InvalidInput(message) = &error else {
            panic!("option refusal changed variant or reached filesystem: {error}");
        };
        if credential {
            assert!(
                !message.contains(raw),
                "import refusal exposed the credential"
            );
            assert!(
                !message.contains(&raw.to_ascii_lowercase()),
                "import refusal exposed the normalized credential"
            );
            assert!(
                message.contains("***MASKED***"),
                "import refusal omitted the mask"
            );
        }
        assert_eq!(message, expected);
        assert_eq!(
            runtime_error_value(error, DomainDisposition::NotCommitted),
            json!({
                "kind": "runtime_error",
                "message": format!("invalid input: {expected}"),
                "domain_disposition": "not_committed"
            })
        );
        assert_eq!(self.counts().await, before, "refusal changed the corpus");
        assert!(!path.exists(), "refusal created its missing source");
    }

    async fn import(&self, name: &str, options: Value) -> Value {
        let path = self.root.path().join(format!("{name}.md"));
        std::fs::write(&path, format!(
            "# {name}\n\nThis imported markdown document contains enough meaningful words to satisfy atom content validation while preserving deterministic knowledge corpus identity and source provenance for accepted import options.\n"
        )).unwrap();
        let mut params = options;
        params["path"] = json!(path);
        let result = self
            .registry
            .dispatch("knowledge.import", params)
            .await
            .expect("accepted options still import");
        assert_eq!(result["files_discovered"], 1);
        assert_eq!(result["imported_atoms"], 1);
        let stored = self
            .registry
            .dispatch("knowledge.get", json!({"id": name}))
            .await
            .expect("imported atom is readable");
        assert_eq!(stored["kind"], "atom");
        result
    }
}

#[tokio::test]
async fn chunk_strategy_refusal_masks_credentials_and_retains_case_normalization() {
    let f = Fixture::new();
    f.refuse(
        "chunk_strategy",
        "BOGUS",
        "unknown chunk_strategy \"bogus\"; valid: section | atom",
        false,
    )
    .await;
    let secret = format!("sk-proj-{}", "A".repeat(80));
    f.refuse(
        "chunk_strategy",
        &secret,
        "unknown chunk_strategy \"***MASKED***\"; valid: section | atom",
        true,
    )
    .await;
    f.import("accepted-atom", json!({"chunk_strategy": "ATOM"}))
        .await;
    f.import("accepted-section", json!({"chunk_strategy": "SeCtIoN"}))
        .await;
    assert_eq!(f.counts().await[0], 2);
}

#[tokio::test]
async fn format_refusal_masks_credentials_and_supported_default_still_imports() {
    let f = Fixture::new();
    f.refuse(
        "format",
        "bogus",
        "unknown format \"bogus\"; only \"atlas_md\" is supported",
        false,
    )
    .await;
    let secret = format!("sk-proj-{}", "B".repeat(80));
    f.refuse(
        "format",
        &secret,
        "unknown format \"***MASKED***\"; only \"atlas_md\" is supported",
        true,
    )
    .await;
    f.import("accepted-format", json!({"format": "atlas_md"}))
        .await;
    f.import("accepted-default", json!({})).await;
    assert_eq!(f.counts().await[0], 2);
}
