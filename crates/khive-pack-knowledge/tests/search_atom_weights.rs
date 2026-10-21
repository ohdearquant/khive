//! Public search coverage for persisted, namespace-scoped atom weights.
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};

const CONTENT: &str = "weightprobe retrieval fixture gives both atoms identical evidence for deterministic ranking while learned weights vary independently of source text status namespace and embedding similarity";

struct EqualVectors;

#[async_trait]
impl EmbeddingService for EqualVectors {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|_| {
                let mut vector = vec![0.0; 384];
                vector[0] = 1.0;
                vector
            })
            .collect())
    }

    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "atom-weight-equal-vectors"
    }
}

struct EqualVectorProvider;

#[async_trait]
impl EmbedderProvider for EqualVectorProvider {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }
    fn dimensions(&self) -> usize {
        384
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(EqualVectors))
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new(embedding: bool) -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: embedding.then_some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            packs: vec!["kg".into(), "knowledge".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private in-memory runtime");
        assert!(runtime.backend().pool().canonical_path().is_none());
        if embedding {
            runtime.register_embedder(EqualVectorProvider);
        }
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn seed_atoms(&self) {
        let response = self.registry.dispatch("knowledge.upsert_atoms", json!({
            "atoms": [
                {"slug": "weight-alpha", "name": "weightprobe", "content": CONTENT, "finalized": true},
                {"slug": "weight-beta", "name": "weightprobe", "content": CONTENT, "finalized": true}
            ]
        })).await.expect("seed equal-evidence atoms");
        assert_eq!(response["created"], 2);
    }

    async fn search(&self, overrides: Value) -> Value {
        let mut args = json!({"query": "weightprobe", "kind": "atom", "limit": 2, "rerank": false});
        args.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        self.registry
            .dispatch("knowledge.search", args)
            .await
            .expect("public search")
    }

    async fn write(&self, sql: &str, params: Vec<SqlValue>) {
        let access = self.runtime.sql();
        let mut writer = access.writer().await.expect("fixture writer");
        assert_eq!(
            writer
                .execute(SqlStatement::new(sql, params))
                .await
                .expect("fixture write"),
            1
        );
    }

    async fn weight(&self, namespace: &str, id: &str, weight: f64) {
        self.write(
            "INSERT INTO atom_weights(namespace, atom_id, weight, updated_at) VALUES(?1, ?2, ?3, 1) \
             ON CONFLICT(namespace, atom_id) DO UPDATE SET weight=excluded.weight",
            vec![SqlValue::Text(namespace.into()), SqlValue::Text(id.into()), SqlValue::Float(weight)],
        ).await;
    }
}

fn rows(response: &Value) -> &[Value] {
    response["results"].as_array().expect("results array")
}

fn score(hit: &Value) -> f32 {
    hit["score"].as_f64().expect("numeric score") as f32
}

fn assert_healthy_weight(hit: &Value, weight: f64) {
    assert_eq!(hit["atom_weight"].as_f64(), Some(weight));
}

#[tokio::test]
async fn stored_weights_promote_limit_one_and_deletion_restores_the_tie_without_a_cache() {
    let f = Fixture::new(false);
    f.seed_atoms().await;
    let baseline = f.search(json!({})).await;
    let original = rows(&baseline);
    assert_eq!(original.len(), 2);
    assert_eq!(original[0]["slug"], "weight-alpha");
    assert_eq!(original[1]["slug"], "weight-beta");
    assert_eq!(score(&original[0]).to_bits(), score(&original[1]).to_bits());
    assert!(score(&original[0]) > 0.0);
    for hit in original {
        assert_eq!(hit["kind"], "atom");
        assert_healthy_weight(hit, 1.0);
    }
    assert!(baseline["degraded"]["atom_weights_unavailable"].is_null());
    let beta = original[1]["id"].as_str().unwrap();

    f.weight("other-namespace", beta, 5.0).await;
    let isolated = f.search(json!({})).await;
    assert_eq!(
        rows(&isolated),
        original,
        "another namespace cannot change the score or weight"
    );

    f.weight("local", beta, 1.5).await;
    let promoted = f.search(json!({"limit": 1})).await;
    assert_eq!(rows(&promoted).len(), 1);
    assert_eq!(rows(&promoted)[0]["id"], beta);
    assert_healthy_weight(&rows(&promoted)[0], 1.5);
    assert_eq!(
        score(&rows(&promoted)[0]).to_bits(),
        (score(&original[1]) * 1.5).to_bits()
    );
    let both = f.search(json!({})).await;
    assert_eq!(rows(&both)[1]["slug"], "weight-alpha");
    assert_eq!(
        score(&rows(&both)[1]).to_bits(),
        score(&original[0]).to_bits()
    );

    f.write(
        "DELETE FROM atom_weights WHERE namespace=?1 AND atom_id=?2",
        vec![SqlValue::Text("local".into()), SqlValue::Text(beta.into())],
    )
    .await;
    let restored = f.search(json!({})).await;
    assert_eq!(
        rows(&restored),
        original,
        "fresh request must observe the deleted weight"
    );
}

