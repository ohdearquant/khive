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
async fn missing_documents_keep_surviving_ids_and_scores_aligned() {
    let scorer = Arc::new(RecordingScorer::default());
    let reranker = NativeCrossEncoderReranker::new(
        scorer.clone(),
        Arc::new(FakeResolver {
            documents: vec![Some("low".into()), None, Some("high".into())],
        }),
    );
    let out = reranker
        .rerank("query", candidates(&[10, 20, 30]), 3)
        .await
        .unwrap();
    assert_eq!(
        out,
        vec![
            (30, DeterministicScore::from_f64(0.9_f32 as f64)),
            (10, DeterministicScore::from_f64(0.1_f32 as f64))
        ]
    );
    assert_eq!(
        *scorer.calls.lock().unwrap(),
        vec![(
            "query".to_string(),
            vec!["low".to_string(), "high".to_string()]
        )]
    );
    let all_present = NativeCrossEncoderReranker::new(
        Arc::new(RecordingScorer::default()),
        Arc::new(FakeResolver {
            documents: vec![Some("low".into()), Some("high".into())],
        }),
    );
    assert_eq!(
        all_present
            .rerank("query", candidates(&[10, 30]), 3)
            .await
            .unwrap(),
        out
    );
}

#[derive(Default)]
struct RecordingScorer {
    calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

impl CrossEncoderScorer for RecordingScorer {
    fn score_batch(&self, query: &str, documents: &[&str]) -> Vec<f32> {
        self.calls.lock().unwrap().push((
            query.into(),
            documents.iter().map(|text| (*text).into()).collect(),
        ));
        documents
            .iter()
            .map(|text| match *text {
                "low" => 0.1,
                "high" => 0.9,
                _ => panic!("unexpected document text"),
            })
            .collect()
    }
}

fn candidates(ids: &[u32]) -> Vec<(u32, DeterministicScore)> {
    ids.iter()
        .map(|id| (*id, DeterministicScore::from_f64(0.5)))
        .collect()
}

#[tokio::test]
async fn all_missing_documents_do_not_call_the_scorer() {
    let scorer = Arc::new(RecordingScorer::default());
    let reranker = NativeCrossEncoderReranker::new(
        scorer.clone(),
        Arc::new(FakeResolver {
            documents: vec![None, None],
        }),
    );
    assert!(reranker
        .rerank("query", candidates(&[10, 20]), 2)
        .await
        .unwrap()
        .is_empty());
    assert!(scorer.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn limit_applies_after_missing_documents_and_scoring() {
    let reranker = NativeCrossEncoderReranker::new(
        Arc::new(RecordingScorer::default()),
        Arc::new(FakeResolver {
            documents: vec![None, Some("low".into()), Some("high".into())],
        }),
    );
    assert_eq!(
        reranker
            .rerank("query", candidates(&[10, 20, 30]), 1)
            .await
            .unwrap(),
        vec![(30, DeterministicScore::from_f64(0.9_f32 as f64))]
    );
}

#[tokio::test]
async fn missing_documents_preserve_tie_order_and_duplicate_ids() {
    let reranker = make_reranker(
        vec![0.5, 0.5, 0.5],
        vec![Some("a".into()), None, Some("b".into()), Some("a".into())],
    );
    assert_eq!(
        reranker
            .rerank("q", candidates(&[30, 99, 10, 30]), 4)
            .await
            .unwrap(),
        candidates(&[30, 10, 30])
    );
}

#[tokio::test]
async fn scorer_length_must_match_retained_documents() {
    let reranker = make_reranker(
        vec![0.1, 0.2, 0.3],
        vec![Some("a".into()), None, Some("b".into())],
    );
    let error = reranker
        .rerank("q", candidates(&[10, 20, 30]), 3)
        .await
        .unwrap_err();
    assert!(
        matches!(error, RetrievalError::Rerank(message) if message == "model returned 3 scores for 2 candidates")
    );
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
