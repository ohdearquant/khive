use super::*;

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

fn make_reranker(
    scores: Vec<f32>,
    documents: Vec<Option<String>>,
) -> NativeCrossEncoderReranker<u32, FakeResolver, FakeScorer> {
    NativeCrossEncoderReranker::new(
        Arc::new(FakeScorer { scores }),
        Arc::new(FakeResolver { documents }),
    )
}

#[tokio::test]
async fn test_top_k_zero_returns_empty() {
    let reranker = make_reranker(vec![0.9, 0.1], vec![Some("a".into()), Some("b".into())]);
    let results = vec![(1u32, DeterministicScore::from_f64(0.5))];
    let out = reranker.rerank("q", results, 0).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn test_empty_input_returns_empty() {
    let reranker = make_reranker(vec![], vec![]);
    let out = reranker.rerank("q", vec![], 5).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn test_descending_sort() {
    let reranker = make_reranker(
        vec![0.1, 0.9, 0.5],
        vec![Some("a".into()), Some("b".into()), Some("c".into())],
    );
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.3)),
        (2u32, DeterministicScore::from_f64(0.3)),
        (3u32, DeterministicScore::from_f64(0.3)),
    ];
    let out = reranker.rerank("q", results, 3).await.unwrap();
    assert_eq!(out[0].0, 2u32); // score 0.9
    assert_eq!(out[1].0, 3u32); // score 0.5
    assert_eq!(out[2].0, 1u32); // score 0.1
}

#[tokio::test]
async fn test_tie_preserves_original_order() {
    let reranker = make_reranker(
        vec![0.5, 0.5, 0.5],
        vec![Some("a".into()), Some("b".into()), Some("c".into())],
    );
    let results = vec![
        (10u32, DeterministicScore::from_f64(0.3)),
        (20u32, DeterministicScore::from_f64(0.3)),
        (30u32, DeterministicScore::from_f64(0.3)),
    ];
    let out = reranker.rerank("q", results, 3).await.unwrap();
    assert_eq!(out[0].0, 10u32);
    assert_eq!(out[1].0, 20u32);
    assert_eq!(out[2].0, 30u32);
}

#[tokio::test]
async fn test_missing_document_returns_error() {
    let reranker = make_reranker(vec![0.5], vec![None]);
    let results = vec![(1u32, DeterministicScore::from_f64(0.5))];
    let err = reranker.rerank("q", results, 1).await.unwrap_err();
    assert!(matches!(err, RetrievalError::Rerank(_)));
}

#[tokio::test]
async fn test_resolver_length_mismatch_returns_error() {
    struct BadResolver;

    #[async_trait]
    impl RerankDocumentResolver<u32> for BadResolver {
        async fn resolve_documents(&self, _ids: &[u32]) -> Result<Vec<Option<String>>> {
            Ok(vec![]) // wrong length
        }
    }

    let reranker = NativeCrossEncoderReranker::new(
        Arc::new(FakeScorer { scores: vec![0.5] }),
        Arc::new(BadResolver),
    );
    let results = vec![(1u32, DeterministicScore::from_f64(0.5))];
    let err = reranker.rerank("q", results, 1).await.unwrap_err();
    assert!(matches!(err, RetrievalError::Rerank(_)));
}

#[tokio::test]
async fn test_top_k_limits_output() {
    let reranker = make_reranker(
        vec![0.9, 0.8, 0.7],
        vec![Some("a".into()), Some("b".into()), Some("c".into())],
    );
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.3)),
        (2u32, DeterministicScore::from_f64(0.3)),
        (3u32, DeterministicScore::from_f64(0.3)),
    ];
    let out = reranker.rerank("q", results, 2).await.unwrap();
    assert_eq!(out.len(), 2);
}

#[tokio::test]
async fn test_top_k_larger_than_results_returns_all() {
    // top_k=10 with only 2 candidates — should return all 2, sorted by score
    let reranker = make_reranker(vec![0.1, 0.9], vec![Some("a".into()), Some("b".into())]);
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.5)),
        (2u32, DeterministicScore::from_f64(0.3)),
    ];
    let out = reranker.rerank("q", results, 10).await.unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0, 2u32); // score 0.9
    assert_eq!(out[1].0, 1u32); // score 0.1
}

#[tokio::test]
async fn test_single_result_passes_through() {
    let reranker = make_reranker(vec![0.75], vec![Some("only doc".into())]);
    let results = vec![(42u32, DeterministicScore::from_f64(0.5))];
    let out = reranker.rerank("q", results, 1).await.unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, 42u32);
}