#[tokio::test]
async fn score_floor_runs_after_weighting_and_weighted_scores_are_not_clamped() {
    let f = Fixture::new(false);
    f.seed_atoms().await;
    let baseline = f.search(json!({})).await;
    let beta = rows(&baseline)[1]["id"].as_str().unwrap();
    f.weight("local", beta, 5.0).await;
    let boosted = f.search(json!({"limit": 1})).await;
    assert!(
        score(&rows(&boosted)[0]) > 1.0,
        "weighted score must not be clamped"
    );

    // Raw exact-name score .1 is below .2, but its normalized weighted score is above it.
    let admitted = f
        .search(json!({"weights": {"w_exact_name": 0.1, "w_name": 0.0, "w_tags": 0.0, "w_content": 0.0, "w_bigram": 0.0}, "min_score": 0.2}))
        .await;
    assert_eq!(
        rows(&admitted).len(),
        1,
        "early lexical floor must not lose a boosted candidate: {admitted}"
    );
    assert_eq!(rows(&admitted)[0]["id"], beta);
    assert!(score(&rows(&admitted)[0]) >= 0.2);

    f.weight("local", beta, 0.1).await;
    let filtered = f
        .search(json!({"weights": {"w_exact_name": 0.1, "w_name": 0.0, "w_tags": 0.0, "w_content": 0.0, "w_bigram": 0.0}, "min_score": 0.05}))
        .await;
    assert_eq!(rows(&filtered).len(), 1);
    assert_eq!(rows(&filtered)[0]["slug"], "weight-alpha");
    assert!(score(&rows(&filtered)[0]) >= 0.05);
}

#[tokio::test]
async fn domain_hits_report_one_even_when_a_weight_row_exists_for_their_id() {
    let f = Fixture::new(false);
    let seeded = f.registry.dispatch("knowledge.upsert_domains", json!({"domains": [{
        "slug": "weight-domain", "name": "weightprobe", "description": CONTENT, "members": []
    }]})).await.expect("seed domain");
    assert_eq!(seeded["created"], 1);
    let baseline = f
        .search(json!({"kind": "domain", "include_drafts": true}))
        .await;
    assert_eq!(rows(&baseline).len(), 1);
    assert_eq!(rows(&baseline)[0]["kind"], "domain");
    assert_healthy_weight(&rows(&baseline)[0], 1.0);
    f.weight("local", rows(&baseline)[0]["id"].as_str().unwrap(), 5.0)
        .await;
    let weighted = f
        .search(json!({"kind": "domain", "include_drafts": true}))
        .await;
    assert_eq!(rows(&weighted), rows(&baseline));
    assert!(weighted["degraded"]["atom_weights_unavailable"].is_null());
}

#[tokio::test]
async fn learned_weight_multiplies_the_completed_embedding_blend() {
    let f = Fixture::new(true);
    f.seed_atoms().await;
    let baseline = f.search(json!({"rerank": true})).await;
    assert_eq!(rows(&baseline).len(), 2);
    for hit in rows(&baseline) {
        assert_eq!(hit["kind"], "atom");
        assert_eq!(
            hit["score_provenance"]["embedding_rerank"], true,
            "real rerank must execute: {baseline}"
        );
        assert_healthy_weight(hit, 1.0);
    }
    assert_eq!(
        score(&rows(&baseline)[0]).to_bits(),
        score(&rows(&baseline)[1]).to_bits()
    );
    let beta = rows(&baseline)[1]["id"].as_str().unwrap();
    f.weight("local", beta, 1.5).await;
    let weighted = f.search(json!({"rerank": true, "limit": 1})).await;
    assert_eq!(rows(&weighted).len(), 1);
    assert_eq!(rows(&weighted)[0]["id"], beta);
    assert_eq!(
        rows(&weighted)[0]["score_provenance"]["embedding_rerank"],
        true
    );
    assert_eq!(
        score(&rows(&weighted)[0]).to_bits(),
        (score(&rows(&baseline)[1]) * 1.5).to_bits()
    );
    assert_healthy_weight(&rows(&weighted)[0], 1.5);
}

#[tokio::test]
async fn weight_read_failure_retains_nonempty_public_results_with_a_storage_reason() {
    let f = Fixture::new(false);
    f.seed_atoms().await;
    let baseline = f.search(json!({})).await;
    assert_eq!(rows(&baseline).len(), 2);
    {
        let access = f.runtime.sql();
        let mut writer = access.writer().await.expect("private fixture writer");
        writer
            .execute(SqlStatement::new("DROP TABLE atom_weights", vec![]))
            .await
            .expect("inject a real weight-query failure in this private database");
    }
    let degraded = f.search(json!({})).await;
    assert_eq!(
        rows(&degraded),
        rows(&baseline),
        "failed optional weights must retain lexical results"
    );
    assert_eq!(degraded["degraded"]["atom_weights_unavailable"], "storage");
    for hit in rows(&degraded) {
        assert_healthy_weight(hit, 1.0);
    }
}
