#![cfg(feature = "native-rerank")]

use std::sync::Arc;

use async_trait::async_trait;
use khive_retrieval::{
    CrossEncoderScorer, IdentityReranker, NativeCrossEncoderReranker, RerankDocumentResolver,
    Reranker, Result,
};
use khive_score::DeterministicScore;

struct FakeScorer {
    scores: Vec<f32>,
}

impl CrossEncoderScorer for FakeScorer {
    fn score_batch(&self, _query: &str, _documents: &[&str]) -> Vec<f32> {
        self.scores.clone()
    }
}

struct FakeResolver {
    documents: Vec<Option<String>>,
}

#[async_trait]
impl RerankDocumentResolver<u32> for FakeResolver {
    async fn resolve_documents(&self, _ids: &[u32]) -> Result<Vec<Option<String>>> {
        Ok(self.documents.clone())
    }
}

#[tokio::test]
async fn native_cross_encoder_reranker_reexports_compile_and_run() {
    let reranker = NativeCrossEncoderReranker::new(
        Arc::new(FakeScorer {
            scores: vec![0.1, 0.9],
        }),
        Arc::new(FakeResolver {
            documents: vec![Some("a".into()), Some("b".into())],
        }),
    );
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.5)),
        (2u32, DeterministicScore::from_f64(0.5)),
    ];
    let out = reranker.rerank("q", results, 2).await.unwrap();
    assert_eq!(out[0].0, 2u32);
    assert_eq!(out[1].0, 1u32);
}

#[tokio::test]
async fn identity_reranker_reexport_compiles_and_runs() {
    let reranker = IdentityReranker::<u32>::new();
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.5)),
        (2u32, DeterministicScore::from_f64(0.9)),
    ];
    let out = reranker.rerank("q", results.clone(), 1).await.unwrap();
    assert_eq!(out, results[..1]);
}
