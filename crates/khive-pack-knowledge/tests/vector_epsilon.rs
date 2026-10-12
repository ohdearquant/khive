//! Real section writes use the same near-zero normalization cutoff as ANN.

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use std::sync::Arc;

const DIM: usize = 384;
const MODEL: &str = "all-minilm-l6-v2";
const MARKER: &str = "tiny-vector-control";

struct EpsilonService;

#[async_trait]
impl EmbeddingService for EpsilonService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut vector = vec![0.0; DIM];
                if text.contains(MARKER) {
                    vector[0] = 5e-9;
                } else {
                    vector[0] = 3.0;
                    vector[1] = 4.0;
                }
                vector
            })
            .collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(texts, model).await
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "epsilon-control-service"
    }
}

struct EpsilonProvider;

#[async_trait]
impl EmbedderProvider for EpsilonProvider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        DIM
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(EpsilonService))
    }
}

#[tokio::test]
async fn section_writes_preserve_tiny_vectors_and_normalize_ordinary_vectors() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["kg".into(), "knowledge".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory knowledge runtime");
    runtime.register_embedder(EpsilonProvider);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(KnowledgePack::new(runtime.clone()));
    let registry = builder.build().expect("knowledge registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.dispatch("knowledge.upsert_atoms", json!({
        "namespace": "local",
        "atoms": [{"slug": "epsilon-control", "name": "Epsilon Control", "content":
            "A control atom about vector normalization and section persistence, with enough ordinary text to be independently embedded and read back through the section index."}]
    })).await.expect("upsert control atom");
    registry.dispatch("knowledge.edit", json!({
        "namespace": "local", "id": "epsilon-control", "sections": [
            {"section_type": "overview", "content": format!(
                "{MARKER}: a section with enough descriptive content to clear the minimum length and exercise the actual inline embedding write.")},
            {"section_type": "core_model", "content":
                "An ordinary section with enough descriptive content to clear the minimum length and provide a positive normalized embedding control."}
        ]
    })).await.expect("edit and embed both sections");
    let atom = registry
        .dispatch(
            "knowledge.get",
            json!({
                "namespace": "local", "id": "epsilon-control"
            }),
        )
        .await
        .expect("get control atom");
    let atom_id = atom["id"].as_str().expect("atom UUID");
    let rows = runtime.sql().reader().await.expect("section reader")
        .query_all(SqlStatement {
            sql: "SELECT section_type, embedding FROM knowledge_sections WHERE atom_id = ?1 AND namespace = ?2".into(),
            params: vec![SqlValue::Text(atom_id.to_owned()), SqlValue::Text("local".into())],
            label: None,
        }).await.expect("read actual persisted section vectors");
    assert_eq!(rows.len(), 2, "both real writes must be present");
    let mut seen_types = Vec::new();
    for row in rows {
        let section_type = match row.get("section_type") {
            Some(SqlValue::Text(value)) => value.as_str(),
            other => panic!("section type is not text: {other:?}"),
        };
        seen_types.push(section_type.to_owned());
        let blob = match row.get("embedding") {
            Some(SqlValue::Blob(value)) => value,
            other => panic!("section vector was not persisted: {other:?}"),
        };
        let vector = khive_storage::decode_f32_le(blob).expect("little-endian section vector");
        assert_eq!(vector.len(), DIM);
        match section_type {
            "overview" => {
                assert_eq!(vector[0].to_bits(), 5e-9_f32.to_bits());
                assert!(vector[1..].iter().all(|value| *value == 0.0));
            }
            "core_model" => {
                assert!((vector[0] - 0.6).abs() < 1e-6);
                assert!((vector[1] - 0.8).abs() < 1e-6);
                assert!(vector[2..].iter().all(|value| *value == 0.0));
            }
            other => panic!("unexpected section type: {other}"),
        }
    }
    seen_types.sort();
    assert_eq!(seen_types, vec!["core_model", "overview"]);
}
