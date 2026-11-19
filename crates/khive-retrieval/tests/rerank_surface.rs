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

#[tokio::test]
async fn hook_surface_preserves_legacy_reranker_and_borrowed_invocation() {
    use khive_retrieval::{CrossEncoderScores, LoraHook, NativeRerankResult, NoopLoraHook};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct BorrowedHook<'a>(&'a AtomicUsize);
    impl LoraHook for BorrowedHook<'_> {
        fn apply(&self, _: usize, _: &str, _: &[f32], _: &mut [f32]) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let calls = AtomicUsize::new(0);
    let hook = BorrowedHook(&calls);
    let scorer = Arc::new(FakeScorer {
        scores: vec![0.1, 0.9],
    });
    let opaque: &dyn CrossEncoderScorer = scorer.as_ref();
    let plain: CrossEncoderScores = opaque
        .score_batch_with_hook("q", &["a", "b"], None)
        .unwrap();
    assert_eq!(plain.scores, vec![0.1, 0.9]);
    assert!(!plain.hook_applied);
    for supplied in [&hook as &dyn LoraHook, &NoopLoraHook] {
        assert!(
            matches!(
                opaque.score_batch_with_hook("q", &["a", "b"], Some(supplied)),
                Err(khive_retrieval::RetrievalError::Rerank(_))
            ),
            "legacy scorer must explicitly refuse every supplied hook"
        );
    }
    let reranker = NativeCrossEncoderReranker::new(
        scorer,
        Arc::new(FakeResolver {
            documents: vec![Some("a".into()), Some("b".into())],
        }),
    );
    let input = vec![
        (1u32, DeterministicScore::from_f64(0.5)),
        (2u32, DeterministicScore::from_f64(0.5)),
    ];
    let legacy = reranker.rerank("q", input.clone(), 2).await.unwrap();
    let explicit: NativeRerankResult<u32> = reranker
        .rerank_with_hook("q", input.clone(), 2, None)
        .await
        .unwrap();
    assert_eq!(explicit.results, legacy);
    assert_eq!(
        explicit.results.iter().map(|row| row.0).collect::<Vec<_>>(),
        vec![2, 1]
    );
    assert!(!explicit.hook_applied);
    for supplied in [&hook as &dyn LoraHook, &NoopLoraHook] {
        assert!(
            matches!(
                reranker
                    .rerank_with_hook("q", input.clone(), 2, Some(supplied))
                    .await,
                Err(khive_retrieval::RetrievalError::Rerank(_))
            ),
            "public reranker must preserve unsupported-hook refusal"
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(reranker.rerank("q", input, 2).await.unwrap(), legacy);
}
