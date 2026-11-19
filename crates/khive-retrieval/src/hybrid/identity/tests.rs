use super::*;

#[tokio::test]
async fn preserves_original_order() {
    let reranker = IdentityReranker::new();
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.1)),
        (2u32, DeterministicScore::from_f64(0.9)),
        (3u32, DeterministicScore::from_f64(0.5)),
    ];
    let out = reranker.rerank("q", results.clone(), 3).await.unwrap();
    assert_eq!(out, results);
}

#[tokio::test]
async fn truncates_to_top_k() {
    let reranker = IdentityReranker::new();
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.1)),
        (2u32, DeterministicScore::from_f64(0.9)),
        (3u32, DeterministicScore::from_f64(0.5)),
    ];
    let out = reranker.rerank("q", results.clone(), 2).await.unwrap();
    assert_eq!(out, results[..2]);
}

#[tokio::test]
async fn top_k_zero_returns_empty() {
    let reranker = IdentityReranker::new();
    let results = vec![(1u32, DeterministicScore::from_f64(0.1))];
    let out = reranker.rerank("q", results, 0).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn top_k_larger_than_len_returns_all() {
    let reranker = IdentityReranker::new();
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.1)),
        (2u32, DeterministicScore::from_f64(0.9)),
    ];
    let out = reranker.rerank("q", results, 10).await.unwrap();
    assert_eq!(out.len(), 2);
}

#[tokio::test]
async fn deterministic_across_calls() {
    let reranker = IdentityReranker::new();
    let results = vec![
        (1u32, DeterministicScore::from_f64(0.1)),
        (2u32, DeterministicScore::from_f64(0.9)),
        (3u32, DeterministicScore::from_f64(0.5)),
    ];
    let out1 = reranker.rerank("q", results.clone(), 3).await.unwrap();
    let out2 = reranker.rerank("q", results, 3).await.unwrap();
    let ids1: Vec<_> = out1.iter().map(|(id, _)| *id).collect();
    let ids2: Vec<_> = out2.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids1, ids2);
}
