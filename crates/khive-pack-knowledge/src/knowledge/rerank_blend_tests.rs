use super::*;

const MODEL: &str = "all-minilm-l6-v2";
const DIMENSIONS: usize = 384;
const QUERY: &str = "exact rerank blend query";
const POSITIVE_ID: &str = "45140000-0000-0000-0000-000000000001";
const NEGATIVE_ID: &str = "45140000-0000-0000-0000-000000000002";

struct ExactBlendEmbedder;

fn embedding(text: &str) -> Vec<f32> {
    // Norms 1, 4 and 2 give exactly representable cosines 0.25 and -0.5.
    let components = if text.starts_with(POSITIVE_ID) {
        [1.0, 3.0, 2.0, 1.0, 1.0]
    } else if text.starts_with(NEGATIVE_ID) {
        [-1.0, 1.0, 1.0, 1.0, 0.0]
    } else {
        [1.0, 0.0, 0.0, 0.0, 0.0]
    };
    let mut vector = vec![0.0; DIMENSIONS];
    vector[..components.len()].copy_from_slice(&components);
    vector
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for ExactBlendEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|text| embedding(text)).collect())
    }

    async fn embed_passage(
        &self,
        texts: &[String],
        model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.embed(texts, model).await
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "exact-blend-test"
    }
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for ExactBlendEmbedder {
    fn name(&self) -> &str {
        MODEL
    }

    fn dimensions(&self) -> usize {
        DIMENSIONS
    }

    async fn build(&self) -> Result<Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(Arc::new(Self))
    }
}

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        embedding_model: Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["knowledge".into()],
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory rerank runtime");
    runtime.register_embedder(ExactBlendEmbedder);
    runtime
}

fn hits(scores: [f32; 2]) -> Vec<ScoredHit> {
    vec![
        make_hit(NEGATIVE_ID, None, scores[1]),
        make_hit(POSITIVE_ID, None, scores[0]),
    ]
}

fn assert_scores_and_order(hits: &[ScoredHit], expected: [f32; 2]) {
    let actual: Vec<_> = hits
        .iter()
        .map(|hit| (hit.id.as_str(), hit.score.to_bits()))
        .collect();
    assert_eq!(
        actual,
        [
            (POSITIVE_ID, expected[0].to_bits()),
            (NEGATIVE_ID, expected[1].to_bits()),
        ]
    );
    assert!(hits.iter().all(|hit| hit.provenance.embedding_rerank));
}

#[tokio::test]
async fn generic_rerank_pins_exact_f32_scores_and_order() {
    let runtime = runtime();
    for (scores, expected) in [([4.0, 2.0], [0.625, 0.25]), ([0.0, 0.0], [0.125, 0.0])] {
        let mut hits = hits(scores);
        let applied = rerank_with_embeddings(
            &runtime,
            QUERY,
            &mut QueryEmbeddingCache::default(),
            &mut hits,
            0.5,
        )
        .await
        .expect("generic rerank");
        assert!(applied);
        assert_scores_and_order(&hits, expected);
    }
}

#[tokio::test]
async fn store_rerank_pins_exact_f32_scores_and_order() {
    let runtime = runtime();
    let store = crate::knowledge::a5_rerank_tests::NoReadStore;
    for (scores, expected) in [([4.0, 2.0], [0.625, 0.25]), ([0.0, 0.0], [0.125, 0.0])] {
        let mut hits = hits(scores);
        let provenance = rerank_search_from_store(
            &runtime,
            Some(&store),
            "local",
            &embedding(QUERY),
            &mut hits,
            0.5,
        )
        .await
        .expect("store rerank")
        .expect("document fallback reranked");
        assert_eq!(provenance["stored_vector_lookup"], "unsupported");
        assert_eq!(provenance["from_stored"], 0);
        assert_eq!(provenance["embedded_fallback"], 2);
        assert_scores_and_order(&hits, expected);
    }
}
