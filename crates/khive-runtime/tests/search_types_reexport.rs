//! The search result types are defined in `khive-retrieval` and re-exported by
//! `khive-runtime`: both crate paths name one type, not two copies.

use khive_runtime::{HybridSearchOutcome, RankScoreKind, SearchHit, SearchSignals, SearchSource};
use khive_score::DeterministicScore;
use uuid::Uuid;

fn retrieval_hit(hit: khive_retrieval::SearchHit) -> khive_retrieval::SearchHit {
    hit
}

fn retrieval_outcome(
    outcome: khive_retrieval::HybridSearchOutcome,
) -> khive_retrieval::HybridSearchOutcome {
    outcome
}

fn retrieval_kind(kind: khive_retrieval::RankScoreKind) -> khive_retrieval::RankScoreKind {
    kind
}

fn retrieval_signals(signals: khive_retrieval::SearchSignals) -> khive_retrieval::SearchSignals {
    signals
}

fn retrieval_source(source: khive_retrieval::SearchSource) -> khive_retrieval::SearchSource {
    source
}

#[test]
fn runtime_search_types_are_the_retrieval_types() {
    let entity_id = Uuid::from_u128(7);
    let score = DeterministicScore::from_f64(0.5);
    let hit = SearchHit {
        entity_id,
        score,
        rank_score_kind: RankScoreKind::Rrf,
        signals: SearchSignals {
            vector_similarity: Some(DeterministicScore::from_f64(0.25)),
            keyword_score: None,
        },
        source: SearchSource::Both,
        title: Some("title".to_string()),
        snippet: None,
    };

    let hit = retrieval_hit(hit);
    assert_eq!(hit.entity_id, entity_id);
    assert_eq!(hit.score, score);
    assert_eq!(hit.title.as_deref(), Some("title"));

    let kind = retrieval_kind(hit.rank_score_kind);
    assert_eq!(kind.as_str(), "rrf");

    let signals = retrieval_signals(hit.signals);
    assert!(signals.vector_similarity.is_some());
    assert!(signals.keyword_score.is_none());

    let source = retrieval_source(hit.source);
    assert_eq!(source.as_str(), "both");

    let outcome = HybridSearchOutcome {
        hits: vec![hit],
        vector_error: Some("vector failed".to_string()),
    };
    let outcome = retrieval_outcome(outcome);
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.vector_error.as_deref(), Some("vector failed"));
}
