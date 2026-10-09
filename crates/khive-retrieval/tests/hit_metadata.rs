use std::collections::HashMap;

use khive_retrieval::hit::{merge_hit_metadata, SignalMerge};
use khive_retrieval::{FusionStrategy, RankScoreKind, SearchHit, SearchSignals, SearchSource};
use khive_score::DeterministicScore;
use uuid::Uuid;

fn hit(id: u128, source: SearchSource) -> SearchHit {
    SearchHit {
        entity_id: Uuid::from_u128(id),
        score: DeterministicScore::from_raw(17),
        rank_score_kind: RankScoreKind::Keyword,
        signals: SearchSignals::default(),
        source,
        title: None,
        snippet: None,
    }
}

#[test]
fn repeated_leg_fills_absent_metadata_and_retains_present_empty_text() {
    for policy in [SignalMerge::FirstPresent, SignalMerge::Maximum] {
        let mut metadata = HashMap::new();
        merge_hit_metadata(&mut metadata, hit(1, SearchSource::Text), policy);
        let mut second = hit(1, SearchSource::Text);
        second.title = Some(String::new());
        second.snippet = Some("first snippet".into());
        second.signals.keyword_score = Some(DeterministicScore::ZERO);
        merge_hit_metadata(&mut metadata, second, policy);
        let mut later = hit(1, SearchSource::Text);
        later.title = Some("later title".into());
        later.snippet = Some("later snippet".into());
        later.score = DeterministicScore::from_raw(99);
        later.rank_score_kind = RankScoreKind::Union;
        merge_hit_metadata(&mut metadata, later, policy);
        let merged = &metadata[&Uuid::from_u128(1)];
        assert_eq!(merged.source, SearchSource::Text);
        assert_eq!(merged.title.as_deref(), Some(""));
        assert_eq!(merged.snippet.as_deref(), Some("first snippet"));
        assert_eq!(merged.signals.keyword_score, Some(DeterministicScore::ZERO));
        assert_eq!(merged.signals.vector_similarity, None);
        assert_eq!(merged.score, DeterministicScore::from_raw(17));
        assert_eq!(merged.rank_score_kind, RankScoreKind::Keyword);
    }
}

#[test]
fn signal_policies_keep_first_present_or_per_leg_maximum() {
    for (policy, keyword, vector) in [
        (SignalMerge::FirstPresent, 0, -3),
        (SignalMerge::Maximum, 9, 7),
    ] {
        let mut metadata = HashMap::new();
        for (source, keyword, vector) in [
            (SearchSource::Text, Some(0), None),
            (SearchSource::Text, Some(9), None),
            (SearchSource::Vector, None, Some(-3)),
            (SearchSource::Vector, None, Some(7)),
        ] {
            let mut incoming = hit(1, source);
            incoming.signals = SearchSignals {
                keyword_score: keyword.map(DeterministicScore::from_raw),
                vector_similarity: vector.map(DeterministicScore::from_raw),
            };
            merge_hit_metadata(&mut metadata, incoming, policy);
        }
        merge_hit_metadata(&mut metadata, hit(2, SearchSource::Vector), policy);
        assert_eq!(metadata.len(), 2);
        let merged = &metadata[&Uuid::from_u128(1)];
        assert_eq!(merged.source, SearchSource::Both);
        assert_eq!(
            merged.signals.keyword_score,
            Some(DeterministicScore::from_raw(keyword))
        );
        assert_eq!(
            merged.signals.vector_similarity,
            Some(DeterministicScore::from_raw(vector))
        );
        let separate = &metadata[&Uuid::from_u128(2)];
        assert_eq!(separate.source, SearchSource::Vector);
        assert_eq!(separate.signals, SearchSignals::default());
    }
}

#[test]
fn built_in_rank_kinds_and_custom_executor_arguments_are_preserved() {
    for (strategy, expected) in [
        (FusionStrategy::Rrf { k: 10 }, RankScoreKind::Rrf),
        (FusionStrategy::VectorOnly, RankScoreKind::Vector),
        (FusionStrategy::KeywordOnly, RankScoreKind::Keyword),
        (
            FusionStrategy::weighted(vec![0.7, 0.3]),
            RankScoreKind::Weighted,
        ),
        (FusionStrategy::Union, RankScoreKind::Union),
    ] {
        assert_eq!(RankScoreKind::of(&strategy), Ok(expected));
    }
    let strategy =
        FusionStrategy::try_custom("external".into(), serde_json::json!({"key": [1, 2]})).unwrap();
    let (name, params) = RankScoreKind::of(&strategy).unwrap_err();
    assert_eq!(name, "external");
    let FusionStrategy::Custom {
        params: original, ..
    } = &strategy
    else {
        unreachable!()
    };
    assert!(std::ptr::eq(params, original));
    assert_eq!(params, &serde_json::json!({"key": [1, 2]}));
    assert_eq!(
        RankScoreKind::of(&strategy).unwrap_or(RankScoreKind::Rrf),
        RankScoreKind::Rrf
    );
}
