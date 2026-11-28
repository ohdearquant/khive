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

// This fixture simulates a scorer reporting hook use. Its one-element loop
// lets the reranker test observe hook forwarding and the scorer's evidence.
#[derive(Default)]
struct HookAwareScorer {
    calls: std::sync::atomic::AtomicUsize,
    inputs: std::sync::Mutex<Vec<(String, Vec<String>)>>,
}

impl CrossEncoderScorer for HookAwareScorer {
    fn score_batch(&self, query: &str, documents: &[&str]) -> Vec<f32> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.inputs.lock().unwrap().push((
            query.to_string(),
            documents
                .iter()
                .map(|document| (*document).to_string())
                .collect(),
        ));
        vec![0.25; documents.len()]
    }

    fn score_batch_with_hook(
        &self,
        query: &str,
        documents: &[&str],
        hook: Option<&dyn LoraHook>,
    ) -> Result<CrossEncoderScores> {
        let mut scores = self.score_batch(query, documents);
        let observer = hook.map(ObservedHook::new);
        if let Some(observer) = &observer {
            for score in &mut scores {
                if observer.is_active(0, "ffn_output") {
                    observer.apply(0, "ffn_output", &[1.0], std::slice::from_mut(score));
                }
            }
        }
        Ok(CrossEncoderScores {
            scores,
            hook_applied: observer
                .as_ref()
                .is_some_and(|observer| observer.applied.load(Ordering::Relaxed)),
        })
    }
}

struct BorrowedHook<'a> {
    delta: &'a f32,
    calls: &'a std::sync::atomic::AtomicUsize,
    target: &'a str,
}

impl LoraHook for BorrowedHook<'_> {
    fn apply(&self, _layer_idx: usize, _module: &str, _x: &[f32], output: &mut [f32]) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        for value in output {
            *value += self.delta;
        }
    }

    fn is_active(&self, _layer_idx: usize, module: &str) -> bool {
        module == self.target
    }
}

#[tokio::test]
async fn per_call_hook_reaches_scorer_and_is_not_retained() {
    let scorer = Arc::new(HookAwareScorer::default());
    let reranker = NativeCrossEncoderReranker::new(
        scorer.clone(),
        Arc::new(FakeResolver {
            documents: vec![Some("a".into()), None, Some("b".into())],
        }),
    );
    let baseline = reranker
        .rerank("q", candidates(&[1, 2, 3]), 2)
        .await
        .unwrap();
    let delta = 0.5;
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let hook = BorrowedHook {
        delta: &delta,
        calls: &calls,
        target: "ffn_output",
    };
    let adapted = reranker
        .rerank_with_hook("q", candidates(&[1, 2, 3]), 1, Some(&hook))
        .await
        .unwrap();
    assert!(adapted.hook_applied, "per-call hook must reach the scorer");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(
        adapted.results,
        vec![(1, DeterministicScore::from_f64(0.75))]
    );
    let subsequent = reranker
        .rerank_with_hook("q", candidates(&[1, 2, 3]), 2, None)
        .await
        .unwrap();
    assert_eq!(
        subsequent.results, baseline,
        "hook must not survive its call"
    );
    assert!(!subsequent.hook_applied);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(scorer.calls.load(Ordering::Relaxed), 3);
    assert_eq!(
        *scorer.inputs.lock().unwrap(),
        vec![("q".to_string(), vec!["a".to_string(), "b".to_string()]); 3],
        "per-call scoring must preserve the literal query and retained document order"
    );
}

#[tokio::test]
async fn unsupported_hook_is_not_silently_ignored() {
    let scorer = Arc::new(RecordingScorer::default());
    let reranker = NativeCrossEncoderReranker::new(
        scorer.clone(),
        Arc::new(FakeResolver {
            documents: vec![Some("low".into())],
        }),
    );
    let delta = 0.5;
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let active = BorrowedHook {
        delta: &delta,
        calls: &calls,
        target: "ffn_output",
    };
    for hook in [&NoopLoraHook as &dyn LoraHook, &active] {
        let result = reranker
            .rerank_with_hook("q", candidates(&[1]), 1, Some(hook))
            .await;
        assert!(
            matches!(result, Err(RetrievalError::Rerank(ref message))
                if message == "cross-encoder scorer does not support per-call hooks"),
            "opaque scorer must refuse every supplied hook, including Noop"
        );
    }
    assert!(scorer.calls.lock().unwrap().is_empty());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let none = reranker
        .rerank_with_hook("q", candidates(&[1]), 1, None)
        .await
        .unwrap();
    assert_eq!(
        none.results,
        vec![(1, DeterministicScore::from_f64(0.1_f32 as f64))]
    );
    assert!(!none.hook_applied);
    assert_eq!(scorer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn skipped_batches_do_not_score_or_claim_hook_application() {
    let scorer = Arc::new(RecordingScorer::default());
    let reranker = NativeCrossEncoderReranker::new(
        scorer.clone(),
        Arc::new(FakeResolver {
            documents: vec![None],
        }),
    );
    for (input, limit) in [(candidates(&[1]), 0), (vec![], 1), (candidates(&[1]), 1)] {
        let result = reranker
            .rerank_with_hook("q", input, limit, Some(&NoopLoraHook))
            .await
            .unwrap();
        assert!(result.results.is_empty());
        assert!(!result.hook_applied);
    }
    assert!(scorer.calls.lock().unwrap().is_empty());
}

#[test]
fn observer_preserves_validation_activity_and_actual_apply() {
    struct RefusingHook {
        calls: std::sync::Mutex<Vec<&'static str>>,
    }
    impl LoraHook for RefusingHook {
        fn apply(&self, _layer: usize, _module: &str, _x: &[f32], output: &mut [f32]) {
            self.calls.lock().unwrap().push("apply");
            output[0] += 0.5;
        }
        fn validate_geometry(
            &self,
            _geometry: &dyn LoraTargetGeometry,
        ) -> std::result::Result<(), String> {
            self.calls.lock().unwrap().push("geometry");
            Err("geometry refusal".into())
        }
        fn validate_against_bert(
            &self,
            layers: usize,
            hidden: usize,
            intermediate: usize,
        ) -> std::result::Result<(), String> {
            assert_eq!((layers, hidden, intermediate), (1, 2, 4));
            self.calls.lock().unwrap().push("legacy");
            Err("legacy refusal".into())
        }
        fn is_active(&self, layer: usize, module: &str) -> bool {
            assert_eq!((layer, module), (0, "ffn_output"));
            self.calls.lock().unwrap().push("active");
            true
        }
    }
    let hook = RefusingHook {
        calls: std::sync::Mutex::new(Vec::new()),
    };
    let observer = ObservedHook::new(&hook);
    assert_eq!(
        observer.validate_geometry(&BertLoraGeometry::new(1, 2, 4)),
        Err("geometry refusal".into())
    );
    #[allow(deprecated)]
    let legacy = observer.validate_against_bert(1, 2, 4);
    assert_eq!(legacy, Err("legacy refusal".into()));
    assert!(observer.is_active(0, "ffn_output"));
    assert!(
        !observer.applied.load(Ordering::Relaxed),
        "validation and activity must not count as apply"
    );
    let mut output = [0.25];
    observer.apply(0, "ffn_output", &[1.0], &mut output);
    assert_eq!(output, [0.75]);
    assert!(observer.applied.load(Ordering::Relaxed));
    assert_eq!(
        *hook.calls.lock().unwrap(),
        vec!["geometry", "legacy", "active", "apply"]
    );
}
